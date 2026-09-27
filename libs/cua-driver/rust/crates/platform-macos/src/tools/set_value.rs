//! set_value tool — matches the Swift reference in SetValueTool.swift.
//!
//! The route is determined by the element:
//!
//! * **AXPopUpButton**: Choose the option whose title is exactly `value` from
//!   the popup's `AXMenu`, opening a closed menu with the control's own
//!   `AXPress`/`AXShowMenu`, press it, and read the popup's `AXValue` back.
//!   A menu this call opened is closed again when the choice fails. A Safari
//!   `<select>` whose menu publishes no options goes through
//!   `osascript do JavaScript` instead.
//!
//! * **Date and time controls** (the value is a `CFDate`): parse the ISO-8601
//!   `value` (`crate::ax::date`) and write it as a `CFDate`; the keystroke
//!   rung is only the fallback for a control that refuses that write.
//!
//! * **Native single-line text roles**: Replace the text through the keystroke
//!   rung. An `AXValue` write neither starts nor ends an editing session, so a
//!   control whose value is a binding target keeps its own value while every
//!   read-back the driver can perform returns the written string.
//!
//! * **Everything else**: Write `AXValue` directly (sliders, steppers, search
//!   fields whose `AXConfirm` is the commit, web inputs). An element that
//!   publishes no `AXValue`, or reports it read-only, is refused with
//!   `value_not_settable` before anything is written.

use async_trait::async_trait;
use cua_driver_contract::ActionCommit;
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEscalation, ActionEvidence, ActionExecutionRecord, ActionTransport,
        ActualDelivery, EscalationKind, EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_action_names, copy_bool_attr, copy_children, copy_date_attr, copy_element_attr,
    copy_label_attr, copy_number_attr, copy_string_attr, kAXErrorAttributeUnsupported,
    kAXErrorSuccess, local_iso8601_for_absolute_time, perform_action, release_all, set_date_attr,
    set_number_attr, set_range_attr, set_string_attr, try_copy_string_attr,
    try_is_attribute_settable, AXUIElementRef,
};
use crate::ax::popup::{choose_option, copy_open_menu, copy_popup_options, OptionChoice};
use crate::ax::RetainedElement;
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::{CFEqual, CFTypeRef};

use super::delivery_probe::WindowAnswer;
use super::ToolState;

pub struct SetValueTool {
    state: Arc<ToolState>,
}

