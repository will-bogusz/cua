//! click tool — matches the Swift reference ClickTool.swift.
//!
//! Two addressing modes:
//!
//! * **AX path** (`element_index` + `window_id`): performs AXAction on the cached
//!   element. Fires via AX RPC — the target app never needs to be frontmost.
//!   Extra behaviors vs. the naive dispatch:
//!   - AXTextField / AXTextArea: 800 ms post-click delay for WebKit DOM focus settle.
//!   - AXPopUpButton: appends the list of available options and redirects to set_value.
//!   - Advertised-action warning if the element didn't list the requested action.
//!
//! * **Pixel path** (`x`, `y`): synthesises CGEvent mouse clicks and posts them to
//!   the target pid.  `from_zoom=true` translates zoom-crop pixel coordinates back
//!   to full-window space using the most recent `zoom` context stored per-pid.

use async_trait::async_trait;
use cua_driver_contract::ClickButton;
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEscalation, ActionEvidence, ActionExecutionRecord, ActionTransport,
        ActualDelivery, EscalationKind, EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_legacy_click_input,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_action_names, copy_children, copy_label_attr, copy_string_attr,
    element_at_screen_position, element_screen_rect, kAXErrorSuccess, AXUIElementPerformAction,
    AXUIElementRef,
};
use crate::focus_guard;
use crate::input::ax_actions::{requests_ax_action, resolve_ax_action, UnknownAxAction};
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::{CFRelease, TCFType};

use super::delivery_probe;
use super::ToolState;

pub struct ClickTool {
    state: Arc<ToolState>,
}

impl ClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Focus posture for the raw pixel transport after AX hit-testing has failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PixelActivationPolicy {
    /// Standard background delivery: suppress activation of the target.
    SuppressTarget,
    /// Left-click with a concrete window: intentionally make the target
    /// AppKit-active without raising it, while suppressing every other app.
    AllowTargetWithoutRaise,
    /// Explicit foreground rung owns its brief activation and restoration.
    ForegroundAssist,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SelectionPixelTarget {
    screen_x: f64,
    screen_y: f64,
    window_x: f64,
    window_y: f64,
}

/// Element gestures use live AX geometry in logical screen points, not a
/// cached screenshot transform. Refuse clipped/off-window centers rather than
/// clamping them to a different control.
fn element_click_point(
    rect: [f64; 4],
    window: &crate::windows::WindowBounds,
) -> anyhow::Result<SelectionPixelTarget> {
    let [x, y, width, height] = rect;
    let cx = x + width / 2.0;
    let cy = y + height / 2.0;
    anyhow::ensure!(
        rect.iter().all(|v| v.is_finite()) && width > 0.0 && height > 0.0
            && [window.x, window.y, window.width, window.height].iter().all(|v| v.is_finite())
            && window.width > 0.0 && window.height > 0.0
            && cx >= window.x && cy >= window.y
            && cx < window.x + window.width && cy < window.y + window.height,
        "Element center is unavailable or outside the exact window; observe fresh state before retrying"
    );
    Ok(SelectionPixelTarget {
        screen_x: cx,
        screen_y: cy,
        window_x: cx - window.x,
        window_y: cy - window.y,
    })
}

fn live_element_click_point(pointer: usize, wid: u32) -> anyhow::Result<SelectionPixelTarget> {
    let element = pointer as AXUIElementRef;
    unsafe {
        anyhow::ensure!(
            crate::ax::exact_target::element_window_id(element) == Some(wid),
            "Element no longer belongs to the exact window; click was not dispatched"
        );
        anyhow::ensure!(
            crate::ax::bindings::copy_bool_attr(element, "AXEnabled") != Some(false),
            "Element is disabled; click was not dispatched"
        );
        let rect = element_screen_rect(element).ok_or_else(|| {
            anyhow::anyhow!("Element has no live geometry; click was not dispatched")
        })?;
        let window = crate::windows::window_bounds_by_id(wid).ok_or_else(|| {
            anyhow::anyhow!("Exact window no longer exists; click was not dispatched")
        })?;
        element_click_point(rect, &window)
    }
}

fn selection_readback_confirms(
    before: bool,
    after: bool,
    has_modifiers: bool,
    prior_selected_peers_preserved: bool,
) -> bool {
    if has_modifiers {
        after != before && prior_selected_peers_preserved
    } else {
        after
    }
}

const SELECTION_READBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const SELECTION_READBACK_POLL: std::time::Duration = std::time::Duration::from_millis(25);
const SELECTION_READBACK_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);
const SELECTION_READBACK_STABILITY: std::time::Duration = std::time::Duration::from_millis(250);

/// A reply error cannot establish that the receiver did nothing. In particular,
/// a system file panel can commit the save and disappear before replying.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("AXUIElementPerformAction({action}) returned {code}")]
struct AxActionReplyError {
    action: String,
    code: crate::ax::bindings::AXError,
}

impl AxActionReplyError {
    /// Replies AppKit applications return *from the action they performed*.
    /// Contacts answers `AXPress` on its Edit/Done buttons with
    /// `kAXErrorFailure` or `kAXErrorAttributeUnsupported` after saving the
    /// card, and a text field answers with `kAXErrorActionUnsupported` after
    /// taking focus; each of those presses is readable in the next snapshot.
    /// The remaining codes are framework-level rejections — an illegal
    /// argument, a dead element, disabled API, a messaging timeout — where
    /// nothing is known to have reached the application at all.
    fn outcome_unverifiable(&self) -> bool {
        matches!(
            self.code,
            crate::ax::bindings::kAXErrorFailure
                | crate::ax::bindings::kAXErrorAttributeUnsupported
                | crate::ax::bindings::kAXErrorActionUnsupported
        )
    }

    fn code_name(&self) -> &'static str {
        match self.code {
            crate::ax::bindings::kAXErrorFailure => "kAXErrorFailure",
            crate::ax::bindings::kAXErrorAttributeUnsupported => "kAXErrorAttributeUnsupported",
            crate::ax::bindings::kAXErrorActionUnsupported => "kAXErrorActionUnsupported",
            crate::ax::bindings::kAXErrorInvalidUIElement => "kAXErrorInvalidUIElement",
            _ => "AXError",
        }
    }

    fn element_is_gone(&self) -> bool {
        self.code == crate::ax::bindings::kAXErrorInvalidUIElement
    }
}

fn dispatch_ax_action(
    action: &str,
    dispatch: impl FnOnce() -> crate::ax::bindings::AXError,
) -> Result<(), AxActionReplyError> {
    let code = dispatch();
    if code == kAXErrorSuccess {
        Ok(())
    } else {
        Err(AxActionReplyError {
            action: action.to_owned(),
            code,
        })
    }
}

/// The requested AX action and whether the caller named it; an omitted `action` is a plain click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RequestedAction<'a> {
    action: &'a str,
    named_by_caller: bool,
}

fn press_route_advice(press_is_advertised: bool) -> &'static str {
    if press_is_advertised {
        " The row's own press action is available as perform(\"press\")."
    } else {
        ""
    }
}

/// What a reply to `AXUIElementPerformAction` licenses the driver to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AxReplyDisposition {
    Performed,
    /// The application answered the action without saying whether it acted, so
    /// nothing further may touch the UI: a selection write could act a second
    /// time, and its read-back would then report a confirmed effect the reply
    /// does not support.
    Dispatched(AxActionReplyError),
    /// A refusal to a plain press. Finder answers `kAXErrorCannotComplete` for
    /// a press on a collection row whose selectable object is an ancestor, so
    /// the bounded `AXSelected` fallback is still worth trying.
    TrySelection(AxActionReplyError),
    Failed(AxActionReplyError),
}

fn ax_reply_disposition(
    reply: Result<(), AxActionReplyError>,
    modifiers: &[String],
) -> AxReplyDisposition {
    let Err(reply) = reply else {
        return AxReplyDisposition::Performed;
    };
    if reply.outcome_unverifiable() {
        AxReplyDisposition::Dispatched(reply)
    } else if reply.action == "AXPress" && modifiers.is_empty() {
        AxReplyDisposition::TrySelection(reply)
    } else {
        AxReplyDisposition::Failed(reply)
    }
}

/// The opening line of an AX action's reply. An unverifiable reply claims a
/// dispatch and nothing more: the post-dispatch probe, the window-change
/// suffix and the caller's own next observation are what settle it, so the
/// text must not send the agent to repeat an action that may have landed.
fn ax_reply_summary(
    unverified: Option<&AxActionReplyError>,
    ax_action: &str,
    idx: usize,
    role: &str,
    title: &str,
) -> String {
    match unverified {
        None => format!("✅ Performed {ax_action} on [{idx}] {role} \"{title}\"."),
        Some(reply) => format!(
            "❔ Dispatched {ax_action} to [{idx}] {role} \"{title}\"; the app replied {} ({}), \
             which does not say whether it acted — applications commonly answer this way *after* \
             performing the action. Effect unverified: read the state below, or observe the \
             window, before deciding. Do not repeat the action solely because of this reply.",
            reply.code,
            reply.code_name()
        ),
    }
}

/// The application's own focus state when a refusal was composed.
///
/// A window is key only while its process holds the frontmost application
/// slot and the process publishes that window as its focused one. AppKit
/// disables controls whose enabled state tracks key-window focus — a toolbar
/// search field reads `AXEnabled=false` in a window that is not key — so this
/// is the fact that separates "the application disabled this control" from
/// "this control is disabled until the window is key".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyWindowState {
    app_frontmost: bool,
    focused_window_id: Option<u32>,
}

impl KeyWindowState {
    fn observe(pid: i32) -> Self {
        Self {
            app_frontmost: crate::apps::frontmost_pid() == Some(pid),
            focused_window_id: crate::ax::bindings::focused_window_id_of_pid(pid),
        }
    }

    fn holds(self, window_id: u32) -> bool {
        self.app_frontmost && self.focused_window_id == Some(window_id)
    }

    /// Which observation denies the window its key status, or `None` when the
    /// window is key.
    fn denial(self, pid: i32, window_id: u32) -> Option<String> {
        if self.holds(window_id) {
            return None;
        }
        if !self.app_frontmost {
            return Some(format!("pid {pid} is not the frontmost application"));
        }
        Some(match self.focused_window_id {
            Some(focused) => format!("window {focused} holds pid {pid}'s keyboard focus"),
            None => format!("pid {pid} reports no focused window"),
        })
    }
}

/// The application itself reports the addressed control disabled, with the
/// focus and window-order state that decides which routes are real.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ElementDisabled {
    action: String,
    role: String,
    label: String,
    window_id: u32,
    pid: i32,
    foreground: bool,
    front_in_process: bool,
    obscuring_window: Option<super::ObscuringWindow>,
    key_window: KeyWindowState,
}

/// The one state that explains the disabled control. Prose and the structured
/// payload branch on the same answer, so a reply cannot name a route its
/// escalation withholds.
enum DisabledCause<'a> {
    MenuItem,
    OwnedPanelInFront(&'a super::ObscuringWindow),
    WindowNotKey(String),
    ForegroundDidNotMakeKey(String),
    ApplicationState,
}

impl ElementDisabled {
    fn cause(&self) -> DisabledCause<'_> {
        if self.role == "AXMenuItem" {
            return DisabledCause::MenuItem;
        }
        // A window in front decides the cause only if raising the target
        // cannot take the keyboard from it. Any other same-process window in
        // front is the process's current front window, which the foreground
        // rung's make-key records plus `AXRaise` on the target overtake — so
        // it is named below as the order, and the key-window fact decides.
        if let Some(obscuring) = &self.obscuring_window {
            if obscuring.holds_keyboard_through_raise() {
                return DisabledCause::OwnedPanelInFront(obscuring);
            }
        }
        match self.key_window.denial(self.pid, self.window_id) {
            // The foreground rung is already in force and the activation it
            // requested has not landed: the control was read in the state the
            // rung was supposed to leave behind, not in the state the
            // application chose for it.
            Some(denial) if self.foreground => DisabledCause::ForegroundDidNotMakeKey(denial),
            Some(denial) => DisabledCause::WindowNotKey(denial),
            None => DisabledCause::ApplicationState,
        }
    }

    /// Where the target sits in its process's window order, as one clause:
    /// front, behind a named window, or with nothing of its process in front.
    fn order(&self) -> String {
        let Self {
            window_id, pid, ..
        } = self;
        match &self.obscuring_window {
            Some(front) => format!(
                "Window {} — pid {pid}'s own front window, {} — is drawn in front of window \
                 {window_id}",
                front.window_id,
                front.describe()
            ),
            None if self.front_in_process => format!(
                "Window {window_id} is already pid {pid}'s front window and no window of pid \
                 {pid} is drawn in front of it"
            ),
            None => format!("No window of pid {pid} is drawn in front of window {window_id}"),
        }
    }

    fn reason(&self) -> String {
        let Self {
            action,
            role,
            label,
            window_id,
            pid,
            ..
        } = self;
        let order = self.order();
        match self.cause() {
            DisabledCause::MenuItem => format!(
                "{action} was not dispatched: the {role} \"{label}\" of an open menu reports \
                 AXEnabled=false. A menu item's enabled state tracks the application's own \
                 applicability, not focus or delivery mode: it is disabled in pid {pid}'s \
                 current state."
            ),
            DisabledCause::OwnedPanelInFront(obscuring) => {
                let blocker = obscuring.window_id;
                let route = if obscuring.is_focusable() {
                    format!("Dismiss that window, or address window {blocker} and act on it there.")
                } else {
                    "It publishes no AXWindow, so it cannot become the focused window: dismiss it, \
                     or act on it by pixel."
                        .to_owned()
                };
                format!(
                    "{action} was not dispatched: {role} \"{label}\" of window {window_id} reports \
                     AXEnabled=false, and window {blocker} — pid {pid}'s own front window, {} — is \
                     drawn in front of it. {route}",
                    obscuring.describe()
                )
            }
            DisabledCause::WindowNotKey(denial) => {
                format!(
                    "{action} was not dispatched: {role} \"{label}\" of window {window_id} \
                     reports AXEnabled=false. {order}, but window {window_id} is not pid {pid}'s \
                     key window — {denial} — and a control whose enabled state tracks key-window \
                     focus reads disabled until its window is key. A foreground dispatch makes it \
                     key first."
                )
            }
            DisabledCause::ForegroundDidNotMakeKey(denial) => {
                format!(
                    "{action} was not dispatched: {role} \"{label}\" of window {window_id} \
                     reports AXEnabled=false. {order}, but the foreground rung did not make \
                     window {window_id} pid {pid}'s key window within its wait — {denial} — so \
                     this control was read while its window was still not key. Re-observe: the \
                     control enables once its window is key."
                )
            }
            DisabledCause::ApplicationState => {
                format!(
                    "{action} was not dispatched: {role} \"{label}\" of window {window_id} reports \
                     AXEnabled=false. {order} — the application disabled this control, and neither \
                     delivery mode nor activation changes that. Satisfy its precondition or choose \
                     another control."
                )
            }
        }
    }

    fn payload(&self) -> Value {
        let mut payload = serde_json::json!({
            "code": "element_disabled",
            "effect": "not_dispatched",
            "route": "ax",
            "action": self.action,
            "role": self.role,
            "label": self.label,
            "window_id": self.window_id,
            "pid": self.pid,
            "foreground": self.foreground,
            "front_in_process": self.front_in_process,
            "key_window": {
                "is_key": self.key_window.holds(self.window_id),
                "app_frontmost": self.key_window.app_frontmost,
                "focused_window_id": self.key_window.focused_window_id,
            },
        });
        if let Some(obscuring) = &self.obscuring_window {
            payload["obscured_by"] = obscuring.payload();
        }
        if matches!(self.cause(), DisabledCause::WindowNotKey(_)) {
            payload["escalation"] = serde_json::json!({
                "target": "foreground",
                "reason": "route_unavailable",
            });
        }
        payload
    }
}

