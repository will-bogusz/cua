use async_trait::async_trait;
use cua_driver_contract::PressKeyInput;
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
        EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use libc;
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_bool_attr, copy_string_attr, focused_element_of_pid, AXUIElementRef,
};
use crate::ax::OwnedElement;
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;

use super::delivery_probe;
use super::ToolState;

pub struct PressKeyTool {
    state: Arc<ToolState>,
}

impl PressKeyTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
struct AxKeyState {
    value: Option<String>,
    selected: Option<bool>,
}

enum PressKeyDeliveryOutcome {
    Confirmed,
    /// The addressed control's own value/selection did not move, so the
    /// delivery probe's wider verdict is what the reply has to carry.
    Unverifiable(Option<delivery_probe::DeliveryProbe>),
    Failed(anyhow::Error),
}

fn map_delivery_outcome(
    result: anyhow::Result<(bool, Option<delivery_probe::DeliveryProbe>)>,
) -> PressKeyDeliveryOutcome {
    match result {
        Ok((true, _)) => PressKeyDeliveryOutcome::Confirmed,
        Ok((false, probe)) => PressKeyDeliveryOutcome::Unverifiable(probe),
        Err(error) => PressKeyDeliveryOutcome::Failed(error),
    }
}

/// The transport label for a key press. `press_key`'s foreground rung posts on
/// the global HID tap, which is what its own `ActionExecutionRecord` already
/// reports; the px-focus path falls through to a PID-routed post.
fn key_path(foreground: bool, hid_tap: bool) -> &'static str {
    match (foreground, hid_tap) {
        (true, true) => "key_events_hid_fg",
        (true, false) => "key_events_fg",
        (false, _) => "key_events",
    }
}

/// What the probe watched, for a key press whose control did not move.
///
/// A single key has no general postcondition either, so the probe reports only
/// whether anything reacted. It is never turned into a retry: the post may
/// have landed invisibly, and pressing twice is worse.
fn key_noop_report(polled: bool) -> delivery_probe::NoopReport<'static> {
    delivery_probe::NoopReport {
        signals: if polled {
            "focused element, app focus, window contents, new windows"
        } else {
            "focused element, app focus, window contents"
        },
        escalation: None,
        advice: "",
    }
}

fn validate_post_target(pid: i32) -> anyhow::Result<()> {
    if pid <= 0 {
        anyhow::bail!("target pid {pid} is invalid");
    }
    // Both SLEventPostToPid and CGEventPostToPid are void APIs. A successful
    // call proves only that the request was accepted for posting, not that the
    // target consumed it. Reject the one positive pre-post failure oracle macOS
    // exposes: a process that no longer exists. EPERM still proves liveness.
    let status = unsafe { libc::kill(pid, 0) };
    if status == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EPERM) {
        Ok(())
    } else {
        anyhow::bail!("target pid {pid} is not available for event posting: {error}")
    }
}

fn read_ax_key_state(pid: i32, window_id: Option<u32>, element_ptr: usize) -> Option<AxKeyState> {
    if super::type_text::target_in_web_area(pid, Some((element_ptr, None)), window_id) {
        return None;
    }
    let element = element_ptr as AXUIElementRef;
    let state = AxKeyState {
        value: unsafe { copy_string_attr(element, "AXValue") },
        selected: unsafe { copy_bool_attr(element, "AXSelected") },
    };
    (state.value.is_some() || state.selected.is_some()).then_some(state)
}