impl SetValueTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "set_value".into(),
        description:
            "Set a value on a UI element. The route depends on the element:\n\
             \n\
             - **AXPopUpButton / select dropdown**: chooses the option whose title \
             is exactly `value` (an untitled web option goes by its value). A closed \
             menu is opened with the control's own AXPress/AXShowMenu, the option is \
             pressed, and the popup's value is read back; a menu this call opened is \
             closed again when the choice fails. No match, or several options with \
             that title, is refused with the option list. A Safari <select> whose \
             menu publishes no options is set through the page's DOM.\n\
             \n\
             - **Date and time controls** (the value is a CFDate, which the tree \
             renders as ISO-8601): `value` is ISO-8601. YYYY-MM-DD changes the day \
             and keeps the control's time of day; YYYY-MM-DDTHH:MM[:SS] is local \
             time; append Z or ±HH:MM for an exact instant. Written as a CFDate and \
             read back; typed only when the control refuses that write.\n\
             \n\
             - **All other elements**: writes AXValue directly (sliders, steppers, \
             native text fields that expose settable AXValue). An element that \
             publishes no AXValue, or reports it read-only — a button, a collapsed \
             toolbar search item — is refused with `value_not_settable` before \
             anything is written; the refusal names the element's own route \
             (press a collapsed search item to expand its field, then set that).\n\
             \n\
             For free-form text entry into web inputs, prefer `type_text_chars` \
             which synthesises key events — AXValue writes are ignored by WebKit.\n\
             \n\
             Commits the write on text roles: after a successful AXValue write \
             the element's own AXConfirm action runs, or — when it advertises \
             none — the element is focused and a tab/return keystroke ends the \
             edit, so the app's editing pipeline sees the value instead of only \
             the AX tree. `committed` reports whether the value survived that \
             gesture; false means unproven, and the app may still hold its \
             pre-edit value. Web content is never committed this way (the \
             renderer never observes an AXValue write), and a multi-line \
             AXTextArea has no end-of-edit gesture. A search field's AXConfirm \
             asks the app to run the search, and an app may take it without \
             searching: it counts as the commit only when the window changes \
             after it, and is `unproven` otherwise — type the query into the \
             field when the results did not change.\n\
             \n\
             A committed edit is STILL NOT ON DISK: measured on TextEdit, an \
             AXTextArea write appeared in the AX tree while the document stayed \
             unmarked, with no undo entry and a byte-identical file. Read \
             `document_edited` from get_window_state to see whether the app \
             registered the edit, and make it durable with the app's own save \
             action (`press_key` cmd+s, or `invoke_menu` File > Save)."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "value"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid": { "type": "integer" },
                "window_id": {
                    "type": "integer",
                    "description": "CGWindowID for the window whose get_window_state produced the element_index. Required when element_index is used; optional when element_token is supplied (the token carries it)."
                },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "value": {
                    "type": "string",
                    "description": "New value: a popup's option by its exact title; an ISO-8601 date or date-time for a date control; otherwise the text or number to write."
                },
                "detect_window_change": { "type": "boolean", "description": "Default true: after the action the driver polls WindowServer for up to one second so the reply can name a window the action opened. Pass false when you enumerate windows yourself — the poll is then skipped (roughly a second off this call) and the reply carries no opened-window evidence." },
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  true,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for SetValueTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let value = match args.require_str("value") {
            Ok(v) => v,
            Err(e) => return e,
        };

        // Surface 6: element_token / element_index precedence. Neither
        // is now schema-required so the resolver can centralize the
        // "missing addressing" error message.
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id");
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match self.state.element_cache.resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "set_value",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None => {
                return ToolResult::error(
                    "set_value requires element_index (+ window_id) or element_token to \
                     address the target element.",
                )
            }
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: Some(wid),
                element_index: idx,
                element,
                ..
            } => match u32::try_from(wid) {
                Ok(wid) => (idx, wid, element),
                Err(_) => return ToolResult::error("window_id is out of range for macOS."),
            },
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: None, ..
            } => {
                return ToolResult::error(
                    "set_value requires window_id when element_index is used \
                 (omit only when supplying element_token, which carries it).",
                )
            }
        };

        let element_ptr = element_guard.as_ptr();

        // set_value is an always-background semantic AX mutation. Re-prove
        // that the retained element still belongs to the requested exact
        // window immediately before any cursor or AX work; a cache hit alone
        // is not delivery proof after a window lifecycle or Space change.
        let _mutation_lease = match super::gate_background_window_action(
            pid,
            window_id,
            Some(element_ptr),
            cua_driver_core::background_input::BackgroundAction::AxSemantic,
        )
        .await
        {
            Ok(lease) => lease,
            Err(refusal_result) => return refusal_result,
        };

        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
        let center_guard = element_guard.clone();
        if let Ok(Some((screen_x, screen_y))) =
            cua_driver_core::operation::spawn_blocking(move || unsafe {
                crate::ax::bindings::element_screen_center(center_guard.as_ptr() as AXUIElementRef)
            })
            .await
        {
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::PinAbove(window_id as u64),
            );
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y).await;
            self.state
                .cursor_registry
                .update_position(&cursor_key, screen_x, screen_y);
        }
        // An AXValue read-back is not ground truth for web content. Chromium,
        // WebKit, and Electron can echo the write through accessibility while
        // the renderer never observes it. Reuse type_text's bounded ancestor
        // check so native browser chrome stays trusted but rendered content is
        // always reported as unverified.
        let ax_echo_surface = super::type_text::target_in_web_area(
            pid,
            Some((element_ptr, Some(element_index))),
            Some(window_id),
        );

        let plan = match resolve_write_plan(
            &_mutation_lease,
            pid,
            window_id,
            element_ptr,
            &value,
            ax_echo_surface,
        )
        .await
        {
            Ok(plan) => plan,
            Err(not_settable) => return not_settable.result(),
        };

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // AXValue writes on popups / sliders can cause reflex activations
        // in Chromium-based apps; the AXPopUpButton path also AXPresses a
        // child option which can trigger app activation in some setups.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "set_value.AXValue",
            || async move {
                cua_driver_core::operation::spawn_blocking(move || {
                    set_value_blocking(
                        element_guard.as_ptr(),
                        element_index,
                        pid,
                        window_id,
                        &value,
                        plan,
                        ax_echo_surface,
                    )
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot, &args).await;

        match result {
            Ok(Ok(mut outcome)) => {
                apply_surface_trust(&mut outcome, ax_echo_surface);
                apply_verification_label(&mut outcome);
                let mut msg = std::mem::take(&mut outcome.detail);
                msg.push_str(&changes.result_suffix());
                // The record is built here; the structured payload only
                // carries the appeared windows core adopts into it.
                let mut structured = serde_json::json!({});
                changes.publish_gained_windows(&mut structured);
                ToolResult::text(msg)
                    .with_structured(structured)
                    .with_action_record(action_record(&outcome, ax_echo_surface))
            }
            Ok(Err(e)) => ToolResult::error(format!("set_value failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// The action contract for a value write. `set_value` is always a background
/// semantic mutation; the value travels on `AXValue` or, for a bound
/// single-line control, on PID-routed keystrokes. Only a trusted read-back
/// confirms it, and a read-back from web content is never trusted: the
/// renderer may never have observed a write accessibility echoes back.
fn action_record(outcome: &SetValueOutcome, ax_echo_surface: bool) -> ActionExecutionRecord {
    let transport = if outcome.path == "key_events" {
        ActionTransport::MacosCgEventPid
    } else {
        ActionTransport::MacosAxValue
    };
    let verified = outcome.verified == Some(true);
    let mut record = ActionExecutionRecord::builder(
        if verified {
            ActionEffect::Confirmed
        } else {
            ActionEffect::Unverifiable
        },
        transport,
        RequestedDelivery::Background,
    )
    .actual_delivery(ActualDelivery::Background);
    if verified {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::ValueReadback,
            detail: "the control's AXValue read back as the requested value".into(),
        });
    }
    if let Some(committed) = outcome.committed {
        record = record.committed(committed);
    }
    if let Some(delivered) = outcome
        .delivered
        .and_then(|count| u32::try_from(count).ok())
    {
        record = record.delivered_count(delivered);
    }
    if ax_echo_surface {
        record = record.escalation(ActionEscalation {
            kind: EscalationKind::RetryWithPixelTarget,
            detail: Some(
                "AXValue read-back is not trusted for web content. Verify through the \
                 renderer; use browser page tools for a tab or manipulate the control \
                 through its pixel action."
                    .into(),
            ),
        });
    }
    record.build().expect("set_value record is valid")
}

/// How the written value reaches the app's own editing pipeline.
#[derive(Debug, PartialEq, Eq)]
enum WritePlan {
    /// Not a text control: the `AXValue` write is the whole edit.
    ValueOnly,
    /// `AXValue`, ended by the control's own advertised `AXConfirm`.
    ValueThenConfirm,
    /// `AXValue`, ended by a search field's advertised `AXConfirm`, which asks
    /// the application to run the search — and an application is free to take
    /// the action and not search. Font Book's field did exactly that: the
    /// value read back, the reply said committed, and the list stayed on all
    /// 362 typefaces (V3D fontbook, 4 of 4 runs), while Automator's library
    /// search filters on the same action. Only the window answering the
    /// action witnesses the search.
    ValueThenSearch,
    /// `AXValue`, ended by focusing the control and pressing this key.
    ValueThenKey(&'static str),
    /// Replace the control's text through the keystroke rung, then end the
    /// edit with this key.
    ///
    /// An `AXValue` write sets a control's string without starting or ending
    /// an editing session, so a plain `NSTextField` — a save sheet's name
    /// field, an action parameter — never hands the value to the binding
    /// behind it, while every read-back the driver can perform returns the
    /// written string. Keystrokes go through the field editor the binding
    /// reads on end-of-edit, which is the route a person uses.
    Retype(&'static str),
    /// No route exists, or one was refused before dispatch.
    Blocked(String),
    /// The control's value is a `CFDate`: write the parsed ISO-8601 value as
    /// one, and reach for the keystroke rung only when that write is refused.
    Date(DateFallback),
}

/// What a date control that refuses a `CFDate` write is left with, decided
/// before dispatch like every other keystroke route.
#[derive(Debug, PartialEq, Eq)]
enum DateFallback {
    /// Type the value through the keystroke rung, ending the edit with this key.
    Retype(&'static str),
    /// The keystroke gate refused the rung; this is its reason.
    Refused(String),
}

/// Settle time between the commit gesture and the read-back that judges it.
const COMMIT_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

/// Settle time between an `AXFocused` write and the read-back that proves it.
const FOCUS_SETTLE: std::time::Duration = std::time::Duration::from_millis(60);

/// Native single-line text roles whose value AppKit carries in a field editor.
/// `AXSearchField` is deliberately absent: its `AXConfirm` asks the app to run
/// the search, and the window answering it is judged instead
/// ([`WritePlan::ValueThenSearch`]).
pub(super) fn is_binding_target_role(role: &str, subrole: &str) -> bool {
    matches!(role, "AXTextField" | "AXSecureTextField") && subrole != "AXSearchField"
}

/// Whether keystrokes can carry this value into a single-line control. Tab
/// moves focus and Return ends the edit, so a value holding either can only
/// ever be written losslessly through `AXValue`.
fn is_typeable(value: &str) -> bool {
    !value.chars().any(char::is_control)
}

fn write_plan(role: &str, subrole: &str, advertised: &[String], value: &str) -> WritePlan {
    match role {
        "AXTextArea" => WritePlan::Blocked(
            "a multi-line AXTextArea has no end-of-edit gesture, so the app may never \
             register the write"
                .to_owned(),
        ),
        _ if is_binding_target_role(role, subrole) && is_typeable(value) => {
            WritePlan::Retype("tab")
        }
        "AXTextField" | "AXSecureTextField" | "AXSearchField" | "AXComboBox" | "AXDateField"
        | "AXTimeField" => {
            let search = role == "AXSearchField" || subrole == "AXSearchField";
            if advertised.iter().any(|action| action == "AXConfirm") {
                if search {
                    WritePlan::ValueThenSearch
                } else {
                    WritePlan::ValueThenConfirm
                }
            } else if search {
                WritePlan::ValueThenKey("return")
            } else {
                WritePlan::ValueThenKey("tab")
            }
        }
        _ => WritePlan::ValueOnly,
    }
}

/// Resolve the write route for the addressed element, re-gating the keystroke
/// rung before it is promised: a process-scoped key is a stricter route than
/// the semantic AX write this tool already holds a lease for. An element with
/// no value to write is refused here, before anything is dispatched.
async fn resolve_write_plan(
    lease: &super::BackgroundMutationLease,
    pid: i32,
    window_id: u32,
    element_ptr: usize,
    value: &str,
    ax_echo_surface: bool,
) -> Result<WritePlan, ValueNotSettable> {
    if ax_echo_surface {
        return Ok(WritePlan::Blocked(
            "web content never observes an AXValue write, so there is nothing to commit".to_owned(),
        ));
    }
    let Ok((role, subrole, label, advertised, holds_date, value_attribute)) =
        cua_driver_core::operation::spawn_blocking(move || unsafe {
            let element = element_ptr as AXUIElementRef;
            (
                copy_string_attr(element, "AXRole").unwrap_or_default(),
                copy_string_attr(element, "AXSubrole").unwrap_or_default(),
                copy_label_attr(element).unwrap_or_default(),
                copy_action_names(element),
                copy_date_attr(element, "AXValue").is_some(),
                ValueAttribute::read(element),
            )
        })
        .await
    else {
        return Ok(WritePlan::Blocked(
            "the element stopped answering AX reads before the write".to_owned(),
        ));
    };
    if let Some(value_attribute) =
        unwritable_value(&role, holds_date, value_attribute, &advertised, value)
    {
        return Err(ValueNotSettable {
            role,
            subrole,
            label,
            window_id,
            pid,
            advertised,
            value_attribute,
        });
    }
    let plan = if holds_date {
        WritePlan::Date(DateFallback::Retype("tab"))
    } else {
        write_plan(&role, &subrole, &advertised, value)
    };
    if matches!(
        plan,
        WritePlan::ValueThenKey(_) | WritePlan::Retype(_) | WritePlan::Date(_)
    ) {
        if let Err(refusal) = lease
            .gate_again(
                window_id,
                Some(element_ptr),
                cua_driver_core::background_input::BackgroundAction::GenericKey,
            )
            .await
        {
            if matches!(plan, WritePlan::Date(_)) {
                return Ok(WritePlan::Date(DateFallback::Refused(keystroke_refusal_reason(
                    &refusal,
                ))));
            }
            return Ok(refused_keystroke_plan(&plan, &advertised, &refusal));
        }
    }
    Ok(plan)
}

/// What the addressed element answered about its `AXValue` before the write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueAttribute {
    /// The element reports the attribute settable.
    Settable,
    /// The element publishes the attribute and reports it read-only.
    ReadOnly,
    /// The element does not publish the attribute at all.
    Absent,
    /// The element did not answer either question; nothing is known.
    Unknown,
}

impl ValueAttribute {
    /// # Safety
    ///
    /// `element` must be a valid, live `AXUIElementRef` for the call.
    unsafe fn read(element: AXUIElementRef) -> Self {
        if try_copy_string_attr(element, "AXValue") == Err(kAXErrorAttributeUnsupported) {
            return Self::Absent;
        }
        match try_is_attribute_settable(element, "AXValue") {
            Ok(true) => Self::Settable,
            Ok(false) => Self::ReadOnly,
            Err(err) if err == kAXErrorAttributeUnsupported => Self::Absent,
            Err(_) => Self::Unknown,
        }
    }
}

/// Why `set_value` has nothing to write on this element, or `None` when a
/// route exists.
///
/// Measured on Activity Monitor's collapsed toolbar search (an `AXButton`
/// with subrole `AXSearchField`): `AXValue` was written to the button, the
/// application answered success, nothing read it back, and the list stayed
/// unfiltered (V3D native-read-activity-processes, 4 of 4 runs). A value the
/// element does not publish, or publishes read-only, has no write route
/// whatever the application answers. Three routes do not write a string into
/// `AXValue` and keep their own refusals: a popup's option choice, a date
/// control's `CFDate`, and stepping a numeric control through its advertised
/// increment/decrement actions (SwiftUI's `AXSlider` publishes its value
/// read-only and steps).
fn unwritable_value(
    role: &str,
    holds_date: bool,
    value_attribute: ValueAttribute,
    advertised: &[String],
    value: &str,
) -> Option<ValueAttribute> {
    if !matches!(value_attribute, ValueAttribute::ReadOnly | ValueAttribute::Absent)
        || role == "AXPopUpButton"
        || holds_date
    {
        return None;
    }
    let steps = advertised
        .iter()
        .any(|action| action == "AXIncrement" || action == "AXDecrement");
    if steps && value.trim().parse::<f64>().is_ok() {
        return None;
    }
    Some(value_attribute)
}

/// The refusal for an element whose value cannot be written: nothing was
/// dispatched, and the reason names the element's own route when it has one.
#[derive(Debug)]
struct ValueNotSettable {
    role: String,
    subrole: String,
    label: String,
    window_id: u32,
    pid: i32,
    advertised: Vec<String>,
    value_attribute: ValueAttribute,
}

impl ValueNotSettable {
    fn reason(&self) -> String {
        let Self {
            role,
            subrole,
            label,
            window_id,
            ..
        } = self;
        let control = match (subrole.is_empty(), label.is_empty()) {
            (true, true) => role.clone(),
            (true, false) => format!("{role} \"{label}\""),
            (false, true) => format!("{role}/{subrole}"),
            (false, false) => format!("{role}/{subrole} \"{label}\""),
        };
        let state = match self.value_attribute {
            ValueAttribute::Absent => "publishes no AXValue, so it has no value to write",
            _ => "reports its AXValue read-only",
        };
        let presses = self.advertised.iter().any(|action| action == "AXPress");
        let route = if presses && subrole == "AXSearchField" {
            "It is a collapsed search control that advertises AXPress: press it to expand the \
             search field, observe the window again, and set the field it reveals."
        } else if presses {
            "It advertises AXPress: press it if that is the change you meant; set_value only \
             writes a control whose value is settable."
        } else {
            "It advertises no action that edits a value: address the control that holds the value."
        };
        format!("set_value was not dispatched: {control} of window {window_id} {state}. {route}")
    }

    fn payload(&self) -> Value {
        let mut payload = serde_json::json!({
            "code": "value_not_settable",
            "effect": "not_dispatched",
            "route": "ax",
            "action": "set_value",
            "role": self.role,
            "label": self.label,
            "window_id": self.window_id,
            "pid": self.pid,
            "value_attribute": match self.value_attribute {
                ValueAttribute::Absent => "absent",
                _ => "read_only",
            },
            "advertised_actions": self.advertised,
        });
        if !self.subrole.is_empty() {
            payload["subrole"] = serde_json::json!(self.subrole);
        }
        payload
    }

    fn result(&self) -> ToolResult {
        ToolResult::error(self.reason()).with_structured(self.payload())
    }
}

/// What is left when the keystroke rung is refused before dispatch.
///
/// The gate refuses process-scoped keys whenever the application owns another
/// eligible top-level window, and an open save panel is exactly that: its own
/// top-level window beside the document. `set_value` has no `delivery_mode`,
/// so the refusal is final for this call. An advertised `AXConfirm` is then
/// the only end-of-edit left, and on a save panel it is the panel's own
/// confirm — measured on Automator: the write is saved to disk under the
/// written name. Keeping the `AXValue` route in that case preserves a control
/// the typed route cannot reach; the verdict stays `unproven`, because a
/// read-back is all the evidence there is.
fn refused_keystroke_plan(
    plan: &WritePlan,
    advertised: &[String],
    refusal: &ToolResult,
) -> WritePlan {
    let confirms = advertised.iter().any(|action| action == "AXConfirm");
    if matches!(plan, WritePlan::Retype(_)) && confirms {
        return WritePlan::ValueThenConfirm;
    }
    WritePlan::Blocked(keystroke_refusal_reason(refusal))
}

fn keystroke_refusal_reason(refusal: &ToolResult) -> String {
    let detail = refusal
        .structured_content
        .as_ref()
        .and_then(|structured| structured.get("reason"))
        .and_then(serde_json::Value::as_str);
    match detail {
        Some(detail) => format!("the commit keystroke was refused before dispatch — {detail}"),
        None => "the commit keystroke was refused before dispatch".to_owned(),
    }
}

// ── Following a control across its app's end-of-edit ────────────────────────

/// How to find a control again after its application rebuilds it.
///
/// An end-of-edit is handed to the application, and an application that reacts
/// by re-creating the control does so with a *new* `AXUIElementRef`: every read
/// on the pointer this call addressed then answers `kAXErrorInvalidUIElement`,
/// which is indistinguishable from "the value is gone" unless the control is
/// looked up again. What identifies it across the rebuild is its place in its
/// parent — same role, same label, same ordinal among the parent's children
/// that share both. The value is deliberately not part of that: this call is
/// what just changed it.
struct ElementIdentity {
    parent: RetainedElement,
    role: String,
    label: Option<String>,
    ordinal: usize,
}

/// The parts of a control's description an edit does not change.
///
/// `copy_label_attr` falls back to `AXValue`, which is exactly what a write
/// rewrites, so the label is read only from the attributes AppKit keeps across
/// the edit: the title, the description, the placeholder.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the
/// call.
unsafe fn stable_label(element: AXUIElementRef) -> Option<String> {
    ["AXTitle", "AXDescription", "AXPlaceholderValue"]
        .into_iter()
        .find_map(|attr| copy_string_attr(element, attr).filter(|label| !label.is_empty()))
}

/// Whether `candidate` is a control of the same role and stable label.
///
/// # Safety
///
/// `candidate` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn same_control(candidate: AXUIElementRef, role: &str, label: Option<&str>) -> bool {
    copy_string_attr(candidate, "AXRole").as_deref() == Some(role)
        && stable_label(candidate).as_deref() == label
}

/// Record where the addressed control sits, before a gesture that may rebuild
/// it. `None` when the control has no parent to be found in again.
fn capture_identity(element: AXUIElementRef, role: &str) -> Option<ElementIdentity> {
    let parent = unsafe { RetainedElement::adopt(copy_element_attr(element, "AXParent")?)? };
    let label = unsafe { stable_label(element) };
    let children = unsafe { copy_children(parent.as_ptr() as AXUIElementRef) };
    let ordinal = children
        .iter()
        .filter(|&&sibling| unsafe { same_control(sibling, role, label.as_deref()) })
        .position(|&sibling| unsafe { CFEqual(sibling as CFTypeRef, element as CFTypeRef) } != 0);
    unsafe { release_all(children) };
    Some(ElementIdentity {
        parent,
        role: role.to_owned(),
        label,
        ordinal: ordinal?,
    })
}

/// Find the control an identity describes among its parent's current children.
fn resolve_identity(identity: &ElementIdentity) -> Option<RetainedElement> {
    let children = unsafe { copy_children(identity.parent.as_ptr() as AXUIElementRef) };
    let mut matched = None;
    let mut seen = 0_usize;
    for &child in &children {
        if unsafe { same_control(child, &identity.role, identity.label.as_deref()) } {
            if seen == identity.ordinal {
                matched = Some(unsafe { RetainedElement::retain(child as usize) });
                break;
            }
            seen += 1;
        }
    }
    unsafe { release_all(children) };
    matched
}

/// Read the control's value after a commit gesture, following it across a
/// rebuild: when the addressed pointer stops answering, the control's identity
/// is resolved again and the replacement is read instead. The replacement is
/// returned with the value, because a pointer that had to be replaced is itself
/// evidence the edit session it belonged to is over.
fn read_back_after_commit(
    element: AXUIElementRef,
    identity: Option<&ElementIdentity>,
) -> (Option<String>, Option<RetainedElement>) {
    if let Some(value) = unsafe { copy_string_attr(element, "AXValue") } {
        return (Some(value), None);
    }
    let Some(resolved) = identity.and_then(resolve_identity) else {
        return (None, None);
    };
    let value = unsafe { copy_string_attr(resolved.as_ptr() as AXUIElementRef, "AXValue") };
    (value, Some(resolved))
}

/// What a commit gesture is judged against: the value asked for, the value the
/// control held before the write, and how to find the control again if its app
/// rebuilds it on end-of-edit.
struct CommitTarget<'a> {
    requested: &'a str,
    before: Option<&'a str>,
    numeric: bool,
    identity: Option<&'a ElementIdentity>,
}

impl CommitTarget<'_> {
    /// Judge a commit gesture by reading the control back through it.
    fn judge(
        &self,
        element: AXUIElementRef,
        dispatched: bool,
        witness: CommitWitness,
        gesture: &str,
    ) -> CommitJudgement {
        if !dispatched {
            return CommitJudgement::of(
                judge_commit(CommitReadback::NotDispatched, witness, gesture),
                None,
            );
        }
        std::thread::sleep(COMMIT_SETTLE);
        let (settled, _resolved) = read_back_after_commit(element, self.identity);
        let verdict = judge_commit(
            classify_readback(settled.as_deref(), self.before, self.requested, self.numeric),
            witness,
            gesture,
        );
        CommitJudgement::of(verdict, settled)
    }
}

/// What a commit gesture produced: the verdict, the sentence that reports it,
/// and the value the judging read-back saw — `None` when no gesture ran or
/// nothing answered, so a caller can report the control's settled state rather
/// than the one it read before the gesture.
struct CommitJudgement {
    committed: Option<ActionCommit>,
    detail: String,
    settled: Option<String>,
}

impl CommitJudgement {
    fn of(verdict: (Option<ActionCommit>, String), settled: Option<String>) -> Self {
        Self {
            committed: verdict.0,
            detail: verdict.1,
            settled,
        }
    }
}

/// Run the commit gesture for an `AXValue` write and report what was observed
/// of the app's end-of-edit.
fn commit_written_value(
    element: AXUIElementRef,
    pid: i32,
    window_id: u32,
    plan: WritePlan,
    changed: Option<bool>,
    target: &CommitTarget<'_>,
) -> CommitJudgement {
    match plan {
        WritePlan::ValueOnly => CommitJudgement::of((None, String::new()), None),
        WritePlan::Blocked(reason) => CommitJudgement::of(
            (
                Some(ActionCommit::NotCommitted),
                format!(" Not committed: {reason}."),
            ),
            None,
        ),
        WritePlan::Retype(_) => unreachable!("the retype route does not write AXValue"),
        WritePlan::Date(_) => unreachable!("the date route writes a CFDate, never a string"),
        WritePlan::ValueThenConfirm => {
            let dispatched = unsafe { perform_action(element, "AXConfirm") } == kAXErrorSuccess;
            // A value the control already held survives any gesture, so only a
            // write that moved it can be witnessed by the control's own action.
            let witness = if changed == Some(true) {
                CommitWitness::OwnConfirmAction
            } else {
                CommitWitness::ReadbackOnly
            };
            target.judge(element, dispatched, witness, "AXConfirm")
        }
        WritePlan::ValueThenSearch => {
            // Settled after the write, so the field's own echo of it is in
            // the baseline and only what the app does next can differ.
            let watch = (changed == Some(true))
                .then(|| super::delivery_probe::WindowWatch::capture(pid, window_id));
            let dispatched = unsafe { perform_action(element, "AXConfirm") } == kAXErrorSuccess;
            // A value the control already held proves nothing about this call,
            // whatever the window does.
            let witness = match watch {
                Some(watch) if dispatched => CommitWitness::SearchWindow(watch.answer()),
                _ => CommitWitness::ReadbackOnly,
            };
            target.judge(element, dispatched, witness, "AXConfirm")
        }
        WritePlan::ValueThenKey(key) => {
            let focused = crate::input::ax_actions::focus_element(element as usize).is_ok();
            let dispatched = focused && crate::input::keyboard::press_key(pid, key, &[]).is_ok();
            // The key ends the edit session, but the value in it arrived
            // through `AXValue` rather than the field editor, and a synthesized
            // key is not the control's own advertised commit, so the ended
            // session is not evidence the app took it.
            target.judge(element, dispatched, CommitWitness::ReadbackOnly, key)
        }
    }
}

/// What was observed of the app's end-of-edit beyond the value surviving the
/// commit gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitWitness {
    /// A *typed* edit session ended with the value in place — what an AppKit
    /// binding commits on, observed as the control losing keyboard focus after
    /// a gesture that had established it.
    TypedEditEnded,
    /// The control's own advertised confirm action ran, over a value this call
    /// provably moved the field to. `AXConfirm` is the control's action — what
    /// Return invokes — so its handler running IS the app's end-of-edit.
    OwnConfirmAction,
    /// Nothing but an accessibility read-back.
    ReadbackOnly,
    /// A search field's confirm ran over a value this call moved it to, and
    /// this is what the rest of its window did afterwards.
    SearchWindow(super::delivery_probe::WindowAnswer),
}

/// What the read-back after a commit gesture saw.
///
/// Only one of these refutes a commit. A control that reads back the value it
/// held *before* the write is a control whose application discarded the edit
/// and put its own value back — the one thing "not committed" claims. A control
/// that reads back something else took the edit and rewrote it: an end-of-edit
/// formatter is ordinary AppKit, so a phone number comes back as
/// `(555) 789-0123` and a date in the app's own spelling. A control that stops
/// answering has been re-created by its application. Neither of those is
/// evidence the application kept its own value, so neither may say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitReadback<'a> {
    /// The commit gesture could not be dispatched.
    NotDispatched,
    /// The read-back is the value this call asked for.
    Matches,
    /// The read-back is the value the control held before the write.
    Restored,
    /// The read-back is neither, quoted as the control reports it.
    Rewritten(&'a str),
    /// Nothing answered after the gesture, though the control published a value
    /// before it: the control this pointer addressed is gone and its identity
    /// resolved to nothing readable either.
    Unreadable,
    /// The control publishes no readable value at all, before or after.
    Unpublished,
}

/// Compare a post-gesture read-back against the request and the control's prior
/// value.
///
/// Both comparisons stay exact — `value_equal` normalises nothing but a numeric
/// control's own formatting. Folding digits, whitespace or bidi isolates into a
/// match would report the *application's* value as the caller's, which is the
/// one thing the caller cannot check for itself.
fn classify_readback<'a>(
    after: Option<&'a str>,
    before: Option<&str>,
    requested: &str,
    numeric: bool,
) -> CommitReadback<'a> {
    let Some(after) = after else {
        return if before.is_some() {
            CommitReadback::Unreadable
        } else {
            CommitReadback::Unpublished
        };
    };
    if value_equal(after, requested, numeric) {
        return CommitReadback::Matches;
    }
    match before {
        Some(before) if value_equal(after, before, numeric) => CommitReadback::Restored,
        _ => CommitReadback::Rewritten(after),
    }
}