impl std::fmt::Display for ElementDisabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason())
    }
}

impl std::error::Error for ElementDisabled {}

fn ax_action_error(error: anyhow::Error) -> ToolResult {
    if let Some(unknown) = error.downcast_ref::<UnknownAxAction>() {
        return ToolResult::error(format!("click refused: {error}")).with_structured(
            serde_json::json!({
                "code": "action_unsupported",
                "effect": "not_dispatched",
                "action": unknown.requested,
                "advertised_actions": unknown.advertised,
            }),
        );
    }
    if let Some(disabled) = error.downcast_ref::<ElementDisabled>() {
        return ToolResult::error(disabled.reason()).with_structured(disabled.payload());
    }
    if let Some(reply) = error
        .downcast_ref::<AxActionReplyError>()
        .filter(|reply| reply.element_is_gone())
    {
        return ToolResult::error(format!(
            "The addressed element no longer exists (its accessibility reference is invalid): \
             {}({}) returned {} ({}). Nothing was dispatched. Re-observe the window and \
             re-address the element.",
            "AXUIElementPerformAction",
            reply.action,
            reply.code,
            reply.code_name()
        ))
        .with_structured(serde_json::json!({
            "code": "element_no_longer_exists",
            "effect": "not_dispatched",
            "route": "ax",
            "action": reply.action,
            "ax_error": reply.code,
            "ax_error_name": reply.code_name(),
            "dispatch": "not_dispatched",
            "escalation": { "target": "snapshot", "reason": "route_unavailable" },
        }));
    }
    if let Some(reply) = error.downcast_ref::<AxActionReplyError>() {
        ToolResult::error(format!(
            "AX action outcome is unknown: {error}. The action was attempted and may \
             already have taken effect. Inspect fresh state or the saved result before \
             deciding whether to retry; do not repeat the action solely because of this reply."
        ))
        .with_structured(serde_json::json!({
            "error": "ActionOutcomeUnknown",
            "path": "ax",
            "effect": "unverifiable",
            "dispatch": "attempted",
            "action": reply.action,
            "ax_error": reply.code,
            "retry": "reconcile_first"
        }))
    } else {
        ToolResult::error(format!("AX action failed: {error}"))
    }
}

/// The rung that can still deliver after a background click produced no
/// observable change. Measured on a Chromium control whose handler runs off
/// `pointerdown` (probe page, window not frontmost):
///
/// | rung                                   | pointer events |
/// |----------------------------------------|----------------|
/// | AX press (background or foreground)    | never          |
/// | PID-routed CGEvent (background)        | never          |
/// | HID at the element's pixel center (fg) | yes            |
///
/// So an AX press escalates to a *pixel* target — fronting the app does not
/// give `AXUIElementPerformAction` pointer events — while a pixel click that
/// was routed to the pid escalates to foreground delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextRung {
    PixelForeground,
    Foreground,
}

impl NextRung {
    fn recommended(self) -> &'static str {
        match self {
            NextRung::PixelForeground => "px",
            NextRung::Foreground => "foreground",
        }
    }

    fn escalation(self) -> EscalationKind {
        match self {
            NextRung::PixelForeground => EscalationKind::RetryWithPixelTarget,
            NextRung::Foreground => EscalationKind::RetryWithForegroundDelivery,
        }
    }

    fn advice(self) -> &'static str {
        match self {
            NextRung::PixelForeground => {
                "An AX press never carries pointer events, and fronting the app does not change \
                 that. To deliver a real click, take a fresh get_window_state screenshot and \
                 click this control's pixel center with delivery_mode:\"foreground\". Take that \
                 centre from the control's own frame, not by eye off the image: a capture that \
                 also holds an open popover or menu covers more than the window, so its pixels \
                 are not the window's points."
            }
            NextRung::Foreground => {
                "A PID-routed click carries no trusted pointer events. Re-run this same pixel \
                 click with delivery_mode:\"foreground\" so macOS delivers it at the HID tap."
            }
        }
    }
}

/// One option of a popup/select control, as the reply lists it.
///
/// # Safety
/// `element` must be a live AX element.
unsafe fn popup_option_label(element: AXUIElementRef) -> Option<String> {
    let title = copy_string_attr(element, "AXTitle").unwrap_or_default();
    let value = copy_string_attr(element, "AXValue").unwrap_or_default();
    if title.is_empty() && value.is_empty() {
        return None;
    }
    Some(if value.is_empty() || value == title {
        format!("\"{title}\"")
    } else {
        format!("\"{title}\" (value: {value})")
    })
}

/// The options an `AXPopUpButton` offers.
///
/// A popup's options are the items of its `AXMenu`, not its direct children:
/// once the menu is open the button has exactly one child, a titleless
/// `AXMenu`, and reading only direct children reported no options at all —
/// so the advice that names them never reached a caller who had just opened
/// one.
///
/// # Safety
/// `element` must be a live AX element; every child copied here is released.
unsafe fn popup_option_labels(element: AXUIElementRef) -> Vec<String> {
    let mut options = Vec::new();
    for child in copy_children(element) {
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            for item in copy_children(child) {
                options.extend(popup_option_label(item));
                CFRelease(item as _);
            }
        } else {
            options.extend(popup_option_label(child));
        }
        CFRelease(child as _);
    }
    options
}

/// Why a dispatch the app never reacted to may still have been delivered.
/// Names the signals the probe compared, which is the whole scope of its
/// silence.
fn noop_reason(compared: &[&str]) -> String {
    format!(
        "no observable change after background delivery; the probe compared \
         {} only, so an effect it cannot see is still possible",
        compared.join(", ")
    )
}

/// The one mechanism measured behind a silent background click
/// (`delivery_probe`'s table): background delivery produces no trusted
/// pointer events, so a control that acts on `pointerdown` never hears it.
/// Stated as a candidate for every target, because the probe observed the
/// silence, not its cause — the app's bundle family says which UI toolkit
/// drew the control, never what this control listens for.
const POINTERDOWN_NOTE: &str = " Background delivery carries no trusted pointer events, \
     so a control that acts on pointerdown would not have seen it.";

/// The post-dispatch probe's verdict together with what the reply needs to
/// say about it: the rung that can still deliver when nothing reacted, and
/// whether the caller watched for windows opening.
#[derive(Clone, Copy)]
struct ProbeReport {
    outcome: delivery_probe::ProbeOutcome,
    rung: NextRung,
    polled: bool,
}

impl ProbeReport {
    fn noop_reason(&self) -> String {
        noop_reason(&self.outcome.watched.compared(self.polled))
    }
}

fn apply_delivery_evidence(
    msg: &mut String,
    structured: &mut serde_json::Value,
    report: ProbeReport,
    window_change: Option<&delivery_probe::WindowChangeEvidence>,
) {
    let advice = format!("{POINTERDOWN_NOTE} {}", report.rung.advice());
    delivery_probe::apply_evidence(
        msg,
        structured,
        report.outcome,
        delivery_probe::NoopReport {
            polled: report.polled,
            escalation: Some(serde_json::json!({
                "recommended": report.rung.recommended(),
                "reason": report.noop_reason(),
            })),
            advice: &advice,
        },
        window_change,
    );
}

fn pixel_activation_policy(
    button: &str,
    effective_foreground: bool,
    has_window: bool,
) -> PixelActivationPolicy {
    if effective_foreground {
        PixelActivationPolicy::ForegroundAssist
    } else if button == "left" && has_window {
        PixelActivationPolicy::AllowTargetWithoutRaise
    } else {
        PixelActivationPolicy::SuppressTarget
    }
}