/// Post a key and report whether the addressed control's own value/selection
/// moved. `dispatch` is handed a `capture` callback it MUST call immediately
/// before posting: an activation moves the app's focused element itself, and
/// that move is not the key's effect, so the wider delivery probe can only be
/// captured inside the activation.
fn dispatch_with_ax_oracle(
    pid: i32,
    window_id: Option<u32>,
    explicit_element_ptr: Option<usize>,
    dispatch: impl FnOnce(&mut dyn FnMut()) -> anyhow::Result<()>,
) -> anyhow::Result<(bool, Option<delivery_probe::DeliveryProbe>)> {
    // The focused element the oracle and the probe watch must outlive the
    // dispatch that may destroy it, so it is owned here for the whole call
    // (an explicit element already arrives held by the element cache).
    let owned_focus = match explicit_element_ptr {
        Some(_) => None,
        // SAFETY: both resolvers return a `+1` reference, which the guard
        // takes over and releases when this function returns.
        None => unsafe {
            match window_id {
                Some(wid) => crate::ax::exact_target::focused_element_in_window(pid, wid),
                None => focused_element_of_pid(pid),
            }
            .and_then(|element| OwnedElement::adopt(element))
        },
    };
    let element_ptr =
        explicit_element_ptr.or_else(|| owned_focus.as_ref().map(OwnedElement::as_ptr));
    let before = element_ptr.and_then(|ptr| read_ax_key_state(pid, window_id, ptr));
    let mut probe = None;
    let result = {
        let mut capture = || {
            if let Some(wid) = window_id {
                probe = Some(delivery_probe::DeliveryProbe::capture(
                    pid,
                    wid,
                    element_ptr,
                ));
            }
        };
        dispatch(&mut capture)
    };
    // Native controls normally publish their new value/selection on the next
    // run-loop turn. Keep this bounded and reuse the exact retained element so
    // a focus move cannot become false confirmation from a different control.
    if before.is_some() {
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    let after = element_ptr.and_then(|ptr| read_ax_key_state(pid, window_id, ptr));
    result?;
    let changed =
        matches!((before, after), (Some(before), Some(after)) if ax_state_changed(&before, &after));
    Ok((changed, probe))
}

fn ax_state_changed(before: &AxKeyState, after: &AxKeyState) -> bool {
    matches!((&before.value, &after.value), (Some(before), Some(after)) if before != after)
        || matches!((before.selected, after.selected), (Some(before), Some(after)) if before != after)
}

fn action_record(confirmed: bool, foreground: bool) -> ActionExecutionRecord {
    let effect = if confirmed {
        ActionEffect::Confirmed
    } else {
        ActionEffect::Unverifiable
    };
    let transport = if foreground {
        ActionTransport::MacosCgEventHid
    } else {
        ActionTransport::MacosCgEventPid
    };
    let requested = if foreground {
        RequestedDelivery::Foreground
    } else {
        RequestedDelivery::Background
    };
    let actual = if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    };
    let mut record =
        ActionExecutionRecord::builder(effect, transport, requested).actual_delivery(actual);
    if confirmed {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::AccessibilityReadback,
            detail: "the same native AX element changed value or selection after the key post"
                .into(),
        });
    } else {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::NativeApiResult,
            detail: if foreground {
                "the key events were constructed and the foreground HID post was attempted".into()
            } else {
                "the key events were constructed and the PID-routed post was attempted".into()
            },
        });
    }
    record.build().expect("press_key record is valid")
}

fn delivery_failed(error: anyhow::Error) -> ToolResult {
    let message = format!("press_key delivery failed: {error}");
    let mut details = serde_json::json!({
        "code": "delivery_failed",
        "message": message,
    });
    if let Some(refusal) =
        error.downcast_ref::<crate::input::skylight::ForegroundActivationRefused>()
    {
        details["target_window_id"] = serde_json::json!(refusal.target_window_id);
        if let Some(focused_window_id) = refusal.focused_window_id {
            details["focused_window_id"] = serde_json::json!(focused_window_id);
        }
        if let Some(relation) = refusal.relation {
            details["focused_window_relation"] = serde_json::json!(relation);
        }
    }
    ToolResult::error(&message).with_structured(details)
}

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "press_key".into(),
        description: "Press and release a single key. Follows the same `delivery_mode` ladder as click/type_text \
            — it does NOT raise the window by default:\n\
            • `background` (default): post to the pid WITHOUT fronting/raising — the \
              auth-message path (Chromium-safe). With element_index it focuses that AX \
              element first. `window_id` only targets; it does not raise.\n\
            • `foreground`: guard and briefly front the exact window, focus an addressed AX \
              element when supplied, send a genuine HID key transition so Chromium content, \
              inline editors, and native menu equivalents receive it, then restore prior \
              frontmost. Requires window_id.\n\n\
            A key press is confirmed only when a bounded native AX value/selection read-back \
            changes on the same control. Otherwise a successfully attempted post remains \
            effect:\"unverifiable\" without implying delivery failure or recommending foreground. \
            Key names: return, tab, escape, up/down/left/right, space, delete, \
            home, end, pageup, pagedown, f1-f12, plus any letter or digit. \
            Modifiers array: cmd, shift, option/alt, ctrl, fn.\n\n\
            SHEETS (modal panels attached to a window): background Escape cannot \
            dismiss one. A sheet and the window it is attached to are two \
            same-pid keyboard destinations, so this refuses with \
            same_pid_keyboard_ambiguity at the sheet's window_id AND at the host \
            window's. Dismiss it the exact way instead: get_window_state on the \
            host window reports the sheet under related_windows, snapshot THAT \
            window_id, and `click` the sheet's Cancel/Close button with the \
            default action:\"press\" (AppKit sheets and their buttons do not \
            expose AXCancel, so click action:\"cancel\" returns \
            actionUnsupported).".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["key"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid": { "type": "integer" },
                "key": { "type": "string", "description": "Key name: return, tab, escape, up, down, etc." },
                "modifiers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys: cmd, shift, option/alt, ctrl, fn."
                },
                "window_id": { "type": "integer", "description": "Target window. Required for delivery_mode:\"foreground\". Does NOT itself raise the window — raising is gated on delivery_mode." },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "x": { "type": "number", "description": "Screenshot-pixel X — the element px action form: pixel-click there to focus, then send the key. Use when the key must go to a Chromium/Electron surface the AX path can't focus. Pass with y, no element_index." },
                "y": { "type": "number", "description": "Screenshot-pixel Y (see x)." },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "Use desktop with no pid/window_id to send the key to the frontmost application." },
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema(),
                "detect_window_change": { "type": "boolean", "description": "Default true: after the action the driver polls WindowServer for up to one second so the reply can name a window the action opened. Pass false when you enumerate windows yourself — the poll is then skipped (roughly a second off this call) and the reply carries no opened-window evidence." },
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