/// What the driver may claim about the app's end-of-edit.
///
/// An accessibility read-back is echoed by a bound control whether or not the
/// application took the value, so on its own it can only ever refute a commit.
/// A positive verdict needs a second observation, and there are two:
///
/// A *typed* edit session ending is measured, not assumed. On Automator's
/// "Save as:" action parameter (2026-09-14): an `AXValue` write, a proven
/// `AXFocused` write and a Tab that demonstrably moved focus left the written
/// string in the control's read-back and the application still kept its own
/// value in `document.wflow`. An ended edit session over a value the field
/// editor never saw proves nothing.
///
/// The control's own `AXConfirm` is the other. It is selected only for a
/// control that advertises it, and performing it runs the same action handler
/// Return does. Paired with a read-back that moved off the field's prior value
/// — so the survival is not an echo of a value the field already held — that
/// is the application's end-of-edit, observed.
///
/// Refuting a commit is narrower than it looks: the sentence claims the
/// application put its own value back, and only [`CommitReadback::Restored`]
/// shows that.
fn judge_commit(
    readback: CommitReadback<'_>,
    witness: CommitWitness,
    gesture: &str,
) -> (Option<ActionCommit>, String) {
    match readback {
        CommitReadback::NotDispatched => (
            Some(ActionCommit::NotCommitted),
            format!(" Not committed: the {gesture} commit gesture could not be dispatched."),
        ),
        CommitReadback::Restored => (
            Some(ActionCommit::NotCommitted),
            " Not committed: the value did not survive the app's end-of-edit, so the app \
             still holds its own value."
                .to_owned(),
        ),
        CommitReadback::Rewritten(value) => (
            Some(ActionCommit::Unproven),
            format!(
                " Commit unproven: the app's end-of-edit rewrote the value — it reads back as \
                 \"{value}\", which is neither what was written nor the value the control held \
                 before. Judge it by the app's own output."
            ),
        ),
        CommitReadback::Unreadable => (
            Some(ActionCommit::Unproven),
            " Commit unproven: the control was re-created on end-of-edit; read it back."
                .to_owned(),
        ),
        CommitReadback::Unpublished => (
            Some(ActionCommit::Unproven),
            format!(
                " Commit unproven: the control publishes no readable value, so nothing could \
                 read back what {gesture} left in it."
            ),
        ),
        CommitReadback::Matches => match witness {
            CommitWitness::TypedEditEnded => (
                Some(ActionCommit::Committed),
                format!(
                    " Committed via {gesture}: the typed edit session ended with this value in \
                     place."
                ),
            ),
            CommitWitness::OwnConfirmAction => (
                Some(ActionCommit::Committed),
                format!(
                    " Committed via {gesture}: the control's own confirm action ran and the \
                     value it changed to survived it."
                ),
            ),
            CommitWitness::ReadbackOnly => (
                Some(ActionCommit::Unproven),
                format!(
                    " Commit unproven: the value survived {gesture}, but the app's own model \
                     was not observed — an accessibility read-back is echoed by a bound control \
                     whether or not the app took the value. Check the app's own output."
                ),
            ),
            CommitWitness::SearchWindow(WindowAnswer::Changed) => (
                Some(ActionCommit::Committed),
                format!(
                    " Committed via {gesture}: the search field's own confirm ran over this \
                     value and the window changed after it."
                ),
            ),
            CommitWitness::SearchWindow(WindowAnswer::Unchanged { waited }) => (
                Some(ActionCommit::Unproven),
                format!(
                    " Commit unproven: the search field holds this value and its {gesture} ran, \
                     but nothing else in the window changed within {} ms — an app can take a \
                     search field's confirm and search only on typed input. Judge by its \
                     results; if they did not change, type the query into the field.",
                    waited.as_millis()
                ),
            ),
            CommitWitness::SearchWindow(WindowAnswer::Unwatched) => (
                Some(ActionCommit::Unproven),
                format!(
                    " Commit unproven: the search field holds this value and its {gesture} ran, \
                     but the window could not be watched for the search (it never held still, \
                     or did not answer). Judge by the app's own results."
                ),
            ),
        },
    }
}