/// Return the prior foreground pid that should be restored after a raw
/// background pixel click.
///
/// This decision deliberately depends on observed application state rather
/// than the private focus recipe's return value. The recipe can be unavailable
/// or partially fail while the raw click still makes the target AppKit-active;
/// in that case the allow-target suppression lease will not restore it for us.
fn background_pixel_restore_pid(
    activation_policy: PixelActivationPolicy,
    prior_front: Option<i32>,
    target_pid: i32,
    observed_front: Option<i32>,
) -> Option<i32> {
    if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise
        && prior_front != Some(target_pid)
        && observed_front == Some(target_pid)
    {
        prior_front
    } else {
        None
    }
}

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "click".into(),
        description:
            "Click against a target pid. **Prefer `element_token` over pixel \
             coordinates** — the token works on backgrounded / minimized / hidden / \
             off-Space windows, identifies one exact snapshot element, and tells \
             you what you're clicking via the cached element's role + label. Reach for \
             `x, y` only when the target is a canvas / video / WebGL / custom-drawn surface \
             that doesn't appear in the AX tree.\n\n\
             Two addressing modes:\n\n\
             - element_token, or element_index + snapshot_id (from get_window_state): AX action path. \
               Works on backgrounded/hidden windows. No cursor move, no focus steal. \
               The snapshot cache is scoped per (pid, window_id) and is replaced by the \
               next snapshot of the same window — re-snapshot every turn before clicking.\n\n\
             - x, y (window-local screenshot pixels, top-left origin of the PNG returned \
               by get_window_state): CGEvent path. Synthesizes mouse events and posts to \
               pid. Use modifier for cmd/shift/option/ctrl. Needs a visible on-screen \
               window to anchor the conversion.\n\n\
             button: \"left\" (default), \"right\", or \"middle\". Defaults to left so the \
             field is fully back-compat — omit it and you get the legacy left-click behaviour. \
             Pixel path: routes through the CGEvent left/right/middle mouse-button primitives. \
             AX path: \"right\" maps to AXShowMenu (same surface as the dedicated `right_click` \
             tool); \"middle\" has no AX equivalent and falls back to a pixel middle-click at the \
             element's center.\n\
             action: press (default), show_menu, pick, confirm, cancel, open.\n\
             from_zoom: set true after a zoom call to auto-translate zoom-image pixel \
             coordinates to full-window space."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            // `pid` is conditionally required — needed for window/element clicks
            // but omitted for windowless `scope:"desktop"` clicks — so it is NOT
            // in `required`; the code validates it with a clear error when needed.
            // (Keeps the contract consistent across platforms; see
            // cua_driver_core::tool_schema.)
            "required": [],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid":           { "type": "integer", "description": "Target process ID." },
                "window_id":     { "type": "integer", "description": "Target window ID. Required for element_index. Optional when element_token is supplied (the token carries it)." },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "x":             { "type": "number",  "description": "X in screenshot pixels. A window target uses the get_window_state PNG; a desktop target uses the native get_desktop_state PNG. The driver reverses Retina backing scale and any window-image downscale." },
                "y":             { "type": "number",  "description": "Y in screenshot pixels from the image selected by target." },
                "action":        { "type": "string",  "description": "AX action: press, show_menu, pick, confirm, cancel, open. Any other value must be an action name the element itself advertises — an AX name from the element's `actions`, or the `name` or `raw` string of one of its `custom_actions`; anything else is refused instead of dispatched as a press." },
                "button":        {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button. Default: \"left\" — omit for legacy left-click behaviour. Pixel path uses the matching CGEvent primitive; AX path maps \"right\" to AXShowMenu and falls back to a pixel middle-click at the element's center for \"middle\"."
                },
                "count":         { "type": "integer", "description": "Click count. Default 1. An element token supports a left-button double-click (count 2) at its live bounding-box center; other counted semantic actions are refused." },
                "modifier": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys: cmd, shift, option/alt, ctrl."
                },
                "from_zoom": {
                    "type": "boolean",
                    "description": "When true, x and y are in the last zoom image for this pid; driver translates back to full-window coordinates."
                },
                "debug_image_out": {
                    "type": "string",
                    "description": "Optional file path. When set on a pixel-addressed click, captures a fresh screenshot, draws a red crosshair at (x, y), and writes the PNG. Use to verify coordinate spaces. Requires window_id; incompatible with from_zoom."
                },
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "Best-effort-background ladder rung (default \"background\"). \"background\": perform the AX action or post the CGEvent without fronting. \"foreground\": briefly front the window, act, let transient UI settle, then restore the prior frontmost app. Requires window_id. Modified clicks require \"foreground\" so macOS observes physical modifier-key state. A generic click has no independent postcondition read-back, except selection of list-like AX rows whose AXSelected state can be confirmed; otherwise confirm the effect from a fresh state snapshot. Use the agent loop: background AX (element_index) → snapshot → background pixel (x/y) → snapshot → delivery_mode:\"foreground\"."
                },
                "scope": {
                    "type": "string",
                    "enum": ["window", "desktop"],
                    "description": "Coordinate frame for a windowless screen-absolute click (default \"window\"). Pass \"desktop\" when sending x,y with NO pid/window_id — the coordinates are then true screen pixels (read from get_desktop_state with scope=\"desktop\"). Per-call; not a setting."
                },
                "detect_window_change": { "type": "boolean", "description": "Default true: after the action the driver polls WindowServer for up to one second so the reply can name a window the action opened. Pass false when you enumerate windows yourself — the poll is then skipped (roughly a second off this call) and the reply carries no opened-window evidence." },
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  false,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for ClickTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;

        // ── Window-less screen-absolute branch (scope="desktop") ──────
        // x,y given with NO pid and NO window_id → the coordinates are TRUE
        // SCREEN pixels. This is the foreground, vision-driven desktop-scope
        // path, the macOS peer of the Windows WindowFromPoint click. Gate on the
        // effective scope: under "window" return a structured
        // `desktop_scope_disabled` error (same contract as Windows) rather than
        // silently treating window-local pixels as screen pixels.
        let has_pid = args.get("pid").map(|v| !v.is_null()).unwrap_or(false);
        let has_window_id = args.get("window_id").map(|v| !v.is_null()).unwrap_or(false);
        let has_xy = args.get("x").map(|v| v.is_number()).unwrap_or(false)
            && args.get("y").map(|v| v.is_number()).unwrap_or(false);
        if has_xy && !has_pid && !has_window_id {
            // `scope` is a per-call param now (default "window"); pass
            // scope="desktop" to enable screen-absolute clicks.
            let scope = args.str_or("scope", "window");
            if scope != "desktop" {
                return ToolResult::error(
                    "click: x,y given with no pid/window_id, but scope is \"window\". \
                     Screen-absolute clicks require desktop scope. Pass scope=\"desktop\" \
                     (and use get_desktop_state with scope=\"desktop\" to read true \
                     screen pixels) first."
                        .to_string(),
                )
                .with_structured(serde_json::json!({
                    "code": "desktop_scope_disabled",
                    "scope": scope,
                    "suggestion": "pass scope=\"desktop\"",
                }));
            }
            let input = match parse_legacy_click_input(&args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let sx_shot = input.x;
            let sy_shot = input.y;
            let (sx, sy) = match super::desktop_screenshot_point(sx_shot, sy_shot).await {
                Ok(point) => point,
                Err(error) => return error,
            };
            let button = match input.button.unwrap_or(ClickButton::Left) {
                ClickButton::Left => "left",
                ClickButton::Right => "right",
                ClickButton::Middle => "middle",
            }
            .to_owned();
            let count = input.count.unwrap_or(1) as usize;
            if count == 0 {
                return ToolResult::error("click.count must be at least 1.")
                    .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
            }
            // Glide the session's agent cursor to the screen point for visibility.
            let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), sx, sy).await;
            self.state
                .cursor_registry
                .update_position(&cursor_key, sx, sy);
            // Press edge for the agent-cursor overlay (renders a click pulse on
            // viewers via the cursor hook).
            self.state.cursor_registry.note_press(&cursor_key, sx, sy);

            let btn = button.clone();
            let desktop_modifiers: Vec<String> = args.str_array("modifier");
            let result =
                cua_driver_core::operation::spawn_blocking(move || -> anyhow::Result<()> {
                    // Desktop scope is explicitly foreground and vision-driven: post
                    // at the global HID tap so WindowServer delivers to the window
                    // actually visible at this point. PID-posting here would silently
                    // turn the foreground contract back into background delivery.
                    let modifier_refs: Vec<&str> =
                        desktop_modifiers.iter().map(String::as_str).collect();
                    crate::input::mouse::click_at_xy_desktop_with_modifiers(
                        sx,
                        sy,
                        count,
                        &btn,
                        &modifier_refs,
                    )
                })
                .await;
            let button_label = match button.as_str() {
                "right" => "right-click",
                "middle" => "middle-click",
                _ => "click",
            };
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Sent screen-absolute {button_label} at desktop-pixel \
                     ({sx_shot:.0},{sy_shot:.0}) → screen-point ({sx:.0},{sy:.0}) \
                     (desktop scope; not driver-verified)."
                ))
                .with_structured(serde_json::json!({ "path": "cgevent_hid", "verified": false, "effect": "unverifiable" })),
                Ok(Err(e)) => ToolResult::error(format!("desktop-scope click failed: {e}")),
                Err(e) => ToolResult::error(format!("task error: {e}")),
            };
        }

        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Resolve this action's cursor key so its click-pulse / glide land on
        // the calling session's cursor, not the shared "default" one.
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);

        // Surface 6: resolve element_token / element_index precedence
        // BEFORE the pixel-path fallback. Token wins on disagreement; a
        // stale token returns an explicit error instead of silently
        // falling back to the integer (Surface 6 hard constraint).
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id");
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match self.state.element_cache.resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "click",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let x = args
            .opt_f64("x")
            .or_else(|| args.opt_i64("x").map(|i| i as f64));
        let y = args
            .opt_f64("y")
            .or_else(|| args.opt_i64("y").map(|i| i as f64));
        let action_named_by_caller = args.opt_str("action").is_some();
        let action = args.str_or("action", "press");
        // Surface 5: optional `button` arg, default "left" preserves legacy behaviour.
        // Pixel path: routes to left/right/middle CGEvent primitives.
        // AX path: "right" delegates to AXShowMenu (same surface as right_click);
        // "middle" has no AX equivalent and falls back to a pixel middle-click
        // at the element's screen-space center.
        let button_str = args.str_or("button", "left").to_lowercase();
        // delivery_mode: per-call ladder rung. Foreground briefly activates the
        // target for both AX and pixel paths, then restores the prior app.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        // Reject unknown buttons explicitly so silent left-click fall-through can't
        // mask a typo. Keep "" → default left for old clients that never sent the field.
        if !matches!(button_str.as_str(), "" | "left" | "right" | "middle") {
            return ToolResult::error(format!(
                "click: unknown button \"{button_str}\" — expected one of left, right, middle."
            ));
        }
        let button_str = if button_str.is_empty() {
            "left".to_string()
        } else {
            button_str
        };
        let count = args.u64_or("count", 1) as usize;
        let from_zoom = args.bool_or("from_zoom", false);
        let debug_image_out = args.opt_str("debug_image_out");
        let modifiers: Vec<String> = args.str_array("modifier");

        // PID-routed key transitions can look correct for one AX poll and then
        // collapse to a plain click once AppKit resolves the gesture. Refuse
        // that false-success path. The explicit foreground rung uses physical
        // HID modifier transitions under an exact-window activation guard.
        if !modifiers.is_empty() && (!delivery_mode.is_foreground() || window_id.is_none()) {
            return ToolResult::error(
                "click modifiers require delivery_mode:\"foreground\" and window_id on macOS; \
                 background PID-routed events cannot preserve live modifier-key state",
            )
            .with_structured(serde_json::json!({
                "code": "background_unavailable",
                "effect": "refused",
                "escalation": {
                    "recommended": "foreground",
                    "reason": "macOS modifier clicks require exact-window HID delivery so the target observes live modifier state"
                }
            }));
        }

        if count == 0
            || (element_index.is_some()
                && count != 1
                && (count != 2
                    || button_str != "left"
                    || action != "press"
                    || !modifiers.is_empty()))
        {
            return ToolResult::error("Element click supports one semantic action or an unmodified left double-click (count 2); count must be positive.")
                .with_structured(serde_json::json!({"code":"unsupported_click_gesture", "effect":"not_dispatched"}));
        }
        // AXPress has no click count. Keep the retained snapshot element and
        // exact-window mutation lease while routing a double-click through the
        // same pixel delivery and cancellation machinery as coordinate clicks.
        let indexed_guard = if let (Some(_), Some(_), 2) = (element_index, window_id, count) {
            element_guard.clone()
        } else {
            None
        };
        let indexed_lease = if let Some(element) = &indexed_guard {
            if from_zoom || debug_image_out.is_some() {
                return ToolResult::error("Element double-click does not use zoom coordinates or debug_image_out; click was not dispatched.");
            }
            match super::gate_background_window_action(
                pid,
                window_id.unwrap(),
                Some(element.as_ptr()),
                cua_driver_core::background_input::BackgroundAction::WindowPointer,
            )
            .await
            {
                Ok(lease) => Some(lease),
                Err(refusal) => return refusal,
            }
        } else {
            None
        };
        let indexed_point = if let Some(element) = &indexed_guard {
            let pointer = element.as_ptr();
            let wid = window_id.unwrap();
            match cua_driver_core::operation::spawn_blocking(move || {
                live_element_click_point(pointer, wid)
            })
            .await
            {
                Ok(Ok(point)) => Some(point),
                Ok(Err(error)) => {
                    return ToolResult::error(format!(
                        "Element double-click refused before dispatch: {error}"
                    ))
                }
                Err(error) => {
                    return ToolResult::error(format!(
                        "Element geometry lookup failed before dispatch: {error}"
                    ))
                }
            }
        } else {
            None
        };

        if let (Some(idx), Some(wid), Some(element_guard), None) =
            (element_index, window_id, element_guard, indexed_point)
        {
            // ── AX element path ────────────────────────────────────────────
            let element_ptr = element_guard.as_ptr();

            // ── Exact-target background gate (macOS background input v1) ──
            // The element branch is semantic AX delivery, except button=middle
            // which falls back to a routed pixel click at the element's center
            // and is therefore held to the stricter WindowPointer rung. Gate
            // BEFORE any cursor/dispatch work so a stale or sibling-owned
            // target refuses instead of acting on the wrong window.
            let _mutation_lease = if !delivery_mode.is_foreground() {
                let gate_action = if button_str == "middle" {
                    cua_driver_core::background_input::BackgroundAction::WindowPointer
                } else {
                    cua_driver_core::background_input::BackgroundAction::AxSemantic
                };
                match super::gate_background_window_action(pid, wid, Some(element_ptr), gate_action)
                    .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            };

            // Surface 5: button=right on the AX path → AXShowMenu (the same surface
            // the dedicated `right_click` tool dispatches). Threads through the
            // identical perform_ax_click code path with the action remapped.
            let effective_action = if button_str == "right" && action == "press" {
                "show_menu".to_string()
            } else {
                action.clone()
            };

            if !delivery_mode.is_foreground() && requests_ax_action(&effective_action, "AXOpen") {
                let panel = cua_driver_core::operation::spawn_blocking(move || {
                    crate::ax::exact_target::is_file_panel_element(element_ptr)
                })
                .await;
                match panel {
                    Ok(false) => {},
                    Ok(true) => return ToolResult::error(
                        "AppKit file-panel AXOpen can activate the client application, so background delivery was refused before dispatch. Select the file with a single click, then take a fresh panel snapshot and use its enabled Import/Open button to commit the selection."
                    ).with_structured(serde_json::json!({
                        "code":"file_panel_open_requires_foreground", "dispatched":false,
                        "effect":"not_dispatched", "recommended":"select_then_commit"
                    })),
                    Err(error) => return ToolResult::error(format!("File-panel action provenance failed before dispatch: {error}")),
                }
            }

            // Animate cursor to element center BEFORE firing AX action,
            // mirroring Swift's `performElementClick` → `animateAndWait(to:)`.
            let center_guard = element_guard.clone();
            let center = cua_driver_core::operation::spawn_blocking(move || unsafe {
                crate::ax::bindings::element_screen_center(center_guard.as_ptr() as AXUIElementRef)
            })
            .await
            .ok()
            .flatten();

            // Surface 5: button=middle on the AX path has no AX equivalent.
            // Fall back to a pixel middle-click at the element's screen-space center
            // so the request still produces a real middle-button event (browser tab
            // close, autoscroll, etc.). If we can't resolve a center, error rather
            // than silently degrade to AXPress.
            if button_str == "middle" {
                let (cx, cy) = match center {
                    Some(c) => c,
                    None => {
                        return ToolResult::error(
                            "click(button=middle) on element_index: could not resolve element \
                         center for the pixel-middle-click fallback. Pass x, y directly.",
                        )
                    }
                };
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), cx, cy).await;
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, cx, cy);
                self.state.cursor_registry.note_press(&cursor_key, cx, cy);

                let mods_owned = modifiers.clone();
                let foreground = delivery_mode.is_foreground();
                let result = cua_driver_core::operation::spawn_blocking(move || {
                    let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                    if foreground && !m.is_empty() {
                        crate::input::skylight::with_foreground_hid_activation(
                            pid as libc::pid_t,
                            wid,
                            || {
                                crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                                    cx, cy, 1, "middle", &m,
                                )
                            },
                        )
                    } else {
                        crate::input::mouse::middle_click_at_xy(pid, cx, cy, &m)
                    }
                })
                .await;
                return match result {
                    Ok(Ok(())) => ToolResult::text(format!(
                        "✅ Posted middle-click to pid {pid} at element [{idx}] center \
                         (background CGEvent; not driver-verified — confirm via screenshot)."
                    ))
                    .with_structured(serde_json::json!({ "path": "cgevent", "verified": false, "effect": "unverifiable" })),
                    Ok(Err(e)) => ToolResult::error(format!("Middle-click failed: {e}")),
                    Err(e)     => ToolResult::error(format!("Task error: {e}")),
                };
            }

            if let Some((cx, cy)) = center {
                // Pin overlay above target window first.
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), cx, cy).await;
                // Keep the registry in sync with the overlay so
                // get_agent_cursor_state reports a truthful position even when
                // the click was dispatched via the AX path (no pixel coords).
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, cx, cy);
                self.state.cursor_registry.note_press(&cursor_key, cx, cy);
            }

            // Finder icon/list items can expose a readable AXSelected state
            // while refusing both AXSelected writes and AXPress. Resolve a
            // verified coordinate frame only for those collection-like
            // elements so perform_ax_click can cross that one failed semantic
            // rung internally and confirm the result by AX read-back.
            let selection_candidate = if effective_action == "press" {
                let selection_guard = element_guard.clone();
                cua_driver_core::operation::spawn_blocking(move || {
                    crate::input::ax_actions::nearest_container_selection_state(
                        selection_guard.as_ptr(),
                    )
                    .is_some()
                })
                .await
                .unwrap_or(false)
            } else {
                false
            };
            // `show_menu` needs the same coordinate frame: AXShowMenu can
            // report success without opening a menu, and only a real pointer
            // right-click at the element centre reaches one then.
            let mut element_pixel = if selection_candidate || effective_action == "show_menu" {
                if let Some((cx, cy)) = center {
                    super::px_frame::resolve_or_refuse(wid)
                        .await
                        .ok()
                        .map(|frame| SelectionPixelTarget {
                            screen_x: cx,
                            screen_y: cy,
                            window_x: cx - frame.bounds.x,
                            window_y: cy - frame.bounds.y,
                        })
                } else {
                    None
                }
            } else {
                None
            };
            // Both fallbacks deliver a routed window-local pixel event — a
            // stricter (WindowPointer) rung than the semantic gate above. In
            // background, drop the fallback rather than silently escalate when
            // the pointer rung would refuse (e.g. a minimized/hidden target);
            // the semantic path still runs.
            if element_pixel.is_some()
                && !delivery_mode.is_foreground()
                && _mutation_lease
                    .as_ref()
                    .expect("background element actions hold the per-pid lease")
                    .gate_again(
                        wid,
                        Some(element_ptr),
                        cua_driver_core::background_input::BackgroundAction::WindowPointer,
                    )
                    .await
                    .is_err()
            {
                element_pixel = None;
            }
            let selection_pixel = selection_candidate.then_some(element_pixel).flatten();
            let menu_pixel = (effective_action == "show_menu")
                .then_some(element_pixel)
                .flatten();

            // Background delivery cannot produce trusted pointer events, so a
            // control driven by pointerdown ignores this press while AX still
            // replies success. Sample the target now so the reply can say
            // whether anything reacted instead of asserting a hollow success.
            let probe = if delivery_mode.is_foreground() {
                None
            } else {
                cua_driver_core::operation::spawn_blocking(move || {
                    delivery_probe::DeliveryProbe::capture(pid, wid, Some(element_ptr))
                })
                .await
                .ok()
            };

            // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
            // Capture prior frontmost, arm the wildcard suppressor in the
            // snapshot, then arm a targeted suppressor across the AX action
            // itself via FocusGuard. After the action returns, detect any
            // new-window / foreground side-effects and append a one-liner
            // suffix matching Swift's wording.
            let prior_front = apps::frontmost_pid();
            let foreground = delivery_mode.is_foreground();
            let snapshot = if foreground {
                WindowChangeDetector::snapshot_without_suppression(prior_front)
            } else {
                WindowChangeDetector::snapshot(prior_front)
            };

            // Run AX work on a blocking thread (can't block async executor).
            // Use `effective_action` so button=right rewrites press → show_menu.
            let action_clone = effective_action.clone();
            // Thread the resolved session cursor key into the blocking AX path
            // so its ShowFocusRect + ClickPulse land on THIS session's cursor,
            // not the shared "default" one (which would light the wrong cursor
            // and stomp default for a non-default session).
            let ck = cursor_key.clone();
            let selection_modifiers = modifiers.clone();
            let result = focus_guard::with_focus_suppressed(
                if foreground { None } else { Some(pid) },
                prior_front,
                "click.AXPress",
                || async move {
                    cua_driver_core::operation::spawn_blocking(move || {
                        let element_ptr = element_guard.as_ptr();
                        if foreground {
                            let mut outcome = None;
                            let has_modifiers = !selection_modifiers.is_empty();
                            let action = || {
                                outcome = Some(perform_ax_click(
                                    element_ptr,
                                    idx,
                                    pid,
                                    wid,
                                    RequestedAction {
                                        action: &action_clone,
                                        named_by_caller: action_named_by_caller,
                                    },
                                    &ck,
                                    selection_pixel,
                                    menu_pixel,
                                    &selection_modifiers,
                                    foreground,
                                )?);
                                std::thread::sleep(std::time::Duration::from_millis(150));
                                Ok(())
                            };
                            let fronted = if has_modifiers {
                                crate::input::skylight::with_foreground_hid_activation(
                                    pid as libc::pid_t,
                                    wid,
                                    action,
                                )?;
                                true
                            } else {
                                crate::input::skylight::with_foreground_assist(
                                    pid as libc::pid_t,
                                    wid,
                                    action,
                                )?
                            };
                            let outcome = outcome.ok_or_else(|| {
                                anyhow::anyhow!("foreground AX click did not execute")
                            })?;
                            Ok((outcome, fronted))
                        } else {
                            perform_ax_click(
                                element_ptr,
                                idx,
                                pid,
                                wid,
                                RequestedAction {
                                    action: &action_clone,
                                    named_by_caller: action_named_by_caller,
                                },
                                &ck,
                                selection_pixel,
                                menu_pixel,
                                &selection_modifiers,
                                false,
                            )
                            .map(|outcome| (outcome, false))
                        }
                    })
                    .await
                },
            )
            .await;

            // Drop the wildcard lease + detect window/foreground side-effects.
            let changes = super::finish_window_observation(snapshot, &args).await;

            match result {
                Ok(Ok((mut outcome, fronted))) => {
                    // For text inputs, wait 800ms for WebKit DOM focus to settle
                    // before returning — matches the Swift reference behaviour.
                    if outcome.needs_webkit_delay {
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                    let mut msg = std::mem::take(&mut outcome.summary);
                    msg.push_str(&changes.result_suffix());
                    // A window that appeared during the action (sheet, dialog,
                    // popover) is delivery evidence on its own — no second
                    // probe sample needed for it.
                    let evidence = if outcome.selection_verified {
                        None
                    } else if changes.needs_restore() {
                        probe.map(|probe| {
                            probe.settled(delivery_probe::Evidence::Changed(
                                delivery_probe::WINDOW_SIGNAL,
                            ))
                        })
                    } else if let Some(probe) = probe {
                        cua_driver_core::operation::spawn_blocking(move || probe.compare())
                            .await
                            .ok()
                    } else {
                        None
                    };
                    let window_change = if evidence.is_some() && changes.needs_restore() {
                        let appeared = changes.new_windows.clone();
                        cua_driver_core::operation::spawn_blocking(move || {
                            delivery_probe::WindowChangeEvidence::observe(pid, Some(wid), &appeared)
                        })
                        .await
                        .ok()
                    } else {
                        None
                    };
                    let mut structured = ax_click_structured(&outcome, fronted);
                    // The probe contributes the remaining verdicts: `delivered`
                    // when the target reacted, `no_observed_change` when nothing
                    // did.
                    let report = match evidence {
                        Some(delivery) => Some(ProbeReport {
                            outcome: delivery,
                            // The selection fallback already delivered real
                            // pixels; everything else on this branch is an AX
                            // action, which foreground cannot upgrade.
                            rung: if outcome.selection_via_pixel {
                                NextRung::Foreground
                            } else {
                                NextRung::PixelForeground
                            },
                            polled: changes.polled,
                        }),
                        None => None,
                    };
                    if let Some(report) = report {
                        apply_delivery_evidence(
                            &mut msg,
                            &mut structured,
                            report,
                            window_change.as_ref(),
                        );
                    }
                    ToolResult::text(msg)
                        .with_structured(structured)
                        .with_action_record(ax_click_record(&outcome, foreground, report))
                }
                Ok(Err(e)) => ax_action_error(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            }
        } else if let Some((mut cx, mut cy)) = x
            .zip(y)
            .or(indexed_point.map(|point| (point.screen_x, point.screen_y)))
        {
            // ── Pixel path ─────────────────────────────────────────────────

            // debug_image_out: capture fresh screenshot, overlay crosshair BEFORE
            // any coordinate translation (so it shows received coords in the same
            // space the caller was reasoning in).
            if let Some(ref dbg_path) = debug_image_out {
                if from_zoom {
                    return ToolResult::error(
                        "debug_image_out is incompatible with from_zoom — \
                         received (x, y) would be in zoom-crop space, not window-local.",
                    );
                }
                match window_id {
                    None => return ToolResult::error("debug_image_out requires window_id."),
                    Some(wid) => {
                        // Session-effective max dimension so debug_image_out
                        // matches the resize the calling session sees in
                        // get_window_state (precedence: session override > global).
                        let max_dim = self.state.session_config.effective_max_image_dimension(
                            args.opt_str("_session_id").as_deref(),
                            &self.state.config.read().unwrap(),
                        );
                        let dbg_path_c = dbg_path.clone();
                        let dbg_result = cua_driver_core::operation::spawn_blocking(move || {
                            let png = crate::capture::screenshot_window_bytes(wid)?;
                            let png = crate::capture::resize_png_if_needed(&png, max_dim)?;
                            crate::capture::write_crosshair_png(&png, cx, cy, &dbg_path_c)
                        })
                        .await;
                        match dbg_result {
                            Err(e) => {
                                return ToolResult::error(format!(
                                    "debug_image_out task failed: {e}. Not dispatching click."
                                ))
                            }
                            Ok(Err(e)) => {
                                return ToolResult::error(format!(
                                    "debug_image_out write failed: {e}. Not dispatching click."
                                ))
                            }
                            Ok(Ok(())) => {}
                        }
                    }
                }
            }

            if indexed_point.is_some() {
                // Already expressed in logical screen points by fresh AX geometry.
            } else if from_zoom {
                match self.state.zoom_registry.get(pid) {
                    Some(ctx) => {
                        let (wx, wy) = ctx.zoom_to_window(cx, cy);
                        cx = wx;
                        cy = wy;
                    }
                    None => {
                        return ToolResult::error(format!(
                            "from_zoom=true but no zoom context for pid {pid}. Call zoom first."
                        ))
                    }
                }
            } else if let Some(ratio) = self.state.resize_registry.ratio(pid, window_id) {
                // Coordinates are in the downscaled image space; scale back to native pixels.
                cx *= ratio;
                cy *= ratio;
            }

            // ── Window-local → screen coordinate translation ──────────────────
            // `click_at_xy` accepts screen-space coordinates (top-left origin).
            // Callers supply window-local screenshot pixels; `px_frame` adds the
            // window's screen-origin (and divides out the Retina backing scale)
            // to produce the final screen position, or refuses when the window
            // has no live frame — see px_frame's module docs for why there is
            // no screen-absolute fallback.
            //
            // win_local_x/y: window-local logical-pixel coords needed for
            // CGEventSetWindowLocation in the Chromium recipe.
            let (screen_x, screen_y, win_local_x, win_local_y) = if let Some(point) = indexed_point
            {
                (
                    point.screen_x,
                    point.screen_y,
                    point.window_x,
                    point.window_y,
                )
            } else if let Some(wid) = window_id {
                match super::px_frame::resolve_or_refuse(wid).await {
                    Ok(frame) => {
                        let (sx, sy, lx, ly) = frame.to_screen(cx, cy);
                        // A window-local point outside the live frame would
                        // dispatch onto whatever occupies that screen point —
                        // the same wrong-surface misclick class as #2237.
                        // Refuse in background, where the caller cannot see
                        // what is actually under the translated point.
                        if !delivery_mode.is_foreground()
                            && (lx < 0.0
                                || ly < 0.0
                                || lx > frame.bounds.width
                                || ly > frame.bounds.height)
                        {
                            return ToolResult::error(format!(
                                "click: window-local point ({lx:.1}, {ly:.1}) pt lies outside \
                                 window {wid}'s {:.0}×{:.0} pt frame; background delivery \
                                 refused. Re-read coordinates from a fresh get_window_state \
                                 screenshot.",
                                frame.bounds.width, frame.bounds.height
                            ));
                        }
                        (sx, sy, lx, ly)
                    }
                    Err(refusal) => return refusal,
                }
            } else {
                // No window_id → treat x,y as screen coordinates (legacy behaviour).
                (cx, cy, cx, cy)
            };

            // ── Exact-target background gate (macOS background input v1) ──
            // A window-addressed background pixel action targets coordinates,
            // which only mean something while the exact window is current and
            // not minimized/hidden: a stale target would let the pid-scoped
            // hit-test or routed events land on a same-process sibling. Gate
            // BEFORE the AX hit-test backend and any cursor/dispatch work.
            // delivery_mode:"foreground" stays the explicit last resort.
            let mutation_lease_held = crate::background_mutation::held_by_current_task(pid);
            let _mutation_lease = if !delivery_mode.is_foreground()
                && !mutation_lease_held
                && indexed_lease.is_none()
            {
                if let Some(wid) = window_id {
                    match super::gate_background_window_action(
                        pid,
                        wid,
                        None,
                        cua_driver_core::background_input::BackgroundAction::WindowPointer,
                    )
                    .await
                    {
                        Ok(lease) => Some(lease),
                        Err(refusal_result) => return refusal_result,
                    }
                } else {
                    None
                }
            } else {
                None
            };

            // A background PX action can still use an accessibility delivery
            // backend after resolving the requested screen point. This keeps
            // targeting (PX) orthogonal to delivery (AX) and avoids making a
            // Chromium/AppKit window key merely to satisfy first-mouse rules.
            if !delivery_mode.is_foreground()
                && window_id.is_some()
                && button_str == "left"
                && count == 1
                && modifiers.is_empty()
            {
                let focus_only = action == "focus";
                let hit_test_wid = window_id.expect("guarded by window_id.is_some() above");
                let ax_result = cua_driver_core::operation::spawn_blocking(move || unsafe {
                    let Some(element) = element_at_screen_position(pid, screen_x, screen_y) else {
                        return Ok::<bool, anyhow::Error>(false);
                    };
                    // The pid-scoped hit-test can resolve an element from a
                    // same-process sibling overlapping the requested point.
                    // Require proven ancestry in the requested window before
                    // acting; otherwise fall through to the routed pixel path
                    // (already gated for this exact window).
                    if crate::ax::exact_target::element_window_id(element) != Some(hit_test_wid) {
                        CFRelease(element as _);
                        return Ok(false);
                    }
                    let delivered = if focus_only {
                        crate::input::ax_actions::focus_element(element as usize).is_ok()
                    } else {
                        let press = core_foundation::string::CFString::new("AXPress");
                        AXUIElementPerformAction(element, press.as_concrete_TypeRef())
                            == kAXErrorSuccess
                    };
                    CFRelease(element as _);
                    Ok(delivered)
                })
                .await;
                match ax_result {
                    Ok(Ok(true)) => {
                        let label = if focus_only { "focused" } else { "pressed" };
                        return ToolResult::text(format!(
                            "✅ PX hit-test {label} the background element via AX."
                        ))
                        .with_structured(serde_json::json!({
                            "path": "ax",
                            "verified": false,
                            "effect": "unverifiable"
                        }));
                    }
                    Ok(Ok(false)) if focus_only => {
                        return ToolResult::error(
                            "Background PX focus is unavailable at the requested point.".to_owned(),
                        )
                        .with_structured(serde_json::json!({
                            "code": "background_unavailable"
                        }));
                    }
                    Ok(Err(error)) if focus_only => {
                        return ToolResult::error(format!("Background PX focus failed: {error}"))
                            .with_structured(serde_json::json!({
                                "code": "background_unavailable"
                            }));
                    }
                    _ => {}
                }
            }

            // Resolve the effective delivery posture before observation. A
            // requested foreground click without a window id still degrades to
            // background, matching the existing contract and result label.
            let fg = delivery_mode.is_foreground() && window_id.is_some();
            let activation_policy = pixel_activation_policy(&button_str, fg, window_id.is_some());

            // Pin the overlay above the target window BEFORE animating so
            // the cursor is already sandwiched correctly while it glides in.
            if let Some(wid) = window_id {
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
            }
            // Animate the visual cursor to the click point and wait for it to
            // arrive — mirrors Swift's `AgentCursor.shared.animateAndWait(to:)`.
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y).await;
            // Keep the registry in sync with the overlay (see AX path above).
            self.state
                .cursor_registry
                .update_position(&cursor_key, screen_x, screen_y);
            self.state
                .cursor_registry
                .note_press(&cursor_key, screen_x, screen_y);

            // Cursor animation can yield to a re-render or window move. Never
            // apply an element gesture using the point resolved before that yield.
            if let (Some(element), Some(lease), Some(point)) =
                (&indexed_guard, &indexed_lease, indexed_point)
            {
                let wid = window_id.unwrap();
                let pointer = element.as_ptr();
                if let Err(refusal) = lease
                    .gate_again(
                        wid,
                        Some(pointer),
                        cua_driver_core::background_input::BackgroundAction::WindowPointer,
                    )
                    .await
                {
                    return refusal;
                }
                match cua_driver_core::operation::spawn_blocking(move || live_element_click_point(pointer, wid)).await {
                    Ok(Ok(current)) if current == point => {},
                    _ => return ToolResult::error("Element or window geometry changed before double-click; nothing was dispatched. Observe fresh state before retrying.")
                        .with_structured(serde_json::json!({"code":"stale_element_geometry", "effect":"not_dispatched"})),
                }
            }

            // Element-addressed background clicks get the same delivery probe as
            // the AX path: the pointer-event gap is a property of background
            // delivery, not of the AX route. Pure pixel targets stay out — the
            // caller picked those coordinates off an image and owns the check.
            let probe = match (indexed_guard.as_ref(), window_id, fg) {
                (Some(element), Some(wid), false) => {
                    let pointer = element.as_ptr();
                    cua_driver_core::operation::spawn_blocking(move || {
                        delivery_probe::DeliveryProbe::capture(pid, wid, Some(pointer))
                    })
                    .await
                    .ok()
                }
                _ => None,
            };

            // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
            // A pixel click can land on a "Sign In" button that opens a sheet
            // or a Safari link that activates a new tab — same side-effect
            // shape as the AX path, so we wrap identically.
            let prior_front = apps::frontmost_pid();
            let snapshot = match activation_policy {
                PixelActivationPolicy::SuppressTarget => {
                    WindowChangeDetector::snapshot(prior_front)
                }
                PixelActivationPolicy::AllowTargetWithoutRaise => {
                    WindowChangeDetector::snapshot_allowing_activation(prior_front, pid)
                }
                PixelActivationPolicy::ForegroundAssist => {
                    WindowChangeDetector::snapshot_without_suppression(prior_front)
                }
            };

            // Restore the Swift background-click prologue that was left
            // disconnected in the original Rust port. It makes an opaque
            // target AppKit-active without raising/restacking its window, which
            // is required by Chromium gates and remote-HID proxies such as
            // iPhone Mirroring. Re-pin after the focus record because changing
            // AppKit active state can disturb overlay ordering.
            let focus_without_raise =
                if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise {
                    let wid = window_id.expect("activation policy requires window_id");
                    match cua_driver_core::operation::spawn_blocking(move || {
                        crate::input::mouse::prepare_background_pixel_click(pid, wid)
                    })
                    .await
                    {
                        Ok(activated) => {
                            crate::cursor::overlay::send_command(
                                cursor_key.clone(),
                                cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                            );
                            activated
                        }
                        Err(error) => {
                            return ToolResult::error(format!(
                                "Background click activation task failed: {error}"
                            ));
                        }
                    }
                } else {
                    false
                };

            // Pulse only after the activation settle so it visually coincides
            // with the real target click rather than the private focus prelude.
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::ClickPulse {
                    x: screen_x,
                    y: screen_y,
                },
            );

            let mods_owned = modifiers.clone();
            // Surface 5: route to the right/middle CGEvent primitives when
            // button != left. Left-button path stays on the existing Chromium-
            // routed `click_at_xy_with_window_local` for back-compat.
            let button_kind = button_str.clone();
            let indexed_pointer = indexed_guard.as_ref().map(|element| element.as_ptr());
            let result = focus_guard::with_focus_suppressed(
                if activation_policy == PixelActivationPolicy::SuppressTarget {
                    Some(pid)
                } else {
                    None
                },
                prior_front,
                "click.pixel",
                || async move {
                    cua_driver_core::operation::spawn_blocking(move || {
                        let has_modifiers = !mods_owned.is_empty();
                        let do_click = move || -> anyhow::Result<()> {
                            if let (Some(pointer), Some(point), Some(wid)) = (indexed_pointer, indexed_point, window_id) {
                                anyhow::ensure!(live_element_click_point(pointer, wid)? == point, "Element geometry changed during click preparation; input was not dispatched");
                            }
                            let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                            if fg && !m.is_empty() {
                                return crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                                    screen_x,
                                    screen_y,
                                    count,
                                    &button_kind,
                                    &m,
                                );
                            }
                            match button_kind.as_str() {
                                "right" => {
                                    if let Some(wid) = window_id {
                                        return crate::input::mouse::right_click_at_xy_with_window_local(
                                            pid, screen_x, screen_y, win_local_x, win_local_y, wid, &m,
                                        );
                                    }
                                    crate::input::mouse::right_click_at_xy(pid, screen_x, screen_y, &m)
                                }
                                "middle" => {
                                    if let Some(_wid) = window_id {
                                        return crate::input::mouse::middle_click_at_xy_with_window_local(
                                            pid, screen_x, screen_y, win_local_x, win_local_y, &m,
                                        );
                                    }
                                    crate::input::mouse::middle_click_at_xy(pid, screen_x, screen_y, &m)
                                }
                                // "left" (default) or anything else — preserve legacy left-click path.
                                _ => {
                                    // When we know the window_id, pass the window-local coordinates so
                                    // `click_at_xy_with_window_local` can stamp `CGEventSetWindowLocation`
                                    // and Chromium-specific fields (f40, f51, f58, f91, f92) onto events
                                    // for better backgrounded-target delivery.
                                    if let Some(wid) = window_id {
                                        return crate::input::mouse::click_at_xy_with_window_local(
                                            pid, screen_x, screen_y,
                                            win_local_x, win_local_y,
                                            wid, count, &m,
                                            crate::input::mouse::WindowClickDelivery::from_foreground(fg),
                                        );
                                    }
                                    crate::input::mouse::click_at_xy(pid, screen_x, screen_y, count, &m)
                                }
                            }
                        };
                        // Foreground rung: brief front → click → restore.
                        // Returns whether the window was ACTUALLY fronted, so the
                        // reported `path` honestly reflects the rung that ran.
                        match (fg, window_id, has_modifiers) {
                            (true, Some(wid), true) => {
                                crate::input::skylight::with_foreground_hid_activation(
                                    pid as libc::pid_t,
                                    wid,
                                    do_click,
                                )
                                .map(|_| true)
                            }
                            (true, Some(wid), false) => {
                                crate::input::skylight::with_foreground_assist(
                                    pid as libc::pid_t,
                                    wid,
                                    do_click,
                                )
                            }
                            _ => do_click().map(|_| false),
                        }
                    })
                    .await
                },
            )
            .await;

            // The no-raise record can make NSWorkspace report the target as
            // active even though its window never moved in z-order. Once the
            // click has been queued, restore the prior app if the target is
            // still reported frontmost. Base this on observed state, not
            // `focus_without_raise`: the private recipe can report failure
            // after partially activating the target, and the raw click can
            // self-activate even when that recipe is unavailable. Do not
            // overwrite a different app here; the wildcard suppression lease
            // handles genuine side effects.
            if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise
                && prior_front != Some(pid)
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if let Some(previous_pid) = background_pixel_restore_pid(
                    activation_policy,
                    prior_front,
                    pid,
                    apps::frontmost_pid(),
                ) {
                    let _ = apps::activate_pid(previous_pid);
                }
            }

            let changes = super::finish_window_observation(snapshot, &args).await;

            let button_label = match button_str.as_str() {
                "right" => "right-click",
                "middle" => "middle-click",
                _ => "click",
            };
            match result {
                Ok(Ok(fronted)) => {
                    // `with_foreground_assist` returns `false` when the fronting SPIs
                    // were unavailable and it clicked WITHOUT activation — report the
                    // background path in that case so `path` reflects the rung that ran.
                    let (path, mode_label) = if fg && fronted {
                        ("cgevent_fg", "foreground CGEvent")
                    } else {
                        ("cgevent", "background CGEvent")
                    };
                    let evidence = if changes.needs_restore() {
                        probe.map(|probe| {
                            probe.settled(delivery_probe::Evidence::Changed(
                                delivery_probe::WINDOW_SIGNAL,
                            ))
                        })
                    } else if let Some(probe) = probe {
                        cua_driver_core::operation::spawn_blocking(move || probe.compare())
                            .await
                            .ok()
                    } else {
                        None
                    };
                    let window_change = if evidence.is_some() && changes.needs_restore() {
                        let appeared = changes.new_windows.clone();
                        cua_driver_core::operation::spawn_blocking(move || {
                            delivery_probe::WindowChangeEvidence::observe(pid, window_id, &appeared)
                        })
                        .await
                        .ok()
                    } else {
                        None
                    };
                    let mut msg = format!(
                        "✅ Posted {button_label} to pid {pid} ({mode_label}; \
                         not driver-verified — confirm via screenshot).{}",
                        changes.result_suffix()
                    );
                    let mut structured = serde_json::json!({
                        "path": path,
                        "verified": false,
                        "effect": "unverifiable",
                        "focus_without_raise": focus_without_raise,
                        "targeting": if indexed_point.is_some() { "element_center" } else { "pixel" },
                        "count": count
                    });
                    if let Some(outcome) = evidence {
                        apply_delivery_evidence(
                            &mut msg,
                            &mut structured,
                            ProbeReport {
                                outcome,
                                rung: NextRung::Foreground,
                                polled: changes.polled,
                            },
                            window_change.as_ref(),
                        );
                    }
                    ToolResult::text(msg).with_structured(structured)
                }
                Ok(Err(e)) => ToolResult::error(format!("{button_label} failed: {e}")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            }
        } else {
            ToolResult::error(
                "Provide either (element_index + window_id) or (x + y). pid is always required.",
            )
        }
    }
}