#[async_trait]
impl Tool for PressKeyTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<PressKeyInput>("press_key", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let key = input.key;
            let modifiers = input.modifiers.unwrap_or_default();
            let key_for_input = key.clone();
            let result = cua_driver_core::operation::spawn_blocking(move || {
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                crate::input::keyboard::press_key_global(&key_for_input, &modifier_refs)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed '{key}' on the desktop."))
                    .with_structured(serde_json::json!({
                        "scope": "desktop",
                        "path": "hid",
                        "effect": "unverifiable"
                    })),
                Ok(Err(error)) => ToolResult::error(format!("desktop press_key failed: {error}")),
                Err(error) => ToolResult::error(format!("desktop press_key task failed: {error}")),
            };
        }
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let key_raw = match args.require_str("key") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let mut modifiers: Vec<String> = args.str_array("modifiers");
        // Surface 6: element_token / element_index precedence resolution.
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id").map(|v| v as u32);
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "press_key",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None => (None, window_id_arg),
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: wid,
                element_index: idx,
                via_token: _,
            } => (Some(idx), wid),
        };

        if let Err(error) = validate_post_target(pid) {
            return delivery_failed(error);
        }

        // Remap "+" / "plus" → "=" + Shift (same physical key on US layout).
        let key = if key_raw == "+" || key_raw == "plus" {
            if !modifiers.iter().any(|m| m.eq_ignore_ascii_case("shift")) {
                modifiers.push("shift".to_string());
            }
            "=".to_string()
        } else {
            key_raw.clone()
        };
        let display_key = key_raw.clone();
        // delivery_mode gates the raise: background (default) never fronts the
        // window (auth-envelope post, even with window_id); foreground is the
        // explicit NSMenu-activation rung. Matches click/type_text/hotkey.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let fg = delivery_mode.is_foreground();

        // Argument-shape errors are reported before any gating or retained
        // lookups: a malformed call must fail the same way regardless of
        // background-target state.
        let px = args.get("x").and_then(|v| v.as_f64());
        let py = args.get("y").and_then(|v| v.as_f64());
        if px.is_some() && py.is_some() && element_index.is_some() {
            return ToolResult::error(
                "Pass either element_index (ax) or x,y (px) to press_key, not both.",
            );
        }

        // Resolve the pre-focus element pointer (if requested) outside
        // the suppression closure — only the focus_element() write itself
        // needs to run under suppression, the cache lookup does not.
        // Retain out of the cache so a concurrent get_window_state can't free
        // the element before the suppressed focus below dereferences it
        // (use-after-free → daemon crash). Guard lives to method end.
        let pre_focus_guard = if let (Some(idx), Some(wid)) = (element_index, window_id) {
            match self.state.element_cache.get_element_retained(pid, wid, idx) {
                Some(guard) => Some(guard),
                None => {
                    return ToolResult::error(format!(
                        "Element index {idx} not found. Call get_window_state first."
                    ));
                }
            }
        } else {
            None
        };
        let pre_focus_ptr: Option<usize> = pre_focus_guard.as_ref().map(|g| g.as_ptr());

        // ── Exact-target background gate (macOS background input v1) ──
        // A window-addressed background key is process-scoped transport: it
        // must prove exact delivery to the requested window (fresh AXWindows
        // membership, not minimized/hidden, no competing same-pid keyboard
        // destination, proven element ancestry) BEFORE anything is sent —
        // including the px focus click. delivery_mode:"foreground" stays the
        // caller's explicit last resort and is not gated here.
        let _mutation_lease = if !fg {
            if let Some(wid) = window_id {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    pre_focus_ptr,
                    cua_driver_core::background_input::BackgroundAction::GenericKey,
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

        // px form: pixel-click to focus, then the key goes to the focused element.
        // Reuses click's translation + delivery_mode; after it, deliver via the
        // plain background path (the focus-click already handled fronting if fg).
        let px_focus = {
            if let (Some(cx), Some(cy)) = (px, py) {
                let from_zoom = args
                    .get("from_zoom")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Err(e) = super::focus_by_pixel(
                    &self.state,
                    pid,
                    window_id,
                    cx,
                    cy,
                    fg,
                    args.opt_str("session"),
                    args.opt_str("_session_id"),
                    from_zoom,
                    _mutation_lease.as_ref(),
                )
                .await
                {
                    return e;
                }
                true
            } else {
                false
            }
        };

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Single-key presses can fire autocomplete (Return on a search
        // box opens a results popover) or trigger menu shortcuts that
        // open windows. Wrapping mirrors the hotkey path.
        //
        // The AX focus_element() pre-write also runs inside the closure
        // so any reflex activations it triggers are caught by both the
        // wildcard snapshot suppressor and the targeted FocusGuard lease.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "press_key.CGEvent",
            || async move {
                // Pre-focus the element under suppression so its
                // side-effects are captured by the snapshot + lease.
                if let Some(element_ptr) = pre_focus_ptr {
                    let _ = cua_driver_core::operation::spawn_blocking(move || {
                        crate::input::ax_actions::focus_element(element_ptr)
                    })
                    .await;
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }

                cua_driver_core::operation::spawn_blocking(move || {
                    let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    // Foreground rung: keep the exact target frontmost through a genuine
                    // physical HID key down/up pair, then restore. PID-routed events without the
                    // authentication envelope reach NSMenu, but Chromium/Electron may
                    // silently discard them even while frontmost; the guarded HID route
                    // is accepted by both. Skipped when px-focus already handled the
                    // target-specific foreground transition.
                    if fg && !px_focus {
                        let wid = window_id.ok_or_else(|| {
                            anyhow::anyhow!(
                                "delivery_mode=foreground requires window_id for press_key"
                            )
                        })?;
                        return dispatch_with_ax_oracle(pid, window_id, pre_focus_ptr, |capture| {
                            crate::input::skylight::with_foreground_hid_activation(
                                pid as libc::pid_t,
                                wid,
                                || {
                                    // Activation can change the first responder, so
                                    // repeat the best-effort AX focus write inside
                                    // the guarded foreground interval immediately
                                    // before the physical key transition.
                                    if let Some(element_ptr) = pre_focus_ptr {
                                        let _ =
                                            crate::input::ax_actions::focus_element(element_ptr);
                                    }
                                    capture();
                                    crate::input::keyboard::press_key_bare_global(&key, &m)
                                },
                            )
                        });
                    }
                    // background (default): auth-envelope post, no raise.
                    dispatch_with_ax_oracle(pid, window_id, pre_focus_ptr, |capture| {
                        capture();
                        crate::input::keyboard::press_key(pid, &key, &m)
                    })
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot, &args).await;

        let delivery_outcome = match result {
            Ok(result) => map_delivery_outcome(result),
            Err(error) => {
                PressKeyDeliveryOutcome::Failed(anyhow::anyhow!("posting task failed: {error}"))
            }
        };

        let (confirmed, probe) = match delivery_outcome {
            PressKeyDeliveryOutcome::Confirmed => (true, None),
            PressKeyDeliveryOutcome::Unverifiable(probe) => (false, probe),
            PressKeyDeliveryOutcome::Failed(error) => return delivery_failed(error),
        };
        let label = if fg {
            " (delivery_mode:foreground)"
        } else {
            ""
        };
        let evidence = if confirmed {
            None
        } else if changes.needs_restore() {
            probe.as_ref().map(|probe| {
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
            "✅ Pressed {display_key} on pid {pid}{label}.{}",
            changes.result_suffix()
        );
        let mut structured = serde_json::json!({
            "path": key_path(fg, fg && !px_focus),
            "verified": confirmed,
            "effect": if confirmed { "confirmed" } else { "unverifiable" },
        });
        if let Some(outcome) = evidence {
            delivery_probe::apply_evidence(
                &mut msg,
                &mut structured,
                outcome,
                key_noop_report(changes.polled),
                window_change.as_ref(),
            );
        }
        ToolResult::text(msg)
            .with_structured(structured)
            .with_action_record(action_record(confirmed, fg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_outcome_mapper_distinguishes_confirmed_unverifiable_and_failed() {
        assert!(matches!(
            map_delivery_outcome(Ok((true, None))),
            PressKeyDeliveryOutcome::Confirmed
        ));
        assert!(matches!(
            map_delivery_outcome(Ok((false, None))),
            PressKeyDeliveryOutcome::Unverifiable(None)
        ));
        let failed = map_delivery_outcome(Err(anyhow::anyhow!("post rejected")));
        assert!(matches!(failed, PressKeyDeliveryOutcome::Failed(_)));
    }

    #[test]
    fn accepted_without_oracle_has_no_escalation_but_ax_change_confirms() {
        let unverifiable = action_record(false, false).public_result().unwrap();
        assert_eq!(
            unverifiable.effect,
            cua_driver_contract::ActionEffect::Unverifiable
        );
        assert_eq!(
            unverifiable.delivery.unwrap().mode,
            cua_driver_contract::ActionDeliveryMode::Background
        );
        assert!(unverifiable.escalation.is_none());

        let confirmed = action_record(true, false).public_result().unwrap();
        assert_eq!(
            confirmed.effect,
            cua_driver_contract::ActionEffect::Confirmed
        );
        assert_eq!(confirmed.evidence.unwrap().len(), 1);
        assert!(confirmed.escalation.is_none());
    }

    #[test]
    fn definitely_dead_pid_is_a_typed_delivery_failure() {
        let child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id() as i32;
        let mut child = child;
        child.wait().expect("wait for child exit");
        let failure = delivery_failed(validate_post_target(pid).unwrap_err());
        assert_eq!(failure.is_error, Some(true));
        assert_eq!(
            failure.structured_content.unwrap()["code"],
            "delivery_failed"
        );
    }

    #[test]
    fn a_sheet_that_held_focus_is_named_in_the_refusal() {
        let failure = delivery_failed(
            crate::input::skylight::ForegroundActivationRefused {
                target_window_id: 11139,
                focused_window_id: Some(11151),
                relation: Some(crate::ax::window_scope::SHEET_RELATION),
            }
            .into(),
        );
        let details = failure.structured_content.expect("structured refusal");
        assert_eq!(details["code"], "delivery_failed");
        assert_eq!(details["focused_window_id"], 11151);
        assert_eq!(details["target_window_id"], 11139);
        assert_eq!(details["focused_window_relation"], "sheet");
        assert!(
            details["message"]
                .as_str()
                .expect("message")
                .contains("window 11151, a sheet attached to 11139"),
            "{details}"
        );
    }

    #[test]
    fn an_unidentified_focus_thief_leaves_the_refusal_unembellished() {
        let failure = delivery_failed(
            crate::input::skylight::ForegroundActivationRefused {
                target_window_id: 11139,
                focused_window_id: None,
                relation: None,
            }
            .into(),
        );
        let details = failure.structured_content.expect("structured refusal");
        assert!(details.get("focused_window_id").is_none(), "{details}");
        assert!(
            details.get("focused_window_relation").is_none(),
            "{details}"
        );
        let message = details["message"].as_str().expect("refusal message");
        assert!(
            !message.contains("holds keyboard focus"),
            "an unidentified thief must not be described as one: {message}"
        );
        assert!(!message.contains("11139"), "{message}");
    }
}