// ── Blocking implementation (runs on spawn_blocking thread) ─────────────────

/// Outcome of a `set_value` write.
///
/// `verified` is `None` for paths that do not perform a value read-back (the
/// AXPopUpButton path drives menu items rather than writing AXValue), and
/// `Some(false)` when a read-back ran but could not confirm the write. A
/// successful `AXUIElementSetAttributeValue` return code is not by itself
/// evidence that the value landed: web content behind an AXWebArea accepts the
/// write and echoes it back through AXValue while the renderer never observes
/// it — the same trap `type_text` already documents.
struct SetValueOutcome {
    detail: String,
    verified: Option<bool>,
    /// `Some(false)` when the element already held the requested value, so the
    /// write was a no-op. Lets callers distinguish "idempotent" from "applied".
    changed: Option<bool>,
    /// What was observed of the app's own end-of-edit. `None` for paths that
    /// have no commit gesture (menu selection, non-text controls).
    committed: Option<ActionCommit>,
    /// The transport the value actually travelled on, published as
    /// `structured.path`.
    path: &'static str,
    /// Characters the keystroke rung proved delivered, for the retype route.
    delivered: Option<usize>,
}

fn apply_surface_trust(outcome: &mut SetValueOutcome, ax_echo_surface: bool) {
    if ax_echo_surface && outcome.verified == Some(true) {
        outcome.verified = Some(false);
        outcome.changed = None;
        outcome.detail.push_str(
            " AXValue read-back is not trusted for web content; verify the \
             renderer via screenshot or use the browser page tools.",
        );
    }
}

fn apply_verification_label(outcome: &mut SetValueOutcome) {
    if outcome.verified != Some(true) {
        for prefix in ["✅ Set", "✅ Typed", "✅ Cleared"] {
            if let Some(rest) = outcome.detail.strip_prefix(prefix) {
                outcome.detail = format!("📨 Sent (unverified){rest}");
                return;
            }
        }
    }
}

fn set_value_blocking(
    element_ptr: usize,
    element_index: usize,
    pid: i32,
    window_id: u32,
    value: &str,
    plan: WritePlan,
    web_content: bool,
) -> anyhow::Result<SetValueOutcome> {
    let element = element_ptr as AXUIElementRef;

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();

    if role == "AXPopUpButton" {
        select_popup_option(element, element_index, pid, value, web_content)
    } else if let WritePlan::Date(fallback) = plan {
        write_date_blocking(
            element,
            element_ptr,
            element_index,
            pid,
            window_id,
            value,
            fallback,
            &role,
        )
    } else if let WritePlan::Retype(end) = plan {
        retype_blocking(
            element,
            element_ptr,
            element_index,
            pid,
            window_id,
            value,
            end,
            &role,
        )
    } else {
        // Default path: write AXValue directly. Numeric controls (AXSlider /
        // AXStepper) reject a CFString with -25201 and need a CFNumber; text
        // fields take a CFString. Try numeric first when the value parses as a
        // number, then fall back to a string write.
        // Numeric target carried through so we can step toward it if the
        // direct writes are rejected (SwiftUI AXSlider rejects every AXValue
        // write with -25200 yet exposes a readable AXValue + increment/decrement
        // actions).
        let numeric_target = value.trim().parse::<f64>().ok();
        // Read the value before writing so an unchanged field can be reported as
        // idempotent rather than silently indistinguishable from a fresh write.
        let before = unsafe { copy_string_attr(element, "AXValue") };
        // Taken while the element still answers: an app that re-creates the
        // control on its end-of-edit leaves this pointer invalid, and the
        // identity is how the read-back finds what the app put in its place.
        let identity = capture_identity(element, &role);
        let err = match numeric_target {
            Some(n) => {
                let e = unsafe { set_number_attr(element, "AXValue", n) };
                if e == kAXErrorSuccess {
                    e
                } else {
                    unsafe { set_string_attr(element, "AXValue", value) }
                }
            }
            None => unsafe { set_string_attr(element, "AXValue", value) },
        };
        if err == kAXErrorSuccess {
            let written = unsafe { copy_string_attr(element, "AXValue") };
            // The witness needs the move the *write* made: a gesture over a
            // value the control already held proves nothing about the app.
            let (_, written_change) = classify_write(
                before.as_deref(),
                written.as_deref(),
                value,
                numeric_target.is_some(),
            );
            let target = CommitTarget {
                requested: value,
                before: before.as_deref(),
                numeric: numeric_target.is_some(),
                identity: identity.as_ref(),
            };
            let judgement =
                commit_written_value(element, pid, window_id, plan, written_change, &target);
            // What the caller acts on is the control's settled state, so the
            // read that judged the gesture outranks the one taken before it.
            let after = judgement.settled.or(written);
            let (verified, changed) = classify_write(
                before.as_deref(),
                after.as_deref(),
                value,
                numeric_target.is_some(),
            );
            let suffix = match (verified, changed) {
                (Some(true), Some(false)) => " Value already matched; write was idempotent.",
                (Some(true), _) => "",
                (Some(false), _) => " Read-back did not confirm the value; verify via screenshot.",
                (None, _) => " Value is not readable through AX; could not confirm.",
            };
            Ok(SetValueOutcome {
                detail: format!(
                    "✅ Set AXValue on [{element_index}] {role}.{suffix}{commit_detail}",
                    commit_detail = judgement.detail
                ),
                verified,
                changed,
                committed: judgement.committed,
                path: "ax",
                delivered: None,
            })
        } else if let Some(target) = numeric_target {
            // Both direct writes failed for a numeric target — fall back to
            // stepping the control via AXIncrement / AXDecrement actions.
            if step_to_value(element, target) {
                let after = unsafe { copy_string_attr(element, "AXValue") };
                let (verified, changed) =
                    classify_write(before.as_deref(), after.as_deref(), value, true);
                Ok(SetValueOutcome {
                    detail: format!(
                        "✅ Set AXValue on [{element_index}] {role} via AXIncrement/AXDecrement stepping."
                    ),
                    verified,
                    changed,
                    committed: None,
                    path: "ax",
                    delivered: None,
                })
            } else {
                anyhow::bail!("AXUIElementSetAttributeValue(AXValue) failed with error {err}")
            }
        } else {
            anyhow::bail!("AXUIElementSetAttributeValue(AXValue) failed with error {err}")
        }
    }
}