// ── AX click implementation (blocking) ───────────────────────────────────────

/// What the AX dispatch did, as far as the driver can tell.
#[derive(Default)]
struct AxClickOutcome {
    summary: String,
    needs_webkit_delay: bool,
    /// True when the element did not advertise the action we dispatched —
    /// AXUIElementPerformAction returns success regardless, so this is the
    /// driver's only signal that the press likely did nothing. The caller
    /// turns it into `effect: "suspected_noop"` + an escalation hint so the
    /// agent crosses to the vision/pixel path instead of trusting a hollow
    /// success.
    suspected_noop: bool,
    selection_verified: bool,
    selection_via_pixel: bool,
    /// The application's own reply, when it neither succeeded nor proved that
    /// nothing was performed. The action stays dispatched and the reply is
    /// reported as such: `AxActionReplyError::outcome_unverifiable`.
    unverified: Option<AxActionReplyError>,
}

/// Why an unadvertised action is escalated to the element's pixel action.
const UNADVERTISED_ACTION_REASON: &str = "element does not advertise this action — the \
                                          AX press likely no-op'd. Do an element px \
                                          action: click by pixel (x,y) off the \
                                          screenshot from get_window_state.";

/// The diagnostic half of an AX click reply, kept beside the action record
/// for the prose the probe appends. A generic click has no independent
/// read-back, so `verified` stays false unless a selection write was
/// confirmed, and the tri-state `effect` carries the richer verdict:
/// `suspected_noop` for an action the element never advertised (cross to the
/// vision/pixel path), `unverifiable` for a dispatch the driver cannot settle
/// (the caller's own observation does).
fn ax_click_structured(outcome: &AxClickOutcome, fronted: bool) -> serde_json::Value {
    let mut structured = serde_json::json!({
        "path": if outcome.selection_via_pixel {
            if fronted { "cgevent_fg" } else { "cgevent" }
        } else if fronted {
            "ax_fg"
        } else {
            "ax"
        },
        "verified": outcome.selection_verified,
        "effect": if outcome.selection_verified {
            "confirmed"
        } else if outcome.suspected_noop {
            "suspected_noop"
        } else {
            "unverifiable"
        },
    });
    if outcome.selection_verified {
        structured["evidence"] = serde_json::json!([{ "kind": "accessibility_readback" }]);
    }
    if outcome.suspected_noop {
        structured["escalation"] = serde_json::json!({
            "recommended": "px",
            "reason": UNADVERTISED_ACTION_REASON,
        });
    }
    structured
}

/// The action contract for an AX click, stated from what the dispatch and the
/// probe established rather than parsed back out of the reply.
///
/// The selection fallback delivers real pointer events on the rung the caller
/// asked for; everything else is an accessibility action. A reply that
/// establishes neither delivery nor a no-op leaves the delivery mode unknown.
/// The probe's reaction is published as observed-change evidence — weaker
/// than a read-back, so the effect it accompanies stays `unverifiable` — and
/// its silence turns the effect into `suspected_noop` escalated to the rung
/// that can still deliver.
fn ax_click_record(
    outcome: &AxClickOutcome,
    foreground: bool,
    probe: Option<ProbeReport>,
) -> ActionExecutionRecord {
    let transport = if !outcome.selection_via_pixel {
        ActionTransport::MacosAxAction
    } else if foreground {
        ActionTransport::MacosCgEventHid
    } else {
        ActionTransport::MacosCgEventPid
    };
    let requested = if foreground {
        RequestedDelivery::Foreground
    } else {
        RequestedDelivery::Background
    };
    let actual = if outcome.unverified.is_some() {
        ActualDelivery::Unknown
    } else if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    };
    // The probe's silence outranks the dispatch's own verdict: a press the
    // element never advertised and a press nothing reacted to are the same
    // suspected no-op, escalated to the rung that can still deliver.
    let silent = probe
        .filter(|report| matches!(report.outcome.evidence, delivery_probe::Evidence::Unchanged));
    let (effect, escalation) = if outcome.selection_verified {
        (ActionEffect::Confirmed, None)
    } else if let Some(report) = silent {
        (
            ActionEffect::SuspectedNoop,
            Some(ActionEscalation {
                kind: report.rung.escalation(),
                detail: Some(report.noop_reason()),
            }),
        )
    } else if outcome.suspected_noop {
        (
            ActionEffect::SuspectedNoop,
            Some(ActionEscalation {
                kind: EscalationKind::RetryWithPixelTarget,
                detail: Some(UNADVERTISED_ACTION_REASON.to_owned()),
            }),
        )
    } else {
        (ActionEffect::Unverifiable, None)
    };
    let mut record =
        ActionExecutionRecord::builder(effect, transport, requested).actual_delivery(actual);
    if outcome.selection_verified {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::AccessibilityReadback,
            detail: "AXSelected read back stable on the addressed collection item".into(),
        });
    }
    if let Some(reaction) = probe
        .map(|report| report.outcome.evidence)
        .filter(|evidence| evidence.is_reaction())
    {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::WindowChange,
            detail: reaction.signal().to_owned(),
        });
    }
    if let Some(escalation) = escalation {
        record = record.escalation(escalation);
    }
    record.build().expect("AX click record is valid")
}

/// Roles whose click gesture is "put the caret here" rather than "activate".
/// None of them advertise `AXPress`.
fn is_text_entry_role(role: &str) -> bool {
    matches!(
        role,
        "AXTextField"
            | "AXTextArea"
            | "AXSecureTextField"
            | "AXSearchField"
            | "AXComboBox"
            | "AXDateField"
            | "AXTimeField"
    )
}

/// An `AXFocused` write can be accepted and then clobbered when AppKit
/// installs the window's remembered first responder, so the write is read back
/// rather than trusted.
const FOCUS_READBACK_SETTLE: std::time::Duration = std::time::Duration::from_millis(80);

/// Focus a text control, proving it through the application's own
/// `AXFocusedUIElement`, and escalate to a pointer click at the control's
/// centre when the `AXFocused` write does not stick.
fn focus_text_entry(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    role: &str,
    title: &str,
    pixel: Option<SelectionPixelTarget>,
    foreground: bool,
) -> anyhow::Result<AxClickOutcome> {
    let ax_accepted = crate::input::ax_actions::focus_element(element_ptr).is_ok();
    if ax_accepted {
        std::thread::sleep(FOCUS_READBACK_SETTLE);
        if crate::input::ax_actions::is_element_focused(pid, element_ptr) {
            return Ok(AxClickOutcome {
                summary: format!(
                    "✅ Focused [{idx}] {role} \"{title}\": a text control has no AXPress \
                     action, so the click set keyboard focus, confirmed through the \
                     application's own AXFocusedUIElement."
                ),
                selection_verified: true,
                ..AxClickOutcome::default()
            });
        }
    }

    let Some(target) = pixel else {
        anyhow::bail!(
            "{role} has no AXPress action, the AXFocused write {}, and no resolvable \
             on-window frame was available for a pointer click; take a fresh snapshot and \
             click by pixel",
            if ax_accepted {
                "did not stick"
            } else {
                "was rejected"
            }
        );
    };
    crate::input::mouse::click_at_xy_with_window_local(
        pid,
        target.screen_x,
        target.screen_y,
        target.window_x,
        target.window_y,
        window_id,
        1,
        &[],
        crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
    )?;
    std::thread::sleep(FOCUS_READBACK_SETTLE);
    if crate::input::ax_actions::is_element_focused(pid, element_ptr) {
        return Ok(AxClickOutcome {
            summary: format!(
                "✅ Focused [{idx}] {role} \"{title}\" at its centre ({:.0}, {:.0}): the \
                 AXFocused write did not stick, so a pointer click was delivered and \
                 confirmed through the application's own AXFocusedUIElement.",
                target.screen_x, target.screen_y
            ),
            selection_verified: true,
            selection_via_pixel: true,
            ..AxClickOutcome::default()
        });
    }
    Ok(AxClickOutcome {
        summary: format!(
            "📨 Clicked [{idx}] {role} \"{title}\" at its centre ({:.0}, {:.0}): a text \
             control has no AXPress action, so focus was written and a pointer click \
             delivered, but the application still reports another element focused.",
            target.screen_x, target.screen_y
        ),
        selection_via_pixel: true,
        ..AxClickOutcome::default()
    })
}

/// Whether the application reports the addressed control disabled, giving a
/// requested activation the time it needs to land before answering.
///
/// This is the same mechanism `hotkey::dispatch_as_menu_command`
/// already documents for menu items: what a closed control reports is
/// whatever the application last validated, and for a window that has just
/// become key that is the stale pre-key value — measured on Notes, a menu
/// item still read disabled ~170 ms after its window was key while the item
/// pressed fine. The menu path re-triggers validation instead of waiting,
/// because opening each menu on the way is itself what makes AppKit validate
/// the item. A click has no equivalent move — nothing about pressing a
/// toolbar control re-validates it — so it re-reads within the activation's
/// own budget ([`crate::input::skylight::ACTIVATION_WAIT_TIMEOUT`], which
/// already bounds how long that activation is waited for, and covers the
/// measured staleness), at the same poll interval, and answers as soon as the
/// control enables. Without this, the single read right after the rung
/// refused a control the activation was about to enable: measured on a
/// toolbar search field, where the identical foreground call succeeded when
/// the read happened later.
///
/// A background dispatch has no activation pending, so its first read is the
/// whole answer. An unreadable attribute (`None`) is unknown, never disabled.
fn reads_disabled(foreground: bool, enabled: impl FnMut() -> Option<bool>) -> bool {
    reads_disabled_within(
        foreground,
        crate::input::skylight::ACTIVATION_WAIT_TIMEOUT,
        crate::input::skylight::ACTIVATION_POLL_INTERVAL,
        enabled,
    )
}

fn reads_disabled_within(
    foreground: bool,
    budget: std::time::Duration,
    tick: std::time::Duration,
    mut enabled: impl FnMut() -> Option<bool>,
) -> bool {
    if enabled() != Some(false) {
        return false;
    }
    if !foreground {
        return true;
    }
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(tick);
        if enabled() != Some(false) {
            return false;
        }
    }
    true
}