/// Replace a bound control's text through the keystroke rung.
///
/// The order is the one AppKit requires: focus installs the field editor, the
/// selection write makes the first keystroke a replacement rather than an
/// insertion, the keystrokes land in the editor the binding reads, and the
/// end-of-edit key is what makes the binding read it.
#[allow(clippy::too_many_arguments)]
fn retype_blocking(
    element: AXUIElementRef,
    element_ptr: usize,
    element_index: usize,
    pid: i32,
    window_id: u32,
    value: &str,
    end: &'static str,
    role: &str,
) -> anyhow::Result<SetValueOutcome> {
    let before = unsafe { copy_string_attr(element, "AXValue") };
    // Taken before the edit: an app that re-creates the control when the edit
    // session ends leaves `element` invalid, and this is how the read-back
    // finds the control the app put in its place.
    let identity = capture_identity(element, role);

    crate::input::ax_actions::focus_element(element_ptr)
        .map_err(|error| anyhow::anyhow!("could not focus [{element_index}] {role}: {error}"))?;
    std::thread::sleep(FOCUS_SETTLE);
    if !crate::input::ax_actions::is_element_focused(pid, element_ptr) {
        // AppKit can install the window's remembered first responder just
        // after the write. One re-apply covers that.
        let _ = crate::input::ax_actions::focus_element(element_ptr);
        std::thread::sleep(FOCUS_SETTLE);
    }
    if !crate::input::ax_actions::is_element_focused(pid, element_ptr) {
        anyhow::bail!(
            "[{element_index}] {role} did not take keyboard focus, so typed keystrokes cannot \
             be proven to reach it; click the control first, or retry with \
             delivery_mode \"foreground\""
        );
    }

    let existing = selection_length(before.as_deref());
    if existing > 0 && !select_all_text(element, existing) {
        anyhow::bail!(
            "[{element_index}] {role} refused both an AXSelectedTextRange selection and an \
             AXValue clear, so typing would append to its current value"
        );
    }

    // A keystroke has to make the change for the editor to notice it, so an
    // empty value is delivered as a deletion of the selection.
    let delivery = if value.is_empty() {
        crate::input::keyboard::press_key(pid, "delete", &[])?;
        std::thread::sleep(COMMIT_SETTLE);
        let after = unsafe { copy_string_attr(element, "AXValue") };
        super::type_text::TypedDelivery {
            verified: after.as_deref() == Some(""),
            delivered: Some(0),
            normalized: false,
        }
    } else {
        super::type_text::type_and_drain(
            pid,
            value,
            0,
            Some(""),
            Some((element_ptr, Some(element_index))),
            Some(window_id),
        )?
    };
    let (delivered_all, delivered) = (delivery.verified, delivery.delivered);

    let dispatched = crate::input::keyboard::press_key(pid, end, &[]).is_ok();
    let (after, resolved) = if dispatched {
        std::thread::sleep(COMMIT_SETTLE);
        read_back_after_commit(element, identity.as_ref())
    } else {
        (None, None)
    };
    let readback = if dispatched {
        classify_readback(after.as_deref(), before.as_deref(), value, false)
    } else {
        CommitReadback::NotDispatched
    };
    // An AppKit edit session ends when the control gives up keyboard focus —
    // and a control its app replaced on end-of-edit ended the session by
    // definition, which is why a resolved replacement counts as that same
    // observation rather than a lost one.
    let edit_ended = dispatched
        && (resolved.is_some() || !crate::input::ax_actions::is_element_focused(pid, element_ptr));
    let witness = if edit_ended {
        CommitWitness::TypedEditEnded
    } else {
        CommitWitness::ReadbackOnly
    };
    let (committed, commit_detail) = judge_commit(readback, witness, end);

    let (verified, changed) = classify_write(before.as_deref(), after.as_deref(), value, false);
    let delivery = match (delivered_all, delivered) {
        (true, _) => String::new(),
        (false, Some(count)) => format!(
            " Only {count} of {} character(s) were observed in the control.",
            value.chars().count()
        ),
        (false, None) => " Delivery is not readable through AX.".to_owned(),
    };
    let gesture = if value.is_empty() {
        format!("✅ Cleared [{element_index}] {role}")
    } else {
        format!(
            "✅ Typed {} character(s) into [{element_index}] {role}",
            value.chars().count()
        )
    };
    Ok(SetValueOutcome {
        detail: format!(
            "{gesture} through the app's own editing pipeline.{delivery}{commit_detail}"
        ),
        verified,
        changed,
        committed,
        path: "key_events",
        delivered,
    })
}

/// How much of the control's current value the selection has to cover for the
/// first keystroke to replace it instead of extending it.
///
/// `AXSelectedTextRange` counts UTF-16 code units, as `NSString` does, so a
/// non-BMP scalar occupies two of them: a `char` count leaves the tail of an
/// emoji-bearing value outside the selection, and the typed text is appended to
/// what stayed behind.
fn selection_length(existing: Option<&str>) -> usize {
    super::type_text::utf16_len(existing.unwrap_or_default())
}

/// Select the control's whole value so the first keystroke replaces it.
/// A control that refuses a selection write still accepts an `AXValue` clear;
/// without one of the two, typing would append to the current value.
fn select_all_text(element: AXUIElementRef, utf16_length: usize) -> bool {
    if unsafe { set_range_attr(element, "AXSelectedTextRange", 0, utf16_length as isize) }
        == kAXErrorSuccess
    {
        return true;
    }
    (unsafe { set_string_attr(element, "AXValue", "") }) == kAXErrorSuccess
}

/// Whether an observed AXValue string equals `expected`. Numeric controls are
/// compared numerically so `"25"` matches a slider that reports `"25.0"`.
fn value_equal(observed: &str, expected: &str, numeric: bool) -> bool {
    if observed == expected {
        return true;
    }
    if !numeric {
        return false;
    }
    match (
        observed.trim().parse::<f64>(),
        expected.trim().parse::<f64>(),
    ) {
        (Ok(a), Ok(b)) => {
            let scale = a.abs().max(b.abs()).max(1.0);
            (a - b).abs() <= 1e-9 * scale
        }
        _ => false,
    }
}

/// Decide what a post-write AXValue read proves.
///
/// Returns `(verified, changed)`:
/// - `verified = None` when AXValue is not readable at all, so the write can be
///   neither confirmed nor denied.
/// - `verified = Some(true)` when the read-back equals the requested value.
///   Numeric controls are compared numerically so `"25"` matches a slider that
///   reports `"25.0"`.
/// - `changed = Some(false)` when the read-back equals what was there before,
///   i.e. the element's value did not move. Combined with `verified` this
///   separates "already had the requested value" (verified + unchanged) from
///   "the write did not take" (unverified + unchanged).
fn classify_write(
    before: Option<&str>,
    after: Option<&str>,
    requested: &str,
    numeric: bool,
) -> (Option<bool>, Option<bool>) {
    let Some(after) = after else {
        return (None, None);
    };
    let verified = value_equal(after, requested, numeric);
    let changed = before.map(|before| !value_equal(after, before, numeric));
    (Some(verified), changed)
}

// ── AXIncrement / AXDecrement stepping fallback ──────────────────────────────

/// Step a numeric control toward `target` using its `AXIncrement` /
/// `AXDecrement` actions. Used only when direct `AXValue` writes are rejected
/// (notably SwiftUI's `AXSlider`, which exposes a readable-but-unsettable
/// `AXValue` plus increment/decrement actions).
///
/// Returns `true` once the control's value lands within half of the last
/// observed step of `target`, `false` if it can't be read or can't be moved.
fn step_to_value(element: AXUIElementRef, target: f64) -> bool {
    // Can't target precisely without feedback — bail if AXValue is unreadable.
    let mut current = match unsafe { copy_number_attr(element, "AXValue") } {
        Some(v) => v,
        None => return false,
    };

    // Half of the last observed step. Start near-zero so we never declare the
    // target "reached" before performing (and observing) a real
    // AXIncrement/AXDecrement — otherwise a slider at 0.0 targeting 0.5 would
    // report success without ever moving. The radius widens only after we learn
    // the control's actual step size from an observed value change.
    let mut step_radius = f64::EPSILON;

    // Hard cap to prevent runaway on a control that never quite converges.
    for _ in 0..500 {
        if (current - target).abs() <= step_radius {
            return true;
        }

        let action = if current < target {
            "AXIncrement"
        } else {
            "AXDecrement"
        };
        let _ = unsafe { perform_action(element, action) };

        let next = match unsafe { copy_number_attr(element, "AXValue") } {
            Some(v) => v,
            None => return false,
        };

        // The action didn't move the value — the control can't be stepped (or
        // has hit a min/max bound short of target). Stop to avoid looping.
        if next == current {
            return false;
        }

        // Refine the stop threshold to half of the actual step the control took.
        let step = (next - current).abs();
        if step > 0.0 {
            step_radius = step / 2.0;
        }
        current = next;
    }

    // Exhausted the iteration cap without converging.
    (current - target).abs() <= step_radius
}

// ── AXPopUpButton path ───────────────────────────────────────────────────────