fn perform_ax_click(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    requested: RequestedAction<'_>,
    cursor_key: &str,
    selection_pixel: Option<SelectionPixelTarget>,
    menu_pixel: Option<SelectionPixelTarget>,
    modifiers: &[String],
    foreground: bool,
) -> anyhow::Result<AxClickOutcome> {
    let element = element_ptr as AXUIElementRef;

    // Capture advertised actions BEFORE dispatching so we can detect silent no-ops
    // (AX returns success even when the element doesn't advertise the action).
    let advertised = crate::ax::actions::split(unsafe { copy_action_names(element) });
    let ax_action = resolve_ax_action(requested.action, &advertised).ok_or_else(|| {
        anyhow::Error::new(UnknownAxAction {
            requested: requested.action.to_owned(),
            advertised: advertised.names(),
        })
    })?;

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let title = unsafe { copy_label_attr(element) }.unwrap_or_default();

    // Read the live value immediately before dispatch. Foreground assist can
    // enable menu items that were disabled in the cached snapshot, while a
    // background transition can disable them after that snapshot. macOS may
    // otherwise return success for a disabled action that did nothing.
    if reads_disabled(foreground, || {
        crate::input::ax_actions::ax_element_enabled(element_ptr)
    }) {
        let order = super::process_front_order(pid, window_id);
        return Err(anyhow::Error::new(ElementDisabled {
            action: ax_action.clone(),
            role: role.clone(),
            label: title.clone(),
            window_id,
            pid,
            foreground,
            front_in_process: order.target_is_front,
            obscuring_window: order.in_front,
            key_window: KeyWindowState::observe(pid),
        }));
    }

    // On a collection row the pointer gesture is select, so a plain click takes
    // the bounded, read-back-verified AXSelected write whether or not the
    // element advertises AXPress: press is the application's own default
    // action, reachable only when the caller names it. Elsewhere this is still
    // the path for an item whose selectable object is an ancestor.
    let press_is_advertised = advertised.advertises(&ax_action);
    if ax_action == "AXPress"
        && (!press_is_advertised
            || (!requested.named_by_caller
                && crate::input::ax_actions::click_selects_role(&role)
                && crate::input::ax_actions::exposes_selected(element_ptr)))
    {
        let press_route = press_route_advice(press_is_advertised);
        if modifiers.is_empty() {
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr)
            {
                return Ok(AxClickOutcome {
                    summary: format!(
                        "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\"; \
                         confirmed AXSelected=true.{press_route}"
                    ),
                    selection_verified: true,
                    ..AxClickOutcome::default()
                });
            }
        }

        if let (Some(target), Some(selection)) = (
            selection_pixel,
            crate::input::ax_actions::capture_nearest_container_selection(element_ptr),
        ) {
            let selected_role = selection.role().to_owned();
            let Some((before, _)) = selection.observe() else {
                anyhow::bail!("selection target stopped exposing AXSelected before delivery");
            };
            let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
            if foreground && !modifier_refs.is_empty() {
                crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                    target.screen_x,
                    target.screen_y,
                    1,
                    "left",
                    &modifier_refs,
                )?;
            } else {
                crate::input::mouse::click_at_xy_with_window_local(
                    pid,
                    target.screen_x,
                    target.screen_y,
                    target.window_x,
                    target.window_y,
                    window_id,
                    1,
                    &modifier_refs,
                    crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
                )?;
            }
            // AppKit may publish a transient AXSelected transition while the
            // event queue is still resolving the gesture. Let it settle before
            // accepting a candidate, then require the same state to survive a
            // second observation. A modified selection additionally preserves
            // every peer that was selected before delivery.
            std::thread::sleep(SELECTION_READBACK_SETTLE);
            let deadline = std::time::Instant::now() + SELECTION_READBACK_TIMEOUT;
            let mut last_observation = None;
            loop {
                if let Some((after, peers_preserved)) = selection.observe() {
                    last_observation = Some((after, peers_preserved));
                    let verified = selection_readback_confirms(
                        before,
                        after,
                        !modifiers.is_empty(),
                        peers_preserved,
                    );
                    if verified {
                        std::thread::sleep(SELECTION_READBACK_STABILITY);
                        if let Some((stable_after, stable_peers_preserved)) = selection.observe() {
                            last_observation = Some((stable_after, stable_peers_preserved));
                            if stable_after == after
                                && selection_readback_confirms(
                                    before,
                                    stable_after,
                                    !modifiers.is_empty(),
                                    stable_peers_preserved,
                                )
                            {
                                return Ok(AxClickOutcome {
                                    summary: format!(
                                        "✅ Selected nearest {selected_role} for [{idx}] {role} \
                                         \"{title}\"; AX selection write was unavailable, so a \
                                         coordinate click was delivered and confirmed by stable \
                                         AXSelected read-back.{press_route}"
                                    ),
                                    selection_verified: true,
                                    selection_via_pixel: true,
                                    ..AxClickOutcome::default()
                                });
                            }
                        }
                    }
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(SELECTION_READBACK_POLL);
            }
            if modifiers.is_empty() {
                anyhow::bail!(
                    "coordinate click did not produce a stable AXSelected transition; \
                     last_readback={last_observation:?}; retry after a fresh snapshot"
                );
            }
            anyhow::bail!(
                "foreground modified coordinate click did not produce a stable AXSelected \
                 transition while preserving the prior selection; \
                 before_selected={before}, last_readback={last_observation:?}; \
                 take a fresh snapshot before retrying"
            );
        }

        // A text control's click gesture places the caret; AppKit text roles
        // do not advertise AXPress, so dispatching one is a known no-op that
        // reports itself as a probable failure. Establish keyboard focus
        // instead, which is what the caller wanted the click for.
        if modifiers.is_empty() && is_text_entry_role(&role) {
            return focus_text_entry(
                element_ptr,
                idx,
                pid,
                window_id,
                &role,
                &title,
                selection_pixel,
                foreground,
            );
        }
    }

    let menus_before = (ax_action == "AXShowMenu")
        .then(|| crate::windows::accessory_window_ids(pid))
        .unwrap_or_default();

    let reply = dispatch_ax_action(&ax_action, || unsafe {
        crate::ax::bindings::perform_action(element, &ax_action)
    });
    let unverified = match ax_reply_disposition(reply, modifiers) {
        AxReplyDisposition::Performed => None,
        AxReplyDisposition::Dispatched(reply) => Some(reply),
        AxReplyDisposition::TrySelection(reply) => {
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr)
            {
                return Ok(AxClickOutcome {
                    summary: format!(
                        "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\" \
                         after AXPress returned {}; confirmed AXSelected=true.",
                        reply.code
                    ),
                    selection_verified: true,
                    ..AxClickOutcome::default()
                });
            }
            return Err(reply.into());
        }
        AxReplyDisposition::Failed(reply) => return Err(reply.into()),
    };

    let mut summary = ax_reply_summary(unverified.as_ref(), &ax_action, idx, &role, &title);

    // AXShowMenu returns success on controls that never open a menu, which
    // left `click(button:"right")` with no way to reach a context menu at all.
    // A real pointer right-click at the element centre is the actuation a user
    // performs, so cross that rung once the AX action proved empty. A reply
    // that cannot say what the app did is not that proof: naming the rung is
    // then the whole escalation, because a second actuation could act twice.
    if ax_action == "AXShowMenu" && !crate::windows::menu_appeared_since(pid, &menus_before) {
        match menu_pixel.filter(|_| unverified.is_none()) {
            Some(target) => {
                crate::input::mouse::right_click_at_xy_with_window_local(
                    pid,
                    target.screen_x,
                    target.screen_y,
                    target.window_x,
                    target.window_y,
                    window_id,
                    &[],
                )?;
                return Ok(AxClickOutcome {
                    summary: format!(
                        "✅ Right-clicked [{idx}] {role} \"{title}\" at its centre \
                         ({:.0}, {:.0}): AXShowMenu opened no menu, so a real pointer \
                         right-click was delivered.",
                        target.screen_x, target.screen_y
                    ),
                    selection_via_pixel: true,
                    ..AxClickOutcome::default()
                });
            }
            None => summary.push_str(&format!(
                "\n⚠️ No menu appeared and no pointer right-click was delivered ({}). \
                 Re-observe, then right-click by pixel with delivery_mode:\"foreground\".",
                if unverified.is_some() {
                    "the AXShowMenu reply cannot establish what the app did, so a second \
                     actuation was not improvised"
                } else {
                    "AXShowMenu reported success; the pixel right-click was unavailable — no \
                     resolvable frame, or the pointer rung is refused for this target"
                }
            )),
        }
    }

    // AXPopUpButton: name the options and the route that does not depend on
    // a menu staying open.
    if role == "AXPopUpButton" {
        summary.push_str(
            "\n\n⚠️ This is a popup/select button. Its menu is a surface of its own rather \
             than part of this window's subtree, and a native macOS menu closes as soon as \
             the window stops being key. To choose an option without depending on an open \
             menu, use:\n  set_value(pid, window_id, element_index, value)\n\
             To work inside the menu instead, re-observe this window: an open menu is \
             rendered under this control.",
        );
        let options = unsafe { popup_option_labels(element) };
        if !options.is_empty() {
            summary.push_str("\nAvailable options: [");
            summary.push_str(&options.join(", "));
            summary.push(']');
        }
    }

    // Advertised-action warning: non-fatal but surfaces likely no-ops. Also the
    // machine-readable `suspected_noop` signal returned to the caller.
    let suspected_noop = !advertised.advertises(&ax_action);
    if suspected_noop {
        let adv_list = if advertised.is_empty() {
            "none".to_owned()
        } else {
            advertised.names().join(", ")
        };
        summary.push_str(&format!(
            "\n⚠️ Element does not advertise {ax_action} (actions: {adv_list}). \
             Action may have been a no-op."
        ));
    }

    // WebKit DOM focus settle: 800 ms for text inputs (returned to async caller).
    let needs_webkit_delay =
        ax_action == "AXPress" && (role == "AXTextField" || role == "AXTextArea");

    // Show focus-rect highlight around the element (matches Swift showFocusRect).
    // Also move the cursor to the element center so the glide animation plays.
    if let Some(rect) = unsafe { element_screen_rect(element) } {
        // Drive THIS session's cursor (threaded in via `cursor_key`), matching
        // the keyed glide already played in the invoke body above. The keyed
        // glide already played on the session's cursor in the invoke body.
        crate::cursor::overlay::send_command(
            cursor_key.to_owned(),
            cursor_overlay::OverlayCommand::ShowFocusRect(Some(rect)),
        );
        // Animate cursor to element center.
        let cx = rect[0] + rect[2] / 2.0;
        let cy = rect[1] + rect[3] / 2.0;
        crate::cursor::overlay::send_command(
            cursor_key.to_owned(),
            cursor_overlay::OverlayCommand::ClickPulse { x: cx, y: cy },
        );
    }
    let _ = pid;
    let _ = window_id; // used by caller context

    Ok(AxClickOutcome {
        summary,
        needs_webkit_delay,
        suspected_noop,
        unverified,
        ..AxClickOutcome::default()
    })
}

#[cfg(test)]
mod selection_fallback_tests {
    use super::selection_readback_confirms;

    #[test]
    fn plain_click_requires_selected_readback() {
        assert!(selection_readback_confirms(false, true, false, true));
        assert!(selection_readback_confirms(true, true, false, true));
        assert!(!selection_readback_confirms(false, false, false, true));
    }

    #[test]
    fn modified_click_requires_a_transition_and_preserves_prior_selection() {
        assert!(selection_readback_confirms(false, true, true, true));
        assert!(selection_readback_confirms(true, false, true, true));
        assert!(!selection_readback_confirms(true, true, true, true));
        assert!(!selection_readback_confirms(false, true, true, false));
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A click that named an element in a quiescent window: every signal the
    /// probe has was comparable.
    fn outcome(evidence: delivery_probe::Evidence, waited_ms: u64) -> delivery_probe::ProbeOutcome {
        delivery_probe::ProbeOutcome {
            evidence,
            probe: std::time::Duration::from_millis(waited_ms + 250),
            waited: std::time::Duration::from_millis(waited_ms),
            watched: delivery_probe::Watched {
                element: true,
                focus: true,
                tree: true,
                accessory: true,
            },
        }
    }

    fn report(
        outcome: delivery_probe::ProbeOutcome,
        rung: NextRung,
        polled: bool,
    ) -> ProbeReport {
        ProbeReport {
            outcome,
            rung,
            polled,
        }
    }

    fn advertised(names: &[&str]) -> crate::ax::actions::ElementActions {
        crate::ax::actions::split(names.iter().map(|name| (*name).to_owned()).collect())
    }

    #[test]
    fn an_action_name_the_element_advertises_is_dispatched_verbatim() {
        let envelope = "Name:Pin List\nTarget:0x0\nSelector:(null)";
        let actions = advertised(&["AXScrollToVisible", envelope]);
        assert_eq!(
            resolve_ax_action("AXScrollToVisible", &actions).as_deref(),
            Some("AXScrollToVisible")
        );
        assert_eq!(
            resolve_ax_action(envelope, &actions).as_deref(),
            Some(envelope)
        );
        assert_eq!(
            resolve_ax_action("Pin List", &actions).as_deref(),
            Some(envelope),
            "the name the row advertises dispatches the envelope macOS expects"
        );
        assert_eq!(
            resolve_ax_action("show_menu", &actions).as_deref(),
            Some("AXShowMenu"),
            "a documented alias resolves without the element advertising it"
        );
    }

    #[test]
    fn an_unknown_action_name_is_refused_instead_of_pressed() {
        let actions = advertised(&["AXPress", "Name:Flag\nTarget:0x0\nSelector:(null)"]);
        assert_eq!(resolve_ax_action("AXScrollToVisible", &actions), None);
        assert_eq!(resolve_ax_action("Unflag", &actions), None);
        assert_eq!(resolve_ax_action("wiggle", &actions), None);

        let refusal = ax_action_error(anyhow::Error::new(UnknownAxAction {
            requested: "AXScrollToVisible".to_owned(),
            advertised: actions.names(),
        }));
        let details = refusal.structured_content.expect("structured refusal");
        assert_eq!(details["code"], "action_unsupported");
        assert_eq!(details["effect"], "not_dispatched");
        assert_eq!(details["action"], "AXScrollToVisible");
        assert_eq!(
            details["advertised_actions"],
            serde_json::json!(["AXPress", "Flag"])
        );
    }

    /// The probe reads five signals; an effect outside them is invisible to
    /// it, so an unobserved dispatch is unverified, never proven undelivered.
    /// The claim is only as wide as the window it watched, so the report has
    /// to name that window: a reader who knows the app reacts late can tell a
    /// short watch from a real no-op.
    #[test]
    fn unobserved_background_click_reports_a_suspected_noop_and_names_a_deliverable_rung() {
        let mut ax_msg = "✅ Performed AXPress on [3] AXButton \"New Item\".".to_owned();
        let mut ax = serde_json::json!({ "path": "ax", "effect": "unverifiable" });
        apply_delivery_evidence(
            &mut ax_msg,
            &mut ax,
            report(
                outcome(delivery_probe::Evidence::Unchanged, 2000),
                NextRung::PixelForeground,
                true,
            ),
            None,
        );
        assert_eq!(ax["effect"], "suspected_noop");
        // An AX press gains nothing from fronting the app: it never carries
        // pointer events, so the rung that can still deliver is pixels + HID.
        assert_eq!(ax["escalation"]["recommended"], "px");
        assert_eq!(ax["delivery_probe"]["waited_ms"], 2000);
        assert!(
            ax_msg.contains("watched for 2000 ms after the dispatch"),
            "{ax_msg}"
        );
        assert!(ax_msg.contains("Re-observe before repeating"), "{ax_msg}");
        assert!(!ax_msg.contains("NOT delivered"), "{ax_msg}");
        assert!(ax_msg.contains("delivery_mode:\"foreground\""), "{ax_msg}");

        let mut pixel_msg = "✅ Posted click to pid 1.".to_owned();
        let mut pixel = serde_json::json!({ "path": "cgevent", "effect": "unverifiable" });
        apply_delivery_evidence(
            &mut pixel_msg,
            &mut pixel,
            report(
                outcome(delivery_probe::Evidence::Unchanged, 2000),
                NextRung::Foreground,
                true,
            ),
            None,
        );
        assert_eq!(pixel["escalation"]["recommended"], "foreground");
    }

    #[test]
    fn observed_change_is_published_as_evidence_without_claiming_the_postcondition() {
        let mut msg = "✅ Performed AXPress on [3] AXButton \"B7\".".to_owned();
        let mut structured = serde_json::json!({ "path": "ax", "effect": "unverifiable" });
        apply_delivery_evidence(
            &mut msg,
            &mut structured,
            report(
                outcome(delivery_probe::Evidence::Changed("element_state"), 120),
                NextRung::PixelForeground,
                true,
            ),
            None,
        );
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["evidence"][0]["kind"], "element_state");
        assert!(structured["evidence"][0]["appeared_windows"].is_null());
        assert!(structured["escalation"].is_null());
        assert!(msg.contains("Delivered: element_state changed"), "{msg}");
    }