/// How long a closed popup has to build its menu items after the open action.
const MENU_OPEN_WAIT: std::time::Duration = std::time::Duration::from_millis(1000);
/// How long the popup has to show a pressed option as its value.
const CHOICE_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);
/// How long an open menu has to go away after `AXCancel`.
const MENU_CLOSE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
const MENU_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Choose one option of a popup, in one call.
///
/// A closed native popup publishes no options at all: AppKit builds its
/// menu's accessibility items only while the menu is open. So a closed menu
/// is opened with the control's own action, the option whose title is exactly
/// `value` is pressed, and the popup's `AXValue` is read back — the read-back,
/// not the press's return code, is what says the app took the choice (a menu
/// can close under a window that is not key). A menu this call opened is
/// closed again whenever no choice is made; one the caller opened is left as
/// it was found.
fn select_popup_option(
    element: AXUIElementRef,
    element_index: usize,
    pid: i32,
    value: &str,
    web_content: bool,
) -> anyhow::Result<SetValueOutcome> {
    let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();
    let popup = if title.is_empty() {
        format!("[{element_index}] AXPopUpButton")
    } else {
        format!("[{element_index}] AXPopUpButton \"{title}\"")
    };
    let before = unsafe { copy_string_attr(element, "AXValue") };
    let mut options = unsafe { copy_popup_options(element) };
    let mut opened = false;
    if options.is_empty() {
        // WebKit publishes no options for a closed `<select>`, and its DOM is
        // reachable without a menu at all.
        if web_content && apps::get_app_name_for_pid(pid).as_deref() == Some("Safari") {
            let detail = set_select_via_js(element_index, &title, value)?;
            return Ok(SetValueOutcome {
                detail,
                verified: None,
                changed: None,
                committed: None,
                path: "ax",
                delivered: None,
            });
        }
        options = open_popup_menu(element, &popup)?;
        opened = true;
    }

    let listed = || {
        options
            .iter()
            .map(|option| option.label())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let chosen = match choose_option(options.iter().map(|option| option.name()), value) {
        OptionChoice::Unique(index) => &options[index],
        OptionChoice::Missing { near } => {
            let hint = near
                .map(|index| format!(" (did you mean \"{}\"?)", options[index].name()))
                .unwrap_or_default();
            anyhow::bail!(
                "No option of {popup} is titled exactly \"{value}\"{hint}. Nothing was \
                 selected. Options: [{options}].{menu}",
                options = listed(),
                menu = close_opened_menu(element, opened),
            )
        }
        OptionChoice::Ambiguous => anyhow::bail!(
            "Several options of {popup} are titled \"{value}\", so the title cannot say \
             which one to choose. Nothing was selected. Options: [{options}].{menu}",
            options = listed(),
            menu = close_opened_menu(element, opened),
        ),
    };
    let item = chosen.element.as_ptr() as AXUIElementRef;
    if unsafe { copy_bool_attr(item, "AXEnabled") } == Some(false) {
        anyhow::bail!(
            "Option \"{value}\" of {popup} is disabled, so the app does not offer it right \
             now. Nothing was selected.{menu}",
            menu = close_opened_menu(element, opened),
        )
    }
    let err = unsafe { perform_action(item, "AXPress") };
    if err != kAXErrorSuccess {
        anyhow::bail!(
            "AXPress on option \"{value}\" of {popup} failed with AX error {err}.{menu}",
            menu = close_opened_menu(element, opened),
        )
    }

    let after = read_back_choice(element, chosen.name());
    let verified = after.as_deref().map(|after| after == chosen.name());
    let changed = match (before.as_deref(), after.as_deref()) {
        (Some(before), Some(after)) => Some(before != after),
        _ => None,
    };
    let detail = match (verified, changed) {
        (Some(true), Some(false)) => format!(
            "✅ Set {popup} to \"{value}\": it already read \"{value}\"; the choice was \
             idempotent."
        ),
        (Some(true), _) => format!("✅ Set {popup} to \"{value}\"; it reads back \"{value}\"."),
        (Some(false), _) => format!(
            "✅ Set {popup}: pressed option \"{value}\", but the popup reads back \"{after}\". \
             The app did not take the choice.{menu}",
            after = after.as_deref().unwrap_or_default(),
            menu = close_opened_menu(element, opened),
        ),
        (None, _) => format!(
            "✅ Set {popup}: pressed option \"{value}\"; the popup publishes no readable \
             value, so the choice could not be confirmed.{menu}",
            menu = close_opened_menu(element, opened),
        ),
    };
    Ok(SetValueOutcome {
        detail,
        verified,
        changed,
        committed: None,
        path: "ax",
        delivered: None,
    })
}

/// Open a closed popup's menu and return its options once AppKit has built
/// them. `AXPress` is what a click does; `AXShowMenu` is the other action
/// AppKit popups answer, tried when the first one opens nothing.
fn open_popup_menu(
    element: AXUIElementRef,
    popup: &str,
) -> anyhow::Result<Vec<crate::ax::popup::PopupOption>> {
    let advertised = unsafe { copy_action_names(element) };
    let mut attempts = Vec::new();
    for action in ["AXPress", "AXShowMenu"] {
        if !advertised.is_empty() && !advertised.iter().any(|name| name == action) {
            continue;
        }
        let err = unsafe { perform_action(element, action) };
        if err != kAXErrorSuccess {
            attempts.push(format!("{action} failed with AX error {err}"));
            continue;
        }
        let deadline = std::time::Instant::now() + MENU_OPEN_WAIT;
        loop {
            let options = unsafe { copy_popup_options(element) };
            if !options.is_empty() {
                return Ok(options);
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(MENU_POLL);
        }
        // A menu that opened without options is closed before the next try,
        // so the second action cannot toggle it shut instead.
        close_popup_menu(element);
        attempts.push(format!(
            "{action} produced no options within {} ms",
            MENU_OPEN_WAIT.as_millis()
        ));
    }
    if attempts.is_empty() {
        attempts.push(format!(
            "it advertises neither AXPress nor AXShowMenu (actions: {})",
            advertised.join(", ")
        ));
    }
    anyhow::bail!(
        "{popup} offers no options: its menu is closed and opening it did not publish \
         any ({}). Nothing was selected.",
        attempts.join("; ")
    )
}

/// The popup's value once it shows `expected`, or whatever it settled on.
fn read_back_choice(element: AXUIElementRef, expected: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + CHOICE_SETTLE_WAIT;
    loop {
        let after = unsafe { copy_string_attr(element, "AXValue") };
        if after.as_deref() == Some(expected) || std::time::Instant::now() >= deadline {
            return after;
        }
        std::thread::sleep(MENU_POLL);
    }
}

/// Close the popup's open menu with the menu's own `AXCancel`. `true` once no
/// menu is open.
fn close_popup_menu(element: AXUIElementRef) -> bool {
    let Some(menu) = (unsafe { copy_open_menu(element) }) else {
        return true;
    };
    unsafe { perform_action(menu.as_ptr() as AXUIElementRef, "AXCancel") };
    drop(menu);
    let deadline = std::time::Instant::now() + MENU_CLOSE_WAIT;
    loop {
        if unsafe { copy_open_menu(element) }.is_none() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(MENU_POLL);
    }
}

/// Close a menu this call opened, and say what state it was left in. A menu
/// the caller had open is theirs: it is left, and nothing is said.
fn close_opened_menu(element: AXUIElementRef, opened: bool) -> &'static str {
    if !opened || unsafe { copy_open_menu(element) }.is_none() {
        ""
    } else if close_popup_menu(element) {
        " The menu this call opened was closed again."
    } else {
        " The menu this call opened is still open: it did not answer AXCancel."
    }
}

// ── Date and time controls ──────────────────────────────────────────────────

/// Write a date or time control's `CFDate` from an ISO-8601 `value`.
///
/// The control publishes its value only as a `CFDate` and refuses a
/// `CFString` carrying the same date, so the value is parsed
/// (`crate::ax::date`, local time unless an offset is given) and written as a
/// `CFDate`, then read back as one. The keystroke rung is used only when the
/// control refuses that write; a value that is not ISO-8601 is refused before
/// anything is written, naming the forms that are accepted.
#[allow(clippy::too_many_arguments)]
fn write_date_blocking(
    element: AXUIElementRef,
    element_ptr: usize,
    element_index: usize,
    pid: i32,
    window_id: u32,
    value: &str,
    fallback: DateFallback,
    role: &str,
) -> anyhow::Result<SetValueOutcome> {
    use crate::ax::date::{parse_iso8601, ACCEPTED_FORMS};

    let before = unsafe { copy_date_attr(element, "AXValue") };
    let render = |absolute: Option<f64>| absolute.and_then(local_iso8601_for_absolute_time);
    let Some(request) = parse_iso8601(value) else {
        let reads = render(before)
            .map(|current| format!(" It reads {current} now."))
            .unwrap_or_default();
        anyhow::bail!(
            "[{element_index}] {role} holds a date, and \"{value}\" is not an ISO-8601 date: \
             write {ACCEPTED_FORMS}. Nothing was written.{reads}"
        )
    };
    let Some(target) = request.absolute_time(before) else {
        anyhow::bail!(
            "\"{value}\" names a local time this Mac's calendar cannot place. Nothing was \
             written."
        )
    };
    let judge = |after: Option<f64>| {
        let verified = after.map(|after| request.is_held_by(target, after));
        let changed = match (before, after) {
            (Some(before), Some(after)) => Some((after - before).abs() >= 1e-3),
            _ => None,
        };
        (verified, changed)
    };

    let err = unsafe { set_date_attr(element, "AXValue", target) };
    if err == kAXErrorSuccess {
        let after = unsafe { copy_date_attr(element, "AXValue") };
        let (verified, changed) = judge(after);
        let reads = render(after).unwrap_or_default();
        let detail = match (verified, changed) {
            (Some(true), Some(false)) => format!(
                "✅ Set [{element_index}] {role}: it already read {reads}; the write was \
                 idempotent."
            ),
            (Some(true), _) => format!("✅ Set [{element_index}] {role} to {reads}."),
            (Some(false), _) => format!(
                "✅ Set [{element_index}] {role}, but it reads back {reads}, not \"{value}\"."
            ),
            (None, _) => format!(
                "✅ Set [{element_index}] {role}; its value is no longer a readable date, so \
                 the write could not be confirmed."
            ),
        };
        return Ok(SetValueOutcome {
            detail,
            verified,
            changed,
            committed: None,
            path: "ax",
            delivered: None,
        });
    }

    let end = match fallback {
        DateFallback::Retype(end) => end,
        DateFallback::Refused(reason) => anyhow::bail!(
            "[{element_index}] {role} refused a CFDate write (AX error {err}), and the \
             keystroke fallback is unavailable: {reason}. Nothing was written."
        ),
    };
    let mut outcome = retype_blocking(
        element,
        element_ptr,
        element_index,
        pid,
        window_id,
        value,
        end,
        role,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "[{element_index}] {role} refused a CFDate write (AX error {err}), and typing \
             the value failed too: {error}"
        )
    })?;
    // The typed route judged a string read-back, which a date control never
    // publishes; the date it holds now is the evidence.
    let after = unsafe { copy_date_attr(element, "AXValue") };
    (outcome.verified, outcome.changed) = judge(after);
    outcome.detail = format!(
        "{} It refused a CFDate write (AX error {err}), so the value was typed; {}",
        outcome.detail,
        match (outcome.verified, render(after)) {
            (Some(true), Some(reads)) => format!("it reads back {reads}."),
            (Some(false), Some(reads)) => format!("it reads back {reads}, not \"{value}\"."),
            _ => "its date is not readable, so the value could not be confirmed.".to_owned(),
        }
    );
    Ok(outcome)
}

// ── Safari JavaScript fallback ───────────────────────────────────────────────