    #[test]
    fn a_declined_window_poll_is_not_reported_as_a_watched_signal() {
        let mut declined_msg = String::new();
        let mut declined = serde_json::json!({ "path": "ax", "effect": "unverifiable" });
        apply_delivery_evidence(
            &mut declined_msg,
            &mut declined,
            report(
                outcome(delivery_probe::Evidence::Unchanged, 2000),
                NextRung::PixelForeground,
                false,
            ),
            None,
        );
        assert!(
            declined_msg.contains(
                "element_state, app_focus, window_tree, menu_opened read the same. Re-observe"
            ),
            "{declined_msg}"
        );
        assert!(!declined_msg.contains("window_change"), "{declined_msg}");
        let reason = declined["escalation"]["reason"].as_str().expect("reason");
        assert!(
            reason.contains("element_state, app_focus, window_tree, menu_opened only"),
            "{reason}"
        );
        assert!(!reason.contains("window_change"), "{reason}");

        let mut polled_msg = String::new();
        let mut polled = serde_json::json!({ "path": "ax", "effect": "unverifiable" });
        apply_delivery_evidence(
            &mut polled_msg,
            &mut polled,
            report(
                outcome(delivery_probe::Evidence::Unchanged, 2000),
                NextRung::PixelForeground,
                true,
            ),
            None,
        );
        assert!(
            polled_msg.contains(
                "element_state, app_focus, window_tree, menu_opened, window_change read the same"
            ),
            "{polled_msg}"
        );
        assert!(polled["escalation"]["reason"]
            .as_str()
            .expect("reason")
            .contains("window_change"));
    }

    #[test]
    fn evidence_names_the_signal_that_moved_and_only_a_window_signal_carries_windows() {
        let observed = delivery_probe::WindowChangeEvidence {
            appeared_windows: vec![],
            target_window_main: Some(false),
        };
        for signal in ["element_state", "app_focus", "window_tree"] {
            let mut msg = String::new();
            let mut structured = serde_json::json!({ "path": "ax" });
            apply_delivery_evidence(
                &mut msg,
                &mut structured,
                report(
                    outcome(delivery_probe::Evidence::Changed(signal), 120),
                    NextRung::PixelForeground,
                    true,
                ),
                Some(&observed),
            );
            assert_eq!(structured["evidence"][0]["kind"], signal);
            assert!(
                structured["evidence"][0]["target_window_main"].is_null(),
                "{structured}"
            );
            assert!(
                structured["evidence"][0]["appeared_windows"].is_null(),
                "{structured}"
            );
        }
    }

    #[test]
    fn a_window_that_appeared_is_named_in_the_evidence_with_the_target_window_state() {
        let mut msg = String::new();
        let mut structured = serde_json::json!({ "path": "ax", "effect": "unverifiable" });
        let observed = delivery_probe::WindowChangeEvidence {
            appeared_windows: vec![delivery_probe::AppearedWindow {
                window_id: 10764,
                pid: 588,
                app_name: "Google Chrome".to_owned(),
                title: "Print".to_owned(),
                subrole: Some("AXDialog".to_owned()),
            }],
            target_window_main: Some(true),
        };
        apply_delivery_evidence(
            &mut msg,
            &mut structured,
            report(
                outcome(
                    delivery_probe::Evidence::Changed(delivery_probe::WINDOW_SIGNAL),
                    0,
                ),
                NextRung::PixelForeground,
                true,
            ),
            Some(&observed),
        );
        let appeared = &structured["evidence"][0]["appeared_windows"][0];
        assert_eq!(appeared["window_id"], 10764);
        assert_eq!(appeared["title"], "Print");
        assert_eq!(appeared["subrole"], "AXDialog");
        assert_eq!(structured["evidence"][0]["target_window_main"], true);
    }

    #[test]
    fn unusable_probe_leaves_the_existing_effect_alone() {
        let mut msg = "✅ Performed AXPress on [3] AXButton \"B7\".".to_owned();
        let mut structured = serde_json::json!({ "path": "ax", "effect": "suspected_noop" });
        apply_delivery_evidence(
            &mut msg,
            &mut structured,
            report(
                outcome(delivery_probe::Evidence::Unusable, 2000),
                NextRung::PixelForeground,
                true,
            ),
            None,
        );
        assert_eq!(structured["effect"], "suspected_noop");
        assert!(structured["evidence"].is_null());
        assert!(msg.contains("Delivery unverified"), "{msg}");
    }

    /// Contacts answers `AXPress` on its Edit/Done buttons with
    /// `kAXErrorFailure` or `kAXErrorAttributeUnsupported` *after* saving the
    /// card, and a text field answers `kAXErrorActionUnsupported` after taking
    /// focus. Reporting those replies as a failed invocation aborted the
    /// caller's cell before the observe that would have read the effect back.
    #[test]
    fn an_app_reply_that_cannot_disprove_the_action_reports_a_dispatch() {
        for code in [
            crate::ax::bindings::kAXErrorFailure,
            crate::ax::bindings::kAXErrorAttributeUnsupported,
            crate::ax::bindings::kAXErrorActionUnsupported,
        ] {
            let reply = dispatch_ax_action("AXPress", || code).unwrap_err();
            assert!(reply.outcome_unverifiable(), "{code}");
            let summary = ax_reply_summary(Some(&reply), "AXPress", 7, "AXButton", "Done");
            assert!(
                summary.contains("Dispatched AXPress to [7] AXButton \"Done\""),
                "{summary}"
            );
            assert!(summary.contains(&code.to_string()), "{summary}");
            assert!(summary.contains(reply.code_name()), "{summary}");
            assert!(!summary.contains('✅'), "{summary}");
        }
        assert!(ax_reply_summary(None, "AXOpen", 3, "AXRow", "note.txt")
            .starts_with("✅ Performed AXOpen"));
    }

    /// The reply decides what may happen next, before anything else touches
    /// the UI. An application that answered the action may have performed it,
    /// so the collection-row selection fallback — an `AXSelected` write whose
    /// read-back would publish a confirmed effect — must not run for those
    /// replies. `-25204` (kAXErrorCannotComplete) is the refusal that fallback
    /// exists for and keeps it.
    #[test]
    fn an_unverifiable_reply_is_classified_before_the_selection_fallback() {
        for code in [
            crate::ax::bindings::kAXErrorFailure,
            crate::ax::bindings::kAXErrorAttributeUnsupported,
            crate::ax::bindings::kAXErrorActionUnsupported,
        ] {
            let reply = dispatch_ax_action("AXPress", || code);
            assert_eq!(
                ax_reply_disposition(reply, &[]),
                AxReplyDisposition::Dispatched(AxActionReplyError {
                    action: "AXPress".to_owned(),
                    code
                }),
                "{code}"
            );
        }
        let refusal = AxActionReplyError {
            action: "AXPress".to_owned(),
            code: -25204,
        };
        assert_eq!(
            ax_reply_disposition(Err(refusal.clone()), &[]),
            AxReplyDisposition::TrySelection(refusal.clone())
        );
        assert_eq!(
            ax_reply_disposition(Err(refusal.clone()), &["cmd".to_string()]),
            AxReplyDisposition::Failed(refusal),
            "a modified click never improvises a selection"
        );
        assert_eq!(
            ax_reply_disposition(
                Err(AxActionReplyError {
                    action: "AXShowMenu".to_owned(),
                    code: -25204
                }),
                &[]
            ),
            AxReplyDisposition::Failed(AxActionReplyError {
                action: "AXShowMenu".to_owned(),
                code: -25204
            }),
            "only a plain press has a selection equivalent"
        );
        assert_eq!(
            ax_reply_disposition(Ok(()), &[]),
            AxReplyDisposition::Performed
        );
    }

    fn public(record: &ActionExecutionRecord) -> serde_json::Value {
        serde_json::to_value(record.public_result().expect("public result")).unwrap()
    }

    /// The published result of a dispatch the application answered: uncertain,
    /// with the delivery mode unknown and no read-back evidence. Only a
    /// confirmed selection write earns `confirmed`.
    #[test]
    fn an_answered_dispatch_is_not_published_as_a_confirmed_effect() {
        let dispatched = ax_click_record(
            &AxClickOutcome {
                unverified: Some(AxActionReplyError {
                    action: "AXPress".to_owned(),
                    code: crate::ax::bindings::kAXErrorFailure,
                }),
                ..AxClickOutcome::default()
            },
            false,
            None,
        );
        assert_eq!(
            public(&dispatched),
            serde_json::json!({
                "effect": "unverifiable",
                "route": "accessibility",
                "delivery": {"mode": "unknown"},
            })
        );

        let selected = ax_click_record(
            &AxClickOutcome {
                selection_verified: true,
                selection_via_pixel: true,
                ..AxClickOutcome::default()
            },
            false,
            None,
        );
        assert_eq!(
            public(&selected),
            serde_json::json!({
                "effect": "confirmed",
                "route": "synthetic_events",
                "delivery": {"mode": "background"},
                "evidence": [{"kind": "value_readback"}],
            })
        );
    }

    /// The probe settles what the dispatch could not: a reaction is published
    /// as observed-change evidence under an effect that stays unverifiable,
    /// and silence is a suspected no-op escalated to the rung that can still
    /// deliver — over the unadvertised-action escalation the dispatch chose.
    #[test]
    fn the_probe_verdict_reaches_the_published_result() {
        let reacted = ax_click_record(
            &AxClickOutcome::default(),
            true,
            Some(report(
                outcome(delivery_probe::Evidence::ElementGone, 120),
                NextRung::PixelForeground,
                true,
            )),
        );
        assert_eq!(
            public(&reacted),
            serde_json::json!({
                "effect": "unverifiable",
                "route": "accessibility",
                "delivery": {"mode": "foreground"},
                "evidence": [{"kind": "window_change", "signal": "element_state"}],
            })
        );

        let silent = ax_click_record(
            &AxClickOutcome {
                suspected_noop: true,
                ..AxClickOutcome::default()
            },
            false,
            Some(report(
                outcome(delivery_probe::Evidence::Unchanged, 2000),
                NextRung::Foreground,
                false,
            )),
        );
        assert_eq!(silent.effect, ActionEffect::SuspectedNoop);
        let escalation = silent.escalation.as_ref().expect("escalation");
        assert_eq!(escalation.kind, EscalationKind::RetryWithForegroundDelivery);
        assert!(
            escalation
                .detail
                .as_deref()
                .unwrap()
                .contains("the probe compared element_state, app_focus, window_tree, menu_opened"),
            "{escalation:?}"
        );
        assert_eq!(
            public(&silent)["escalation"],
            serde_json::json!({"target": "foreground", "reason": "suspected_noop"})
        );
    }

    /// A reply the framework produced rather than an application answering an
    /// action — disabled API, a messaging timeout — keeps the error contract
    /// and its reconcile-first advice: nothing there says the request ever
    /// reached the receiver, and nothing rules out that it did.
    #[test]
    fn framework_reply_errors_stay_failures_that_ask_for_reconciliation() {
        let mut receiver_commits = 0;
        let failure = dispatch_ax_action("AXPress", || {
            receiver_commits += 1;
            crate::ax::bindings::kAXErrorAPIDisabled
        })
        .unwrap_err();
        assert!(!failure.outcome_unverifiable());
        let result = ax_action_error(anyhow::Error::new(failure).context("save panel closed"));
        assert_eq!(receiver_commits, 1);
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.unwrap();
        assert_eq!(data["error"], "ActionOutcomeUnknown");
        assert_eq!(data["dispatch"], "attempted");
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["retry"], "reconcile_first");

        let failure = dispatch_ax_action("AXPress", || -25204).unwrap_err();
        assert!(!failure.outcome_unverifiable());
        assert_eq!(
            ax_action_error(failure.into()).structured_content.unwrap()["effect"],
            "unverifiable"
        );
        assert!(dispatch_ax_action("AXPress", || kAXErrorSuccess).is_ok());
    }

    #[test]
    fn preflight_failure_does_not_claim_ax_dispatch_was_attempted() {
        let result = ax_action_error(anyhow::anyhow!("target no longer exists"));
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
    }

    fn reply_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| match content {
                cua_driver_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// `-25202` is the one reply code that proves the opposite of "attempted
    /// and may already have taken effect": the reference was already invalid,
    /// so the framework never reached the application.
    #[test]
    fn a_dead_element_reference_reports_that_nothing_was_dispatched() {
        let reply = dispatch_ax_action("AXPress", || crate::ax::bindings::kAXErrorInvalidUIElement)
            .unwrap_err();
        assert!(reply.element_is_gone());
        assert_eq!(reply.code_name(), "kAXErrorInvalidUIElement");

        let result = ax_action_error(reply.into());
        let text = reply_text(&result);
        assert!(
            text.contains("The addressed element no longer exists"),
            "{text}"
        );
        assert!(text.contains("Nothing was dispatched."), "{text}");
        assert!(
            !text.contains("may already have taken effect"),
            "still claimed a possible effect: {text}"
        );
        let data = result.structured_content.expect("structured refusal");
        assert_eq!(data["code"], "element_no_longer_exists");
        assert_eq!(data["effect"], "not_dispatched");
        assert_eq!(data["ax_error"], -25202);
        assert_eq!(data["ax_error_name"], "kAXErrorInvalidUIElement");
        assert_eq!(data["escalation"]["target"], "snapshot");
        assert_eq!(data["escalation"]["reason"], "route_unavailable");
    }

    /// A disabled control in a window that *is* the application's key window:
    /// the state in which no focus-related route is left to name.
    fn disabled(role: &str, front_in_process: bool) -> ElementDisabled {
        ElementDisabled {
            action: "AXPress".to_owned(),
            role: role.to_owned(),
            label: "Back".to_owned(),
            window_id: 17002,
            pid: 47983,
            foreground: false,
            front_in_process,
            obscuring_window: None,
            key_window: KeyWindowState {
                app_frontmost: true,
                focused_window_id: Some(17002),
            },
        }
    }

    #[test]
    fn a_disabled_control_already_in_front_names_no_route() {
        let reason = disabled("AXButton", true).reason();
        assert!(
            reason.contains("window 17002 reports AXEnabled=false"),
            "{reason}"
        );
        assert!(
            reason.contains("Window 17002 is already pid 47983's front window"),
            "{reason}"
        );
        assert!(
            reason.contains("neither delivery mode nor activation changes that"),
            "{reason}"
        );
        for ruled_out in ["foreground", "bring_to_front"] {
            assert!(
                !reason.contains(ruled_out),
                "named a route the state rules out ({ruled_out}): {reason}"
            );
        }
    }

    fn blocker(window_id: u32, title: &str, ax_backed: bool) -> super::super::ObscuringWindow {
        super::super::ObscuringWindow {
            window_id,
            title: title.to_owned(),
            layer: 0,
            ax_backed: Some(ax_backed),
            role: ax_backed.then(|| "AXWindow".to_owned()),
            subrole: ax_backed.then(|| "AXUnknown".to_owned()),
            modal: None,
        }
    }

    /// A blocker the application reports modal: the one AX-backed window in
    /// front that raising the target cannot get past.
    fn modal_blocker(window_id: u32, title: &str) -> super::super::ObscuringWindow {
        let mut blocker = blocker(window_id, title, true);
        blocker.modal = Some(true);
        blocker
    }

    /// An AX-backed window the application reports modal is the one window in
    /// front that raising the target cannot get past, so it decides the cause.
    #[test]
    fn a_disabled_control_behind_a_modal_window_offers_that_window() {
        let mut state = disabled("AXButton", false);
        state.obscuring_window = Some(modal_blocker(17018, ""));
        let reason = state.reason();
        assert!(
            reason.contains(
                "window 17018 — pid 47983's own front window, titleless, AXWindow/AXUnknown — is \
                 drawn in front of it"
            ),
            "{reason}"
        );
        assert!(
            reason.contains("Dismiss that window, or address window 17018 and act on it there."),
            "{reason}"
        );

        let payload = state.payload();
        assert_eq!(payload["code"], "element_disabled");
        assert_eq!(payload["effect"], "not_dispatched");
        assert_eq!(payload["front_in_process"], false);
        assert_eq!(payload["obscured_by"]["window_id"], 17018);
        assert_eq!(payload["obscured_by"]["layer"], 0);
        assert_eq!(payload["obscured_by"]["ax_backed"], true);
        assert_eq!(payload["obscured_by"]["subrole"], "AXUnknown");
        assert_eq!(payload["obscured_by"]["modal"], true);
        assert!(payload.get("escalation").is_none(), "{payload}");
    }

    /// An ordinary window of the process in front of the target is what the
    /// foreground rung's make-key-plus-`AXRaise` overtakes, so it does not
    /// decide the cause: the key-window fact does, the rung is named, and the
    /// window in front is still named as the order.
    #[test]
    fn a_sibling_window_in_front_that_is_not_modal_leaves_the_foreground_rung_open() {
        let mut state = not_key(false);
        state.key_window = KeyWindowState {
            app_frontmost: true,
            focused_window_id: Some(19077),
        };
        state.obscuring_window = Some(blocker(19077, "Second", true));
        let reason = state.reason();
        assert!(
            reason.contains(
                "Window 19077 — pid 84264's own front window, titled \"Second\", \
                 AXWindow/AXUnknown — is drawn in front of window 19080, but window 19080 is \
                 not pid 84264's key window — window 19077 holds pid 84264's keyboard focus"
            ),
            "{reason}"
        );
        assert!(
            reason.contains("A foreground dispatch makes it key first."),
            "{reason}"
        );
        assert!(!reason.contains("Dismiss that window"), "{reason}");

        let payload = state.payload();
        assert_eq!(payload["escalation"]["target"], "foreground");
        assert_eq!(payload["obscured_by"]["window_id"], 19077);
        assert_eq!(payload["key_window"]["is_key"], false);
    }

    /// A layer-0 panel with no `AXWindow` can never become the focused window,
    /// so the reply must not offer to address it.
    #[test]
    fn a_panel_with_no_ax_surface_is_not_offered_as_a_target() {
        let mut state = disabled("AXButton", false);
        state.obscuring_window = Some(blocker(17013, "", false));
        let reason = state.reason();
        assert!(
            reason.contains("own front window, titleless, no AX surface — is drawn in front of it"),
            "{reason}"
        );
        assert!(
            reason.contains(
                "It publishes no AXWindow, so it cannot become the focused window: dismiss it, \
                 or act on it by pixel."
            ),
            "{reason}"
        );
        assert!(!reason.contains("address window"), "{reason}");
        assert_eq!(state.payload()["obscured_by"]["ax_backed"], false);
        assert!(
            state.payload()["obscured_by"]["role"].is_null(),
            "{state:?}"
        );
    }

    #[test]
    fn a_disabled_menu_item_is_disabled_by_app_state_not_by_focus() {
        let reason = disabled("AXMenuItem", false).reason();
        assert!(
            reason
                .contains("tracks the application's own applicability, not focus or delivery mode"),
            "{reason}"
        );
        assert!(
            reason.contains("disabled in pid 47983's current state"),
            "{reason}"
        );
        assert!(!reason.contains("foreground"), "{reason}");
    }

    #[test]
    fn a_titled_blocker_is_named_by_its_title() {
        let mut state = disabled("AXButton", false);
        state.obscuring_window = Some(blocker(17018, "Print", true));
        assert!(
            state
                .reason()
                .contains("own front window, titled \"Print\", AXWindow/AXUnknown —"),
            "{}",
            state.reason()
        );
    }

    /// The measured Notes state: a background press on the toolbar search
    /// field of a window that is front in its own process, with nothing in
    /// front of it, while the application is not frontmost.
    fn not_key(front_in_process: bool) -> ElementDisabled {
        ElementDisabled {
            action: "AXPress".to_owned(),
            role: "AXTextField".to_owned(),
            label: String::new(),
            window_id: 19080,
            pid: 84264,
            foreground: false,
            front_in_process,
            obscuring_window: None,
            key_window: KeyWindowState {
                app_frontmost: false,
                focused_window_id: Some(19080),
            },
        }
    }

    #[test]
    fn a_disabled_control_on_a_window_that_is_not_key_names_the_foreground_rung() {
        let state = not_key(true);
        let reason = state.reason();
        assert_eq!(
            reason,
            "AXPress was not dispatched: AXTextField \"\" of window 19080 reports \
             AXEnabled=false. Window 19080 is already pid 84264's front window and no window of \
             pid 84264 is drawn in front of it, but window 19080 is not pid 84264's key window — \
             pid 84264 is not the frontmost application — and a control whose enabled state \
             tracks key-window focus reads disabled until its window is key. A foreground \
             dispatch makes it key first."
        );

        let payload = state.payload();
        assert_eq!(payload["code"], "element_disabled");
        assert_eq!(payload["effect"], "not_dispatched");
        assert_eq!(payload["escalation"]["target"], "foreground");
        assert_eq!(payload["escalation"]["reason"], "route_unavailable");
        assert_eq!(payload["key_window"]["is_key"], false);
        assert_eq!(payload["key_window"]["app_frontmost"], false);
        assert_eq!(payload["key_window"]["focused_window_id"], 19080);
        assert!(payload.get("obscured_by").is_none(), "{payload}");
    }

    /// Which observation denies the key status is named, not paraphrased.
    #[test]
    fn the_window_holding_the_app_s_focus_is_named_when_the_app_is_frontmost() {
        let mut state = not_key(false);
        state.key_window = KeyWindowState {
            app_frontmost: true,
            focused_window_id: Some(19077),
        };
        let reason = state.reason();
        assert!(
            reason.contains("No window of pid 84264 is drawn in front of window 19080, but"),
            "{reason}"
        );
        assert!(
            reason.contains("window 19077 holds pid 84264's keyboard focus"),
            "{reason}"
        );

        state.key_window = KeyWindowState {
            app_frontmost: true,
            focused_window_id: None,
        };
        assert!(
            state
                .reason()
                .contains("pid 84264 reports no focused window"),
            "{}",
            state.reason()
        );
    }

    /// A refusal composed inside the foreground assist has no activation left
    /// to offer — but it is not the application's own verdict either. The
    /// rung's activation had not made the window key by the time the control
    /// was read, and that is what the reply says, with no escalation to a rung
    /// that is already in force.
    #[test]
    fn a_spent_foreground_rung_reports_the_activation_that_did_not_land() {
        let mut state = not_key(true);
        state.foreground = true;
        let reason = state.reason();
        assert_eq!(
            reason,
            "AXPress was not dispatched: AXTextField \"\" of window 19080 reports \
             AXEnabled=false. Window 19080 is already pid 84264's front window and no window of \
             pid 84264 is drawn in front of it, but the foreground rung did not make window \
             19080 pid 84264's key window within its wait — pid 84264 is not the frontmost \
             application — so this control was read while its window was still not key. \
             Re-observe: the control enables once its window is key."
        );
        assert!(
            !reason.contains("neither delivery mode nor activation changes that"),
            "an unlanded activation is not the application's own verdict: {reason}"
        );
        assert!(
            state.payload().get("escalation").is_none(),
            "{}",
            state.payload()
        );
    }

    /// The activation *did* land — the window is key and the control is still
    /// disabled. That, and only that, is the application's own verdict.
    #[test]
    fn a_landed_activation_leaves_only_the_application_s_own_state() {
        let mut state = disabled("AXButton", true);
        state.foreground = true;
        let reason = state.reason();
        assert!(
            reason.contains(
                "the application disabled this control, and neither delivery mode nor \
                 activation changes that"
            ),
            "{reason}"
        );
        assert!(
            state.payload().get("escalation").is_none(),
            "{}",
            state.payload()
        );
    }

    /// The pre-dispatch enabled read is the whole answer for a background
    /// dispatch: there is no activation pending that could change it.
    #[test]
    fn a_background_dispatch_answers_the_first_enabled_read() {
        let mut reads = 0;
        let disabled = reads_disabled_within(
            false,
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(5),
            || {
                reads += 1;
                Some(false)
            },
        );
        assert!(disabled);
        assert_eq!(reads, 1, "a background dispatch waited for an activation");
    }

    /// Under `foreground` the read is retried until the activation the rung
    /// requested re-enables the control: AppKit re-validates a
    /// key-window-sensitive control only once it installs the key window.
    #[test]
    fn a_foreground_dispatch_lets_the_activation_enable_the_control() {
        let mut reads = 0;
        let disabled = reads_disabled_within(
            true,
            std::time::Duration::from_millis(400),
            std::time::Duration::from_millis(5),
            || {
                reads += 1;
                Some(reads >= 3)
            },
        );
        assert!(!disabled, "the control enabled on read {reads}");
        assert_eq!(reads, 3);
    }

    /// A control the activation never enables is still refused, and the wait
    /// is bounded.
    #[test]
    fn a_control_the_activation_never_enables_is_refused() {
        let started = std::time::Instant::now();
        let disabled = reads_disabled_within(
            true,
            std::time::Duration::from_millis(40),
            std::time::Duration::from_millis(5),
            || Some(false),
        );
        assert!(disabled);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(400),
            "the wait outran its budget: {:?}",
            started.elapsed()
        );
    }

    /// An application that reports no `AXEnabled` at all reports nothing about
    /// this control: unknown is never disabled.
    #[test]
    fn an_unreported_enabled_attribute_is_not_a_disabled_control() {
        assert!(!reads_disabled(false, || None));
        assert!(!reads_disabled(true, || None));
    }

    /// A key window's disabled control and a menu row keep their own arms, and
    /// a modal window in front outranks the key-window fact: dismissing or
    /// addressing that window is what the state leaves open.
    #[test]
    fn the_other_arms_withhold_the_foreground_escalation() {
        for state in [disabled("AXButton", true), disabled("AXMenuItem", false)] {
            assert!(
                state.payload().get("escalation").is_none(),
                "{}",
                state.payload()
            );
        }
        let mut behind_panel = not_key(false);
        behind_panel.obscuring_window = Some(modal_blocker(19091, "Print"));
        let reason = behind_panel.reason();
        assert!(
            reason.contains("Dismiss that window, or address window 19091 and act on it there."),
            "{reason}"
        );
        assert!(!reason.contains("key window"), "{reason}");
        assert!(
            behind_panel.payload().get("escalation").is_none(),
            "{}",
            behind_panel.payload()
        );
    }

    /// Surface 5: schema must advertise the new `button` field with the three
    /// canonical values and default to "left". Hermes / Codex / Claude Code
    /// consumers branch on this enum being present.
    #[test]
    fn schema_advertises_button_enum() {
        let d = def();
        let props = d.input_schema.get("properties").expect("properties");
        let button = props.get("button").expect("button field present");
        let kind = button.get("type").and_then(|v| v.as_str());
        assert_eq!(kind, Some("string"));
        let enum_vals: Vec<&str> = button
            .get("enum")
            .and_then(|v| v.as_array())
            .expect("button.enum present")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(enum_vals.contains(&"left"));
        assert!(enum_vals.contains(&"right"));
        assert!(enum_vals.contains(&"middle"));
    }

    /// Surface 5 hard constraint: the tool description must mention the
    /// `button` argument and the "left" default so MCP introspection (which
    /// pipes description into LLM prompts) carries the back-compat note.
    #[test]
    fn description_mentions_button_default() {
        let d = def();
        let desc = d.description.to_ascii_lowercase();
        assert!(
            desc.contains("button"),
            "description should mention button arg"
        );
        assert!(
            desc.contains("left"),
            "description should mention left default"
        );
        assert!(
            desc.contains("middle"),
            "description should mention middle button"
        );
    }

    /// Existing default behaviour preserved: no `button` field on the call →
    /// resolves to "left" inside invoke. We can't drive the AX path without a
    /// live macOS Window Server, but we CAN check the same arg-parsing logic
    /// the invoke uses produces "left" for empty / absent input.
    #[test]
    fn element_gesture_uses_live_logical_center_and_refuses_off_window_geometry() {
        let window = crate::windows::WindowBounds {
            x: 100.0,
            y: 200.0,
            width: 500.0,
            height: 300.0,
        };
        let point = element_click_point([120.0, 230.0, 40.0, 60.0], &window).unwrap();
        assert_eq!(
            (
                point.screen_x,
                point.screen_y,
                point.window_x,
                point.window_y
            ),
            (140.0, 260.0, 40.0, 60.0)
        );
        assert!(element_click_point([700.0, 230.0, 40.0, 60.0], &window).is_err());
        assert!(element_click_point([120.0, 230.0, 0.0, 60.0], &window).is_err());
        assert!(element_click_point([f64::NAN, 230.0, 40.0, 60.0], &window).is_err());
    }

    #[test]
    fn button_defaults_to_left_when_absent() {
        use cua_driver_core::tool_args::ArgsExt;
        let args = serde_json::json!({ "pid": 1234 });
        let button_str_raw = args.str_or("button", "left").to_lowercase();
        let resolved = if button_str_raw.is_empty() {
            "left".to_string()
        } else {
            button_str_raw
        };
        assert_eq!(resolved, "left");
    }

    /// Round-trip the three canonical values through the same parse the invoke
    /// uses, so any future refactor that changes str_or semantics breaks here
    /// before it breaks consumers.
    #[test]
    fn button_round_trips_right_and_middle() {
        use cua_driver_core::tool_args::ArgsExt;
        for v in ["left", "right", "middle"] {
            let args = serde_json::json!({ "pid": 1234, "button": v });
            let s = args.str_or("button", "left").to_lowercase();
            assert_eq!(s, v);
        }
    }

    /// Regression for the Swift→Rust port gap: only a raw background left
    /// click with an exact window may intentionally activate the target
    /// without raising it. Other background buttons retain strict suppression,
    /// and the explicit foreground rung owns its separate activation.
    #[test]
    fn raw_background_left_click_restores_focus_without_raise_policy() {
        assert_eq!(
            pixel_activation_policy("left", false, true),
            PixelActivationPolicy::AllowTargetWithoutRaise
        );
        assert_eq!(
            pixel_activation_policy("left", false, false),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("right", false, true),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("middle", false, true),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("left", true, true),
            PixelActivationPolicy::ForegroundAssist
        );
    }

    /// The no-foreground contract must not depend on the private activation
    /// recipe reporting full success. If that recipe is unavailable or only
    /// partially succeeds but the target is nevertheless observed frontmost,
    /// restore the user's prior app.
    #[test]
    fn failed_private_activation_still_restores_observed_target_focus() {
        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::AllowTargetWithoutRaise,
                Some(7),
                42,
                Some(42),
            ),
            Some(7)
        );

        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::AllowTargetWithoutRaise,
                Some(7),
                42,
                Some(99),
            ),
            None,
            "do not overwrite an unrelated app that became frontmost"
        );
        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::SuppressTarget,
                Some(7),
                42,
                Some(42),
            ),
            None,
            "strict-suppression paths retain their existing ownership"
        );
    }
}