/// Set an HTML `<select>` value in Safari via `osascript do JavaScript`.
/// Searches all `<select>` elements for an `<option>` whose text or value matches
/// `value` (case-insensitive), then sets it and dispatches a `change` event.
fn set_select_via_js(
    element_index: usize,
    element_title: &str,
    value: &str,
) -> anyhow::Result<String> {
    // Percent-encode the lowercased value using only unreserved URL characters
    // as the allowed set, matching the Swift reference's percent-encoding approach.
    // This makes the string safe to embed in both a JS single-quoted string
    // (via decodeURIComponent) and an AppleScript double-quoted string.
    let v_low = value.to_lowercase();
    let v_encoded = percent_encode_unreserved(&v_low);

    // JavaScript that matches the Swift reference verbatim.
    let js = format!(
        "(function(){{\
         var v=decodeURIComponent('{v_encoded}');\
         var ss=document.querySelectorAll('select'),opts=[];\
         for(var i=0;i<ss.length;i++){{\
         for(var j=0;j<ss[i].options.length;j++){{\
         var t=ss[i].options[j].text.toLowerCase(),\
         u=ss[i].options[j].value.toLowerCase();\
         opts.push(t+'|'+u);\
         if(t===v||u===v){{\
         ss[i].value=ss[i].options[j].value;\
         ss[i].dispatchEvent(new Event('change',{{bubbles:true}}));\
         return 'SET:'+ss[i].value;}}}}\
         }}return 'NOTFOUND:'+opts.join(',');\
         }})()"
    );

    let apple_script =
        format!("tell application \"Safari\" to do JavaScript \"{js}\" in front document");

    // Spawn osascript with a 10-second deadline. A stuck Safari permission
    // prompt or unresponsive renderer can cause wait() to block indefinitely,
    // which would stall the MCP tool handler permanently.
    let mut child = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&apple_script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("osascript launch failed: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    anyhow::bail!("osascript timed out after 10 seconds");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => anyhow::bail!("osascript wait error: {e}"),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("osascript output error: {e}"))?;

    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();

    if let Some(dom_val) = raw.strip_prefix("SET:") {
        Ok(format!(
            "✅ Set select [{element_index}] '{element_title}' to '{value}' via \
             Safari JavaScript (DOM value: \"{dom_val}\")."
        ))
    } else if let Some(available) = raw.strip_prefix("NOTFOUND:") {
        anyhow::bail!(
            "No <option> matching '{value}' found in any <select>. \
             Available (text|value): {available}"
        )
    } else if raw.is_empty() && !out.status.success() {
        let err_text = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("osascript failed: {}", err_text.trim())
    } else {
        anyhow::bail!(
            "JavaScript returned unexpected output: {}",
            &raw[..raw.len().min(200)]
        )
    }
}

// ── Percent-encoding helper ──────────────────────────────────────────────────

/// Percent-encode a string, leaving only unreserved URL characters (`-._~` +
/// alphanumerics) unencoded.  Matches the Swift reference's approach.
fn percent_encode_unreserved(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0xF));
        }
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _ => '0',
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_surface_trust, apply_verification_label, classify_readback, classify_write,
        commit_written_value, judge_commit, refused_keystroke_plan, selection_length,
        unwritable_value, write_plan, CommitReadback, CommitTarget, CommitWitness, SetValueOutcome,
        ToolResult, ValueAttribute, ValueNotSettable, WindowAnswer, WritePlan,
    };
    use cua_driver_contract::ActionCommit;

    fn outcome(
        detail: &str,
        verified: Option<bool>,
        committed: Option<ActionCommit>,
    ) -> SetValueOutcome {
        SetValueOutcome {
            detail: detail.to_owned(),
            verified,
            changed: Some(true),
            committed,
            path: "ax",
            delivered: None,
        }
    }

    /// A commit target with no identity to re-resolve: the branches these tests
    /// exercise never touch an element.
    fn target<'a>(requested: &'a str, before: Option<&'a str>) -> CommitTarget<'a> {
        CommitTarget {
            requested,
            before,
            numeric: false,
            identity: None,
        }
    }

    /// What Contacts' phone field reads back after its own end-of-edit, as the
    /// bench trees print it: the number parenthesised and wrapped in the bidi
    /// isolates AppKit's phone formatter adds (U+202D … U+202C), written as
    /// escapes because raw direction-changing codepoints are a rustc error.
    const REFORMATTED_PHONE: &str = "\u{202d}(555) 789-0123\u{202c}";

    /// The Automator "Save as:" field advertises `AXConfirm`, and an `AXValue`
    /// write plus that action left the app holding `Untitled.txt`. A plain
    /// `NSTextField` has to be typed into, whatever it advertises.
    #[test]
    fn a_binding_target_field_is_written_by_typing() {
        assert_eq!(
            write_plan("AXTextField", "", &["AXConfirm".to_owned()], "name.txt"),
            WritePlan::Retype("tab")
        );
        assert_eq!(
            write_plan("AXTextField", "", &[], "name.txt"),
            WritePlan::Retype("tab")
        );
        assert_eq!(
            write_plan("AXSecureTextField", "", &[], "name.txt"),
            WritePlan::Retype("tab")
        );
    }

    /// Tab moves focus and Return ends the edit, so a value holding either can
    /// only be written losslessly through `AXValue`.
    #[test]
    fn a_value_keystrokes_cannot_carry_keeps_the_ax_value_route() {
        for value in ["\n", " \tΩ café\n", "a\rb"] {
            assert_eq!(
                write_plan("AXTextField", "", &["AXConfirm".to_owned()], value),
                WritePlan::ValueThenConfirm,
                "{value:?}"
            );
        }
        assert_eq!(
            write_plan("AXTextField", "", &[], ""),
            WritePlan::Retype("tab"),
            "an empty value is delivered as a deletion, which is typeable"
        );
    }

    /// A search field keeps the `AXValue` route, and its advertised
    /// `AXConfirm` is judged by the window it searches, not by the value
    /// surviving it: Font Book's field took the action and ran no search.
    #[test]
    fn a_search_field_keeps_the_ax_value_route() {
        assert_eq!(
            write_plan(
                "AXTextField",
                "AXSearchField",
                &["AXConfirm".to_owned()],
                "name.txt"
            ),
            WritePlan::ValueThenSearch
        );
        // Any other control's own confirm is its end-of-edit.
        assert_eq!(
            write_plan("AXComboBox", "", &["AXConfirm".to_owned()], "name.txt"),
            WritePlan::ValueThenConfirm
        );
        assert_eq!(
            write_plan("AXSearchField", "", &[], "name.txt"),
            WritePlan::ValueThenKey("return")
        );
        assert_eq!(
            write_plan("AXComboBox", "", &[], "name.txt"),
            WritePlan::ValueThenKey("tab")
        );
    }

    #[test]
    fn controls_without_an_editing_pipeline_report_no_commit() {
        assert_eq!(
            write_plan("AXSlider", "", &[], "name.txt"),
            WritePlan::ValueOnly
        );
        assert_eq!(
            write_plan("AXCheckBox", "", &[], "name.txt"),
            WritePlan::ValueOnly
        );
        let judgement = commit_written_value(
            std::ptr::null_mut(),
            0,
            0,
            WritePlan::ValueOnly,
            Some(true),
            &target("value", Some("old")),
        );
        assert_eq!(
            judgement.committed, None,
            "a slider write has nothing to commit"
        );
        assert!(judgement.detail.is_empty());
    }

    #[test]
    fn a_text_area_write_is_reported_as_uncommitted() {
        // Measured on TextEdit: the text appeared in the AX tree while the
        // document stayed unmarked. There is no end-of-edit gesture to run.
        let plan = write_plan("AXTextArea", "", &[], "name.txt");
        assert!(matches!(plan, WritePlan::Blocked(_)), "{plan:?}");
        let judgement = commit_written_value(
            std::ptr::null_mut(),
            0,
            0,
            plan,
            Some(true),
            &target("value", Some("old")),
        );
        assert_eq!(judgement.committed, Some(ActionCommit::NotCommitted));
        assert!(
            judgement.detail.contains("Not committed"),
            "{}",
            judgement.detail
        );
    }

    /// A read-back is echoed by a bound control whether or not the app took
    /// the value, so surviving the gesture alone can never say `committed` —
    /// and neither can an ended edit session over a value the field editor
    /// never saw. Measured on Automator's "Save as:" action parameter: an
    /// `AXValue` write plus a Tab that moved focus survived the read-back and
    /// the app still kept its own value on disk.
    #[test]
    fn a_readback_alone_is_never_a_commit() {
        let (committed, detail) = judge_commit(
            CommitReadback::Matches,
            CommitWitness::ReadbackOnly,
            "AXConfirm",
        );
        assert_eq!(committed, Some(ActionCommit::Unproven));
        assert!(detail.contains("Commit unproven"), "{detail}");

        // The AXValue + Tab route: dispatched, survived, and the edit really
        // ended — but untyped, so the only honest verdict is unproven.
        assert_eq!(
            judge_commit(CommitReadback::Matches, CommitWitness::ReadbackOnly, "tab").0,
            Some(ActionCommit::Unproven)
        );
        assert_eq!(
            judge_commit(CommitReadback::Matches, CommitWitness::TypedEditEnded, "tab").0,
            Some(ActionCommit::Committed)
        );
        assert_eq!(
            judge_commit(
                CommitReadback::NotDispatched,
                CommitWitness::ReadbackOnly,
                "tab"
            )
            .0,
            Some(ActionCommit::NotCommitted)
        );
    }

    /// `not committed` makes exactly one claim — the application put its own
    /// value back — so only the read-back that shows that may carry it.
    #[test]
    fn only_the_apps_own_value_coming_back_refutes_the_commit() {
        let (committed, detail) =
            judge_commit(CommitReadback::Restored, CommitWitness::TypedEditEnded, "tab");
        assert_eq!(committed, Some(ActionCommit::NotCommitted));
        assert!(detail.contains("still holds its own value"), "{detail}");
    }

    /// A value the app's end-of-edit rewrote is the app having *taken* the
    /// edit, not having refused it. Contacts reformats a phone number it keeps
    /// (`555-789-0123` comes back parenthesised and wrapped in bidi isolates)
    /// and three bench writes were reported as "the app still holds its own
    /// value" while the same reply's tree printed the value under the field.
    /// What the control holds now is the only thing worth reporting, quoted,
    /// so the caller can judge it.
    #[test]
    fn a_rewritten_value_is_unproven_and_quotes_what_it_reads() {
        let (committed, detail) = judge_commit(
            CommitReadback::Rewritten(REFORMATTED_PHONE),
            CommitWitness::TypedEditEnded,
            "tab",
        );
        assert_eq!(committed, Some(ActionCommit::Unproven));
        assert!(
            detail.contains(&format!("reads back as \"{REFORMATTED_PHONE}\"")),
            "{detail}"
        );
        assert!(!detail.contains("Not committed"), "{detail}");
    }

    /// An app may replace the control itself when the edit session ends, which
    /// leaves the addressed pointer answering nothing. That is a lost
    /// observation, not a refused edit.
    #[test]
    fn a_control_that_stopped_answering_is_unproven() {
        let (committed, detail) = judge_commit(
            CommitReadback::Unreadable,
            CommitWitness::TypedEditEnded,
            "tab",
        );
        assert_eq!(committed, Some(ActionCommit::Unproven));
        assert!(
            detail.contains("the control was re-created on end-of-edit; read it back"),
            "{detail}"
        );

        // A control that never published a value has nothing to do with a
        // rebuild and must not be told it was re-created.
        let (committed, detail) = judge_commit(
            CommitReadback::Unpublished,
            CommitWitness::TypedEditEnded,
            "tab",
        );
        assert_eq!(committed, Some(ActionCommit::Unproven));
        assert!(detail.contains("publishes no readable value"), "{detail}");
        assert!(!detail.contains("re-created"), "{detail}");
    }

    /// The comparison stays exact in both directions. A read-back that differs
    /// from the request only by the app's own formatting is still a different
    /// value: normalising digits, whitespace or bidi isolates would report the
    /// application's value as the caller's.
    #[test]
    fn a_reformatted_read_back_is_never_read_as_a_match() {
        assert_eq!(
            classify_readback(Some(REFORMATTED_PHONE), Some(""), "555-789-0123", false),
            CommitReadback::Rewritten(REFORMATTED_PHONE)
        );
        assert_eq!(
            classify_readback(Some(" ramp"), Some("old"), "ramp", false),
            CommitReadback::Rewritten(" ramp")
        );
        assert_eq!(
            classify_readback(Some("ramp"), Some("old"), "ramp", false),
            CommitReadback::Matches
        );
        assert_eq!(
            classify_readback(Some("old"), Some("old"), "ramp", false),
            CommitReadback::Restored
        );
        // Without a prior value nothing can be shown to have been put back.
        assert_eq!(
            classify_readback(Some("old"), None, "ramp", false),
            CommitReadback::Rewritten("old")
        );
        assert_eq!(
            classify_readback(None, Some("old"), "ramp", false),
            CommitReadback::Unreadable
        );
        assert_eq!(
            classify_readback(None, None, "ramp", false),
            CommitReadback::Unpublished
        );
        // A numeric control's own formatting is not the app rewriting a value.
        assert_eq!(
            classify_readback(Some("25.0"), Some("10"), "25", true),
            CommitReadback::Matches
        );
    }

    /// `AXSelectedTextRange` is in UTF-16 units: selecting `chars().count()`
    /// of a value holding a non-BMP scalar leaves its tail unselected, and the
    /// replacement is appended to what stayed behind ("😀AB" retyped as "zed"
    /// becomes "zedB").
    #[test]
    fn the_replaced_selection_is_measured_in_utf16_units() {
        assert_eq!(selection_length(Some("😀AB")), 4);
        assert_eq!(
            "😀AB".chars().count(),
            3,
            "a char count would select 3 of the 4 units"
        );
        assert_eq!(selection_length(Some("ramp")), 4);
        assert_eq!(selection_length(Some("")), 0);
        assert_eq!(selection_length(None), 0);
    }

    /// The control's own `AXConfirm` is its action handler — the one Return
    /// invokes — and it is selected only for a control that advertises it. Run
    /// over a value this call provably moved the field to, that IS the app's
    /// end-of-edit. Every one of the 23 landed writes in the 0917 census read
    /// back as written and still published `unproven`, so the verdict carried
    /// no information.
    #[test]
    fn a_confirmed_change_on_the_controls_own_action_commits() {
        let (committed, detail) = judge_commit(
            CommitReadback::Matches,
            CommitWitness::OwnConfirmAction,
            "AXConfirm",
        );
        assert_eq!(committed, Some(ActionCommit::Committed));
        assert!(detail.contains("Committed via AXConfirm"), "{detail}");

        // An idempotent write moved nothing, so the survival is an echo of a
        // value the field already held and there is no commit to prove.
        assert_eq!(
            judge_commit(
                CommitReadback::Matches,
                CommitWitness::ReadbackOnly,
                "AXConfirm"
            )
            .0,
            Some(ActionCommit::Unproven)
        );
        // The confirm still has to have been dispatched, and the app still has
        // to have kept what it changed the control to.
        assert_eq!(
            judge_commit(
                CommitReadback::NotDispatched,
                CommitWitness::OwnConfirmAction,
                "AXConfirm"
            )
            .0,
            Some(ActionCommit::NotCommitted)
        );
        assert_eq!(
            judge_commit(
                CommitReadback::Restored,
                CommitWitness::OwnConfirmAction,
                "AXConfirm"
            )
            .0,
            Some(ActionCommit::NotCommitted)
        );
    }

    /// `set_value` has no `delivery_mode`, so a refused keystroke is final for
    /// the call. A save panel is always a sibling top-level window, which is
    /// exactly what the gate refuses on — dropping to the control's own
    /// advertised `AXConfirm` keeps that control reachable instead of turning
    /// the whole write into a refusal.
    #[test]
    fn a_refused_retype_falls_back_to_an_advertised_confirm() {
        let refusal = ToolResult::error("refused".to_owned());
        assert_eq!(
            refused_keystroke_plan(
                &WritePlan::Retype("tab"),
                &["AXShowMenu".to_owned(), "AXConfirm".to_owned()],
                &refusal,
            ),
            WritePlan::ValueThenConfirm
        );
        assert!(matches!(
            refused_keystroke_plan(&WritePlan::Retype("tab"), &[], &refusal),
            WritePlan::Blocked(_)
        ));
        // The AXValue routes have no second gesture to fall back to.
        assert!(matches!(
            refused_keystroke_plan(
                &WritePlan::ValueThenKey("tab"),
                &["AXConfirm".to_owned()],
                &refusal,
            ),
            WritePlan::Blocked(_)
        ));
    }

    #[test]
    fn unreadable_value_reports_neither_verified_nor_changed() {
        // AXValue is not exposed: the write can be neither confirmed nor denied,
        // so the tool must not claim success on the return code alone.
        assert_eq!(
            classify_write(Some("old"), None, "new", false),
            (None, None)
        );
    }

    #[test]
    fn matching_read_back_verifies_the_write() {
        assert_eq!(
            classify_write(Some("old"), Some("new"), "new", false),
            (Some(true), Some(true))
        );
    }

    #[test]
    fn echoed_but_wrong_value_fails_verification() {
        // Web content behind an AXWebArea accepts the write and echoes a value
        // the renderer never took. A success return code must not be reported
        // as a verified write.
        assert_eq!(
            classify_write(Some("old"), Some("old"), "new", false),
            (Some(false), Some(false))
        );
    }

    #[test]
    fn idempotent_write_is_verified_but_unchanged() {
        assert_eq!(
            classify_write(Some("same"), Some("same"), "same", false),
            (Some(true), Some(false))
        );
    }

    #[test]
    fn numeric_controls_compare_numerically() {
        // AXSlider reports "25.0" for a requested "25".
        assert_eq!(
            classify_write(Some("10"), Some("25.000000001"), "25", true),
            (Some(true), Some(true))
        );
    }

    #[test]
    fn numeric_text_is_not_normalised_on_a_text_target() {
        assert_eq!(
            classify_write(Some("old"), Some("7"), "007", false),
            (Some(false), Some(true))
        );
    }

    #[test]
    fn missing_before_still_verifies_numeric_after() {
        assert_eq!(
            classify_write(None, Some("25.0"), "25", true),
            (Some(true), None)
        );
    }

    #[test]
    fn web_content_ax_echo_is_never_reported_as_verified() {
        let mut result = outcome("Set value.", Some(true), Some(ActionCommit::Committed));
        apply_surface_trust(&mut result, true);
        assert_eq!(result.verified, Some(false));
        assert_eq!(result.changed, None);
        assert!(result.detail.contains("not trusted for web content"));
    }

    #[test]
    fn native_read_back_remains_trusted() {
        let mut result = outcome("Set value.", Some(true), Some(ActionCommit::Committed));
        apply_surface_trust(&mut result, false);
        assert_eq!(result.verified, Some(true));
        assert_eq!(result.changed, Some(true));
        assert_eq!(result.detail, "Set value.");
    }

    #[test]
    fn unverified_result_does_not_keep_a_success_checkmark() {
        let mut result = outcome(
            "✅ Set AXValue on [4] AXTextField.",
            Some(false),
            Some(ActionCommit::NotCommitted),
        );
        apply_verification_label(&mut result);
        assert_eq!(
            result.detail,
            "📨 Sent (unverified) AXValue on [4] AXTextField."
        );

        let mut typed = outcome(
            "✅ Typed 4 character(s) into [4] AXTextField.",
            Some(false),
            Some(ActionCommit::Unproven),
        );
        apply_verification_label(&mut typed);
        assert_eq!(
            typed.detail,
            "📨 Sent (unverified) 4 character(s) into [4] AXTextField."
        );
    }

    /// Activity Monitor's collapsed toolbar search is an `AXButton` with
    /// subrole `AXSearchField` that publishes no `AXValue`. `set_value` wrote
    /// the button anyway, the app answered success, and the list stayed
    /// unfiltered (V3D native-read-activity-processes, 4 of 4 runs).
    #[test]
    fn a_control_without_a_writable_value_is_refused_before_the_write() {
        let press = ["AXPress".to_owned()];
        assert_eq!(
            unwritable_value("AXButton", false, ValueAttribute::Absent, &press, "Dock"),
            Some(ValueAttribute::Absent)
        );
        assert_eq!(
            unwritable_value("AXTextField", false, ValueAttribute::ReadOnly, &[], "Dock"),
            Some(ValueAttribute::ReadOnly)
        );
        // A settable value, or one the element did not answer about, keeps
        // the write: an unanswered question is not a "no".
        for answered in [ValueAttribute::Settable, ValueAttribute::Unknown] {
            assert_eq!(
                unwritable_value("AXButton", false, answered, &press, "Dock"),
                None
            );
        }
    }

    /// Three routes write something other than a string into `AXValue`, so a
    /// read-only `AXValue` does not close them.
    #[test]
    fn routes_that_do_not_write_axvalue_keep_their_own_refusals() {
        // A popup's option is pressed, not written.
        assert_eq!(
            unwritable_value("AXPopUpButton", false, ValueAttribute::ReadOnly, &[], "Red"),
            None
        );
        // A date control takes a CFDate, and types when it refuses one.
        assert_eq!(
            unwritable_value("AXDateTimeArea", true, ValueAttribute::ReadOnly, &[], "2026-09-27"),
            None
        );
        // SwiftUI's slider publishes its value read-only and steps.
        let steps = ["AXIncrement".to_owned(), "AXDecrement".to_owned()];
        assert_eq!(
            unwritable_value("AXSlider", false, ValueAttribute::ReadOnly, &steps, " 40 "),
            None
        );
        // Stepping only reaches a number.
        assert_eq!(
            unwritable_value("AXSlider", false, ValueAttribute::ReadOnly, &steps, "loud"),
            Some(ValueAttribute::ReadOnly)
        );
    }

    fn not_settable(subrole: &str, advertised: &[&str], attribute: ValueAttribute) -> ValueNotSettable {
        ValueNotSettable {
            role: "AXButton".to_owned(),
            subrole: subrole.to_owned(),
            label: "Search".to_owned(),
            window_id: 4242,
            pid: 77,
            advertised: advertised.iter().map(|action| (*action).to_owned()).collect(),
            value_attribute: attribute,
        }
    }

    /// The refusal names the route the element itself offers, and only that:
    /// pressing a collapsed search item expands the field that takes a value.
    #[test]
    fn the_refusal_names_the_elements_own_route() {
        let collapsed = not_settable("AXSearchField", &["AXPress"], ValueAttribute::Absent);
        let reason = collapsed.reason();
        assert!(reason.starts_with("set_value was not dispatched: "), "{reason}");
        assert!(reason.contains("AXButton/AXSearchField \"Search\" of window 4242"), "{reason}");
        assert!(reason.contains("publishes no AXValue"), "{reason}");
        assert!(reason.contains("press it to expand the search field"), "{reason}");

        let button = not_settable("", &["AXPress"], ValueAttribute::ReadOnly).reason();
        assert!(button.contains("AXButton \"Search\""), "{button}");
        assert!(button.contains("read-only"), "{button}");
        assert!(!button.contains("search field"), "{button}");

        let inert = not_settable("", &[], ValueAttribute::ReadOnly).reason();
        assert!(!inert.contains("AXPress"), "{inert}");
        assert!(inert.contains("advertises no action"), "{inert}");
    }

    #[test]
    fn the_refusal_payload_carries_the_state_it_was_decided_on() {
        let result = not_settable("AXSearchField", &["AXPress"], ValueAttribute::Absent).result();
        assert_eq!(result.is_error, Some(true));
        let payload = result.structured_content.expect("refusal payload");
        assert_eq!(payload["code"], "value_not_settable");
        assert_eq!(payload["effect"], "not_dispatched");
        assert_eq!(payload["action"], "set_value");
        assert_eq!(payload["role"], "AXButton");
        assert_eq!(payload["subrole"], "AXSearchField");
        assert_eq!(payload["value_attribute"], "absent");
        assert_eq!(payload["advertised_actions"], serde_json::json!(["AXPress"]));
        assert_eq!(payload["window_id"], 4242);
        assert!(payload.get("escalation").is_none());

        let plain = not_settable("", &[], ValueAttribute::ReadOnly).payload();
        assert_eq!(plain["value_attribute"], "read_only");
        assert!(plain.get("subrole").is_none());
    }

    /// Font Book's search field took `AXValue` and `AXConfirm`, read back as
    /// written, and ran no search: the window stayed on "All Fonts – 362
    /// typefaces" while the reply said committed (V3D fontbook, 4 of 4 runs).
    /// A search field's confirm is a commit only when the window answers it.
    #[test]
    fn a_search_the_window_never_answered_is_unproven() {
        let (committed, detail) = judge_commit(
            CommitReadback::Matches,
            CommitWitness::SearchWindow(WindowAnswer::Unchanged {
                waited: std::time::Duration::from_millis(2000),
            }),
            "AXConfirm",
        );
        assert_eq!(committed, Some(ActionCommit::Unproven));
        assert!(detail.contains("within 2000 ms"), "{detail}");
        assert!(detail.contains("type the query into the field"), "{detail}");

        let (committed, detail) = judge_commit(
            CommitReadback::Matches,
            CommitWitness::SearchWindow(WindowAnswer::Unwatched),
            "AXConfirm",
        );
        assert_eq!(committed, Some(ActionCommit::Unproven));
        assert!(detail.contains("could not be watched"), "{detail}");
    }

    /// Automator's library search filters on the same action: the window
    /// changing after it is the app running the search.
    #[test]
    fn a_search_the_window_answered_commits() {
        let (committed, detail) = judge_commit(
            CommitReadback::Matches,
            CommitWitness::SearchWindow(WindowAnswer::Changed),
            "AXConfirm",
        );
        assert_eq!(committed, Some(ActionCommit::Committed));
        assert!(detail.contains("the window changed after it"), "{detail}");

        // The window answering never outranks the value not surviving.
        let (committed, _) = judge_commit(
            CommitReadback::Restored,
            CommitWitness::SearchWindow(WindowAnswer::Changed),
            "AXConfirm",
        );
        assert_eq!(committed, Some(ActionCommit::NotCommitted));
    }
}
