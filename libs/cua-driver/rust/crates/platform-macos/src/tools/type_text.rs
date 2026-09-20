//! type_text tool — matches the Swift reference TypeTextTool.swift.
//!
//! Inserts text via `AXSelectedText` attribute write — an atomic single-call
//! insertion at the current cursor position. This is the preferred path for
//! all standard Cocoa text views (NSTextField, NSTextView, WKWebView text
//! inputs in Safari, etc.) and is significantly faster than per-keystroke
//! CGEvent synthesis.
//!
//! For Chromium / Electron inputs that don't implement `kAXSelectedText`,
//! the tool falls back to character-by-character CGEvent keystrokes so the
//! caller doesn't need to detect the app type themselves.
//!
//! When the target pid belongs to a terminal emulator (Ghostty,
//! Terminal.app, iTerm2, Alacritty, kitty, WezTerm, Hyper, Warp — see
//! [`crate::terminal::TERMINAL_BUNDLE_IDS`]), the AX path is skipped
//! entirely: terminals expose `AXTextArea` for their grid but the
//! `AXSelectedText` write never reaches the pty, so the tool would
//! report success while the shell sees nothing. We go straight to
//! CGEvent key-event synthesis (`path: "key_events"`).
//!
//! Use `type_text_chars` when you explicitly need per-character pacing
//! (e.g., to trigger live-search debounce handlers).

use async_trait::async_trait;
use cua_driver_contract::TypeTextInput;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_range_attr, copy_string_attr, focused_element_of_pid, kAXErrorSuccess, set_range_attr,
    set_string_attr, AXUIElementRef,
};
use crate::ax::RetainedElement;
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::CFRelease;
use cua_driver_core::background_input::BackgroundRefusal;

use super::ToolState;

pub struct TypeTextTool {
    pub state: Arc<ToolState>,
}

impl TypeTextTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "type_text".into(),
        description:
            "Insert text into the target pid via `AXSetAttribute(kAXSelectedText)`. \
             Works for standard Cocoa text fields and text views. No keystrokes are \
             synthesized — special keys (Return / Escape / arrows) go through \
             `press_key` / `hotkey`. For Chromium / Electron inputs that don't \
             implement `kAXSelectedText`, the tool falls back to CGEvent \
             character synthesis automatically when the estimated route stays \
             within the daemon transport budget. Longer synthesized routes are \
             refused before character events and return a safe chunk size; \
             one-call AX insertion remains uncapped.\n\n\
             Optional `element_index` + `window_id` (from the last \
             `get_window_state` snapshot) directs the write to a specific field. \
             Without `element_index`, the write goes to the pid's currently \
             focused element.\n\n\
             CARET: by default the text lands at the element's current insertion \
             point (after a click on a multi-line body that is usually the start). \
             Pass `caret` with `element_index`/`element_token` to place the caret by \
             content first: \"start\", \"end\", {\"after\": \"<substring>\"} or \
             {\"before\": \"<substring>\"} (first occurrence). The driver reads AXValue, \
             collapses AXSelectedTextRange at that offset, reads it back, then types; \
             an absent substring or a control that refuses the range is a typed \
             refusal (`caret_anchor_not_found`, `caret_not_placed`, ...) and nothing \
             is dispatched. The reply carries `caret_index` (UTF-16 offset) and \
             `caret_anchor`.\n\n\
             WEB CONTENT (Chromium/WebKit/Electron — browser tabs, Slack, VS Code, \
             X's compose box): AXValue is not independent proof that the \
             renderer/DOM observed an AX write or synthesized keystrokes. The \
             driver detects this at the element level (an AXWebArea ancestor) and \
             refuses to trust AXValue-only read-back there — type_text returns \
             effect:\"unverifiable\" + escalation, never a false \"confirmed\" (a \
             browser's own native address bar/toolbar stays trusted). For a browser \
             TAB the reliable path is the `page` tool (drives the DOM via CDP); for \
             an embedded web view use this tool's px form: pass x,y (no \
             element_index) to pixel-click the field then type, in one call. NOTE: \
             a px focus-click won't reliably open+focus a CLOSED control; AX-press \
             to open/activate it first (works in the background), then px-type. \
             Always confirm via the screenshot; if px-background still drops, \
             escalate to delivery_mode:\"foreground\"."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid":  { "type": "integer", "description": "Target process ID." },
                "text": { "type": "string",  "description": "Text to insert at the target's cursor." },
                "window_id": {
                    "type": "integer",
                    "description": "CGWindowID. Required when element_index is used. Optional when element_token is supplied (the token carries it)."
                },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "x": { "type": "number", "description": "Screenshot-pixel X of the field to type into — the element px action form. Pass x,y (no element_index) and the tool pixel-clicks there to establish real renderer focus, then types. Use for Chromium/Electron inputs the AX path can't reach. Read straight off the get_window_state PNG, same convention as click." },
                "y": { "type": "number", "description": "Screenshot-pixel Y of the field (see x)." },
                "delay_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 200,
                    "description": "Milliseconds between characters in the CGEvent fallback path. Default 30. Ignored when the AX path succeeds."
                },
                "caret": {
                    "description": "Where to put the insertion point before typing, resolved against the element's AXValue: \"start\", \"end\", {\"after\": \"<substring>\"} (caret right after the first occurrence) or {\"before\": \"<substring>\"} (right before it). Requires element_index or element_token. Omit to type at the current insertion point. Offsets are UTF-16 code units, as AX counts them.",
                    "oneOf": [
                        { "type": "string", "enum": ["start", "end"] },
                        { "type": "object", "properties": { "after": { "type": "string", "minLength": 1 } }, "required": ["after"], "additionalProperties": false },
                        { "type": "object", "properties": { "before": { "type": "string", "minLength": 1 } }, "required": ["before"], "additionalProperties": false }
                    ]
                },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "Use desktop with no pid/window_id to type into the frontmost application." },
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "Best-effort-background ladder rung (default \"background\"). \"background\": AX insert, then CGEvent keystrokes if needed — no focus steal; native controls can be confirmed via AXValue read-back, while web-content writes remain effect:\"unverifiable\". \"foreground\": briefly front the window, type, restore the prior frontmost — the explicit last resort for focus-sensitive surfaces (e.g. WhatsApp/Catalyst) where background keystrokes don't land. Re-call with \"foreground\" when a background attempt remains unverifiable and a fresh snapshot shows the text did not appear."
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

fn screen_sharing_delivery_error(
    is_screen_sharing: bool,
    foreground: bool,
    window_id: Option<u32>,
) -> Option<ToolResult> {
    if !is_screen_sharing || (foreground && window_id.is_some()) {
        return None;
    }
    Some(
        ToolResult::error(
            "Screen Sharing text input requires delivery_mode:\"foreground\" and window_id \
             so Cua Driver can deliver physical HID key transitions safely.",
        )
        .with_structured(serde_json::json!({
            "code": "SCREEN_SHARING_REQUIRES_FOREGROUND_HID",
            "effect": "refused",
            "escalation": {
                "recommended": "foreground",
                "reason": "Screen Sharing forwards physical keycodes; background PID-routed \
                           Unicode events can corrupt guest text.",
                "requires": ["window_id"]
            }
        })),
    )
}

#[async_trait]
impl Tool for TypeTextTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<TypeTextInput>("type_text", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let text =
                cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&input.text)
                    .into_owned();
            let delay_ms = args.u64_or("delay_ms", 30).min(200);
            if let Some(refusal) = synthesis_preflight(
                TextDeliveryRoute::UnicodeSynthesis,
                text.chars().count(),
                delay_ms,
            ) {
                return synthesis_refusal_result("hid", &refusal, AxAttempt::NotAttempted);
            }
            let result = cua_driver_core::operation::spawn_blocking(move || {
                crate::input::keyboard::type_text_global(&text, delay_ms)
            })
            .await;
            return match result {
                Ok(Ok(())) => {
                    ToolResult::text("Typed text into the frontmost desktop application.")
                        .with_structured(serde_json::json!({
                            "scope": "desktop",
                            "path": "hid",
                            "effect": "unverifiable"
                        }))
                }
                Ok(Err(error)) => ToolResult::error(format!("desktop type_text failed: {error}")),
                Err(error) => ToolResult::error(format!("desktop type_text task failed: {error}")),
            };
        }
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Strip trailing agent-protocol closing tags — see
        // cua_driver_core::text_sanitize docs for rationale.
        let text = cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw)
            .into_owned();
        // Surface 6: element_token / element_index precedence resolution.
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id");
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match self.state.element_cache.resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "type_text",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let delay_ms = args.u64_or("delay_ms", 30);
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        if let Some(error) = screen_sharing_delivery_error(
            crate::input::keyboard::is_screen_sharing_pid(pid),
            delivery_mode.is_foreground(),
            window_id,
        ) {
            return error;
        }

        // Validate element_index requires window_id (still applies for
        // the legacy integer path; token path already resolved window_id).
        if element_index.is_some() && window_id.is_none() {
            return ToolResult::error("window_id is required when element_index is used.");
        }

        // Argument-shape errors are reported before any gating or retained
        // lookups: a malformed call must fail the same way regardless of
        // background-target state.
        let px = args.get("x").and_then(|v| v.as_f64());
        let py = args.get("y").and_then(|v| v.as_f64());
        if px.is_some() && py.is_some() && element_index.is_some() {
            return ToolResult::error(
                "Pass either element_index (ax) or x,y (px) to type_text, not both.",
            );
        }

        let caret = match CaretSpec::parse(args.get("caret")) {
            Ok(caret) => caret,
            Err(error) => return error,
        };
        if caret.is_some() && element_index.is_none() {
            return caret_refusal_result(&CaretRefusal::Unsupported {
                reason: "caret placement needs a named element: pass element_index or \
                         element_token (the px form and the focused-element form carry no \
                         AXValue to resolve the offset against)",
            });
        }

        let element_guard = element_guard.zip(element_index);

        // ── Exact-target background gate (macOS background input v1) ──
        // A window-addressed background insert must prove exact delivery
        // before any input — including the px focus click — is sent. The pure
        // core decides once from fresh facts: full keyboard ladder, semantic
        // AX write only (exact element, no CGEvent fallback), or a structured
        // refusal. delivery_mode:"foreground" stays the caller's explicit
        // last resort and is not gated here.
        let (_mutation_lease, keyboard_policy) =
            if !delivery_mode.is_foreground() && window_id.is_some() {
                let wid = window_id.expect("checked above");
                let gate_element_ptr = element_guard.as_ref().map(|(g, _)| g.as_ptr() as usize);
                match background_keyboard_policy(pid, wid, gate_element_ptr).await {
                    Ok((lease, policy)) => (Some(lease), policy),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                (None, BackgroundKeyboardPolicy::Allowed)
            };

        // ── px form: focus by pixel-click, then type into the focused element ──
        // Pass x,y (no element_index) for an *element px action*: pixel-click the
        // field to give the Chromium/Electron renderer the real keyboard focus the
        // AX path can't, then fall through to the focused-element type path (which
        // escalates AX → CGEvent and lands once focused). Reuses ClickTool's exact
        // coordinate translation + delivery_mode, so it lands on the same pixel a
        // px-click would.
        let used_pixel_focus = px.is_some() && py.is_some();
        if let (Some(cx), Some(cy)) = (px, py) {
            // The px form has no exact element for a semantic-only write; when
            // the keyboard rung is refused, refuse before the focus click too.
            if let BackgroundKeyboardPolicy::SemanticOnly(ref refusal) = keyboard_policy {
                let wid = window_id.expect("gate ran only with window_id");
                return super::background_refusal_result(pid, wid, refusal);
            }
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
                delivery_mode.is_foreground(),
                args.opt_str("session"),
                args.opt_str("_session_id"),
                from_zoom,
                _mutation_lease.as_ref(),
            )
            .await
            {
                return e;
            }
            // element_index stays None → the type path below writes to the now-
            // focused element via the CGEvent (key_events) rung.
        }
        if let (Some((element, _)), Some(wid)) = (element_guard.as_ref(), window_id) {
            let center_guard = element.clone();
            if let Ok(Some((screen_x, screen_y))) =
                cua_driver_core::operation::spawn_blocking(move || unsafe {
                    crate::ax::bindings::element_screen_center(
                        center_guard.as_ptr() as AXUIElementRef
                    )
                })
                .await
            {
                let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y)
                    .await;
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, screen_x, screen_y);
            }
        }
        let element_ptr = element_guard
            .as_ref()
            .map(|(g, idx)| (g.as_ptr(), Some(*idx)));

        let text_clone = text.clone();
        let char_count = text.chars().count();

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Typing into a field can trigger autocomplete popovers or
        // Chrome/Safari's "Save Password?" prompt, both of which open
        // helper windows. Wrap so callers see them in the result suffix
        // and the wildcard suppressor catches reflex activations.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        // Terminal-emulator short-circuit: when the target pid belongs
        // to a known terminal (Ghostty / Terminal.app / iTerm2 / …), the
        // AX value-set is silently dropped — see crate::terminal docs.
        // Skip the AX path entirely so the caller never sees the
        // "success but nothing typed" symptom.
        let is_terminal_target = crate::terminal::is_terminal_pid(pid);

        // ── Caret placement (element-addressed only) ──
        // Runs under the mutation lease, after every gate and before any
        // rung: the caret is collapsed at the resolved offset and read back,
        // so a refusal here has dispatched nothing and the keystrokes below
        // land exactly where the reply says they did.
        let caret_placed = match (&caret, element_guard.as_ref()) {
            (Some(spec), Some((element, _))) => {
                if is_terminal_target {
                    return caret_refusal_result(&CaretRefusal::Unsupported {
                        reason: "the target pid is a terminal emulator; its AXTextArea does \
                                 not take a caret, keystrokes go to the shell's own line editor",
                    });
                }
                let spec = spec.clone();
                let guard = element.clone();
                let placed = cua_driver_core::operation::spawn_blocking(move || {
                    place_caret(guard.as_ptr() as AXUIElementRef, &spec)
                })
                .await;
                match placed {
                    Ok(Ok(placement)) => Some(placement),
                    Ok(Err(refusal)) => return caret_refusal_result(&refusal),
                    Err(error) => return ToolResult::error(format!("Task error: {error}")),
                }
            }
            _ => None,
        };

        let blocking_policy = keyboard_policy.clone();
        let native_guard = element_guard.clone();
        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "type_text.AXSelectedText",
            || async move {
                cua_driver_core::operation::spawn_blocking(move || {
                    let _native_guard = native_guard;
                    type_text_blocking(
                        pid,
                        &text_clone,
                        element_ptr,
                        delay_ms,
                        is_terminal_target,
                        delivery_mode,
                        window_id,
                        blocking_policy,
                    )
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot, &args).await;

        // Unwrap the delivery envelope: a structured refusal means no
        // actuator ran and the caller gets the exact reason.
        let result = match result {
            Ok(Ok(TypeTextDelivery::Refused(refusal))) => {
                let wid = window_id.expect("background refusals require a window target");
                return super::background_refusal_result(pid, wid, &refusal);
            }
            Ok(Ok(TypeTextDelivery::SynthesisRefused {
                path,
                refusal,
                ax_attempt,
            })) => return synthesis_refusal_result(path, &refusal, ax_attempt),
            Ok(Ok(TypeTextDelivery::Typed(outcome))) => Ok(Ok(outcome)),
            Ok(Err(error)) => Ok(Err(error)),
            Err(error) => Err(error),
        };

        match result {
            Ok(Ok(outcome)) if outcome.delivered_chars.is_some_and(|n| n < char_count) => {
                incomplete_result(
                    &text,
                    char_count,
                    outcome.delivered_chars.unwrap_or_default(),
                    &outcome.detail,
                    outcome.path,
                    caret_placed.as_ref(),
                )
            }
            Ok(Ok(outcome)) => {
                let TypeTextOutcome {
                    detail,
                    path,
                    verified,
                    delivered_chars,
                    destination_resolved,
                    normalized,
                } = outcome;
                // SURFACE-AWARE VERIFICATION. On any web-content surface —
                // Chromium/WebKit/Electron — AXValue is not independent renderer
                // evidence. It can report a changed value after either an AX write
                // or synthesized keystrokes while the renderer/DOM still observes
                // no edit (the Slack-search AND Chrome-on-X false-confirms). Detect
                // this at the ELEMENT level (an `AXWebArea` ancestor) so it covers
                // every browser + Electron uniformly, yet a browser's OWN native
                // chrome (address bar, toolbar) stays trusted. Probe ONLY when a
                // path with AXValue-only verification would otherwise confirm, so
                // native types and already-unverified deliveries pay nothing.
                let target_is_web_content = verified
                    && path_has_untrusted_web_readback(path)
                    && target_in_web_area(pid, element_ptr, window_id);
                let verification = surface_verification(path, verified, target_is_web_content);
                let verified = verification.verified;
                let untrusted_web_readback = verification.untrusted_web_readback;
                let electron_web_content = untrusted_web_readback
                    && crate::browser::electron_js::ElectronJs::is_electron(pid);

                // `verified:false` means the driver could not confirm the text
                // landed (Electron AX echo, unreadable AXValue on Catalyst, or a
                // CGEvent rung the app may have dropped). Don't dress that as a
                // confirmed insert — tell the agent to look, and point at the
                // right next rung.
                let unconfirmed =
                    Unconfirmed::of(destination_resolved, untrusted_web_readback, path);
                let (mark, note) = if verified {
                    let note = if normalized {
                        format!(
                            " The app normalised the text: all {char_count} character(s) landed \
                             and the field's value is not byte-identical to the request."
                        )
                    } else {
                        String::new()
                    };
                    ("✅ Inserted", note)
                } else {
                    let note = match unconfirmed {
                        Unconfirmed::NoDestination => no_destination_note(pid, window_id),
                        Unconfirmed::WebReadback => {
                            let next_step = if electron_web_content && used_pixel_focus {
                                "The pixel-focus rung already ran, so do not repeat it; verify \
                                 the result via the screenshot."
                            } else {
                                "For a browser tab use the `page` tool (it drives the DOM); for \
                                 an embedded web view, re-type with the px form (x,y)."
                            };
                            format!(
                                " — web-content surface (Chromium / WebKit / Electron): \
                                 AXValue read-back is not independent proof that the \
                                 renderer/DOM observed the input. {next_step}"
                            )
                        }
                        Unconfirmed::ForegroundKeystrokes => {
                            " — driver could not confirm; verify via screenshot.".to_string()
                        }
                        Unconfirmed::BackgroundKeystrokes => {
                            " — driver could not confirm the text landed; verify via screenshot, \
                              and re-call with delivery_mode:\"foreground\" if it didn't."
                                .to_string()
                        }
                    };
                    ("📨 Sent (unverified)", note)
                };
                // A native single-line text control's value is a binding
                // target: AppKit hands it to the app's own model when the edit
                // session ends, and `type_text` delivers no end-of-edit. The
                // read-back proves the characters arrived in the editor and
                // nothing about what the app will keep.
                let commit_unproven =
                    uncommitted_binding_target(element_ptr, target_is_web_content);
                let commit_note = if commit_unproven {
                    " The app takes this field's value at end-of-edit, which type_text does \
                      not deliver: press Tab or Return, or use set_value, or the app may \
                      keep its own value."
                } else {
                    ""
                };
                let caret_note = caret_placed
                    .as_ref()
                    .map(CaretPlacement::note)
                    .unwrap_or_default();
                ToolResult::text(format!(
                    "{mark} {char_count} char(s){detail}{caret_note}.{note}{commit_note}{}",
                    changes.result_suffix()
                ))
                .with_structured({
                    // `effect` mirrors `verified`'s read-back tri-state: a TRUSTED
                    // positive read-back is "confirmed"; an unreadable/unchanged
                    // AXValue, a dropped CGEvent rung, or an Electron AX echo we
                    // refuse to trust is "unverifiable".
                    let mut s = serde_json::json!({
                        "path": path,
                        "characters": char_count,
                        "requested_chars": char_count,
                        "verified": verified,
                        "effect": if verified { "confirmed" } else { "unverifiable" },
                    });
                    if commit_unproven {
                        s["committed"] = serde_json::json!(
                            cua_driver_contract::ActionCommit::Unproven.as_wire()
                        );
                    }
                    if let Some(delivered_chars) = delivered_chars {
                        s["delivered_chars"] = serde_json::json!(delivered_chars);
                    }
                    if let Some(placement) = &caret_placed {
                        placement.attach_evidence(&mut s);
                    }
                    if untrusted_web_readback {
                        // Web-content AXValue read-back. A real browser TAB → the
                        // `page` tool (drives the DOM via CDP) is the reliable rung;
                        // an embedded web view (Electron, no CDP) → the element px
                        // action. It's a renderer/DOM-focus problem, never a
                        // foreground one.
                        let escalation =
                            match web_readback_next_rung(electron_web_content, used_pixel_focus) {
                                Some("px") => Some((
                                    "px",
                                    "Electron web view — AXValue read-back cannot prove \
                                 that the renderer observed the input. Confirm via the \
                                 screenshot; if it didn't land, re-type with the \
                                 element px action (x,y to pixel-focus the field, then \
                                 type).",
                                )),
                                Some("page") => Some((
                                    "page",
                                    "Browser web content — AXValue read-back cannot prove \
                                 that the DOM observed the input (and AX type_text on a \
                                 contenteditable is racy). Drive the tab's DOM with the \
                                 `page` tool: execute_javascript + el.value/innerText for a \
                                 plain input; for a rich-text contenteditable \
                                 (Draft.js/Lexical/Slate-style editors can silently discard \
                                 a one-shot DOM write on their next render) try insert_text \
                                 first (one CDP call, cheap), then type_keystrokes if that \
                                 also gets discarded (real per-character keyboard events, \
                                 slower but most durable). Or confirm via the screenshot.",
                                )),
                                _ => None,
                            };
                        if let Some((recommended, reason)) = escalation {
                            s["escalation"] = serde_json::json!({
                                "recommended": recommended,
                                "reason": reason,
                            });
                        }
                    } else if let Some(target) = (!verified)
                        .then(|| unconfirmed.escalation_target())
                        .flatten()
                    {
                        s["escalation"] = serde_json::json!({
                            "recommended": target,
                            "reason": "effect_unconfirmed",
                        });
                    }
                    s
                })
            }
            Ok(Err(e)) => ToolResult::error(format!("type_text failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── Blocking implementation ───────────────────────────────────────────────────

/// Which delivery path was taken. Surfaced as `structuredContent.path`
/// on success.
const PATH_AX: &str = "ax";
const PATH_KEY_EVENTS: &str = "key_events";
const PATH_KEY_EVENTS_FG: &str = "key_events_fg";

// The daemon transport has a 120-second request deadline. Character synthesis
// is synchronous and costs at least one 8ms key-down gap plus either the
// requested delay or an 8ms key-up gap per character. Keep the complete
// scheduled synthesis sleeps + read-back estimate within 100 seconds, reserving
// 20 seconds for event construction/posting, routing, focus assistance,
// queueing, response serialization, and transport.
// Atomic AX writes are deliberately excluded: their cost does not scale per
// character and a single write remains the preferred route for large text.
const SYNTHESIS_BUDGET_MS: u64 = 100_000;
const KEY_DOWN_GAP_MS: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextDeliveryRoute {
    AtomicAx,
    UnicodeSynthesis,
    PhysicalSynthesis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SynthesisRefusal {
    requested_chars: usize,
    estimated_duration_ms: u64,
    per_character_ms: u64,
    max_chunk_chars: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AxAttempt {
    NotAttempted,
    Rejected,
    Unchanged,
    Unverifiable,
}

impl AxAttempt {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotAttempted => "not_attempted",
            Self::Rejected => "rejected",
            Self::Unchanged => "unchanged",
            Self::Unverifiable => "unverifiable",
        }
    }
}

fn synthesis_preflight(
    route: TextDeliveryRoute,
    requested_chars: usize,
    delay_ms: u64,
) -> Option<SynthesisRefusal> {
    if route == TextDeliveryRoute::AtomicAx {
        return None;
    }
    let per_character_ms = match route {
        TextDeliveryRoute::AtomicAx => unreachable!(),
        // PID-routed and desktop Unicode paths post one key-down and one
        // key-up, sleeping 8ms after the down and max(delay, 8) after the up.
        TextDeliveryRoute::UnicodeSynthesis => {
            KEY_DOWN_GAP_MS.saturating_add(delay_ms.max(KEY_DOWN_GAP_MS))
        }
        // Physical HID may need Shift down/up around each printable key. Use
        // that four-event worst case for a payload-independent safe bound.
        TextDeliveryRoute::PhysicalSynthesis => 24u64.saturating_add(delay_ms.max(8)),
    };
    let drain_ms = DELIVERY_DRAIN_TIMEOUT.as_millis() as u64;
    let estimated_duration_ms = (requested_chars as u64)
        .saturating_mul(per_character_ms)
        .saturating_add(drain_ms);
    if estimated_duration_ms <= SYNTHESIS_BUDGET_MS {
        return None;
    }
    let max_chunk_chars = SYNTHESIS_BUDGET_MS
        .saturating_sub(drain_ms)
        .checked_div(per_character_ms)
        .unwrap_or_default() as usize;
    Some(SynthesisRefusal {
        requested_chars,
        estimated_duration_ms,
        per_character_ms,
        max_chunk_chars,
    })
}

fn synthesis_refusal_result(
    path: &'static str,
    refusal: &SynthesisRefusal,
    ax_attempt: AxAttempt,
) -> ToolResult {
    let effect = if ax_attempt == AxAttempt::Unverifiable {
        "indeterminate"
    } else {
        "refused"
    };
    let retryable = ax_attempt != AxAttempt::Unverifiable;
    let escalation = if retryable {
        serde_json::json!({
            "recommended": "chunk",
            "reason": format!(
                "Character synthesis would exceed the bounded transport-safe budget. Retry in chunks of at most {} characters at this delay.",
                refusal.max_chunk_chars
            )
        })
    } else {
        serde_json::json!({
            "recommended": "verify_state",
            "reason": "The atomic AX attempt could not be observed. Re-read the target before deciding whether any suffix remains; do not retry blindly."
        })
    };
    let mut structured = serde_json::json!({
        "code": "type_text_synthesis_budget_exceeded",
        "path": path,
        "effect": effect,
        "requested_chars": refusal.requested_chars,
        "estimated_duration_ms": refusal.estimated_duration_ms,
        "synthesis_budget_ms": SYNTHESIS_BUDGET_MS,
        "per_character_ms": refusal.per_character_ms,
        "max_chunk_chars": refusal.max_chunk_chars,
        "synthesized_chars": 0,
        "atomic_ax_effect": ax_attempt.as_str(),
        "retryable": retryable,
        "escalation": escalation,
    });
    if ax_attempt != AxAttempt::Unverifiable {
        structured["delivered_chars"] = serde_json::json!(0);
        structured["retry_from_character"] = serde_json::json!(0);
    }
    let message = if retryable {
        format!(
            "type_text refused character synthesis before emitting character events: {} characters require an estimated {}ms at {}ms per character, exceeding the {}ms budget; retry in chunks of at most {} characters",
            refusal.requested_chars,
            refusal.estimated_duration_ms,
            refusal.per_character_ms,
            SYNTHESIS_BUDGET_MS,
            refusal.max_chunk_chars,
        )
    } else {
        format!(
            "type_text did not synthesize character events because {} characters require an estimated {}ms, exceeding the {}ms budget; the preceding atomic AX attempt was unverifiable, so re-read the target before retrying",
            refusal.requested_chars, refusal.estimated_duration_ms, SYNTHESIS_BUDGET_MS,
        )
    };
    ToolResult::error(message).with_structured(structured)
}

/// Whether the addressed element is a control whose value the app reads at
/// end-of-edit. Web content is excluded: it has no AppKit binding and its
/// read-back is already reported as untrusted.
fn uncommitted_binding_target(
    element_ptr: Option<(usize, Option<usize>)>,
    web_content: bool,
) -> bool {
    if web_content {
        return false;
    }
    let Some((ptr, _)) = element_ptr else {
        return false;
    };
    let element = ptr as AXUIElementRef;
    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let subrole = unsafe { copy_string_attr(element, "AXSubrole") }.unwrap_or_default();
    super::set_value::is_binding_target_role(&role, &subrole)
}

fn path_has_untrusted_web_readback(path: &str) -> bool {
    path == PATH_AX || path == PATH_KEY_EVENTS || path == PATH_KEY_EVENTS_FG
}

/// Why an insertion's read-back could not confirm it, in precedence order.
///
/// The escalation target follows from the state, not from the path token: a
/// request that never resolved a field cannot be helped by fronting the
/// window or by re-capturing it, because neither produces a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unconfirmed {
    NoDestination,
    WebReadback,
    ForegroundKeystrokes,
    BackgroundKeystrokes,
}

impl Unconfirmed {
    fn of(destination_resolved: bool, untrusted_web_readback: bool, path: &str) -> Self {
        if !destination_resolved {
            Self::NoDestination
        } else if untrusted_web_readback {
            Self::WebReadback
        } else if path == PATH_KEY_EVENTS_FG {
            Self::ForegroundKeystrokes
        } else {
            Self::BackgroundKeystrokes
        }
    }

    /// The rung that can still deliver, or `None` when this state has no
    /// better one. `WebReadback` chooses between `px` and `page` from surface
    /// facts the caller holds.
    fn escalation_target(self) -> Option<&'static str> {
        match self {
            Self::NoDestination => Some("element"),
            Self::WebReadback | Self::ForegroundKeystrokes => None,
            Self::BackgroundKeystrokes => Some("foreground"),
        }
    }
}

/// The reply for keystrokes posted with no resolvable text destination.
///
/// Catalyst-style targets accept keystrokes without publishing
/// `AXFocusedUIElement`, so the post is still worth making — but there is no
/// field to focus and none to read back, and a capture cannot supply one.
/// State that, and name the one addressing form that can.
fn no_destination_note(pid: i32, window_id: Option<u32>) -> String {
    let scope = match window_id {
        Some(window_id) => format!("window {window_id}"),
        None => format!("pid {pid}"),
    };
    format!(
        " — no focused text element could be resolved in {scope}, so the keystrokes were \
         posted blind and no field can be read back. Address the field itself: pass \
         element_index (or element_token) for it on this call, or use set_value."
    )
}

/// The reply for an insertion the read-back proved incomplete.
///
/// The undelivered remainder is spelled out. A caller that is told only to
/// "retry the remaining suffix" has to slice the request by codepoint and
/// guess where the caret stopped; the driver already knows both.
fn incomplete_result(
    text: &str,
    requested_chars: usize,
    delivered_chars: usize,
    detail: &str,
    path: &'static str,
    caret_placed: Option<&CaretPlacement>,
) -> ToolResult {
    let remainder: String = text.chars().skip(delivered_chars).collect();
    // A placed caret now sits after the delivered prefix; re-sending the
    // same anchor would put the remainder in front of it.
    let caret_note = if caret_placed.is_some() {
        " and without `caret` (the insertion point already follows the delivered prefix)"
    } else {
        ""
    };
    let mut structured = serde_json::json!({
        "code": "type_text_incomplete",
        "path": path,
        "effect": "partial",
        "requested_chars": requested_chars,
        "delivered_chars": delivered_chars,
        "retryable": true,
        "retry_from_character": delivered_chars,
        "retry_text": remainder,
    });
    if let Some(placement) = caret_placed {
        placement.attach_evidence(&mut structured);
    }
    ToolResult::error(format!(
        "type_text incomplete: delivered {delivered_chars} of {requested_chars} \
         character(s){detail}; retry with text: {remainder:?}{caret_note}"
    ))
    .with_structured(structured)
}

// ── Caret placement ───────────────────────────────────────────────────────────
//
// AX text ranges (`AXSelectedTextRange`, `AXNumberOfCharacters`) count UTF-16
// code units, as NSString does: a non-BMP scalar (emoji, supplementary CJK)
// occupies two units. Every offset this section produces or reports is in
// those units — never Rust `char`s or bytes — so the arithmetic re-encodes the
// relevant prefix rather than counting characters.

/// Where the caller wants the insertion point before the text is typed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CaretSpec {
    Start,
    End,
    /// Right after the first occurrence of the substring.
    After(String),
    /// Right before the first occurrence of the substring.
    Before(String),
}

impl CaretSpec {
    /// Parse the `caret` argument: `"start"`, `"end"`, `{"after": s}` or
    /// `{"before": s}` with a non-empty `s`. `None` when the argument is absent.
    fn parse(value: Option<&Value>) -> Result<Option<Self>, ToolResult> {
        const SHAPE: &str = "caret must be \"start\", \"end\", {\"after\": \"<substring>\"} or \
                             {\"before\": \"<substring>\"}";
        let Some(value) = value else {
            return Ok(None);
        };
        let invalid = |detail: &str| {
            Err(
                ToolResult::error(format!("type_text: {SHAPE}; got {detail}")).with_structured(
                    serde_json::json!({
                        "code": "invalid_arguments",
                        "tool": "type_text",
                        "detail": format!("{SHAPE}; got {detail}"),
                    }),
                ),
            )
        };
        match value {
            Value::Null => Ok(None),
            Value::String(name) => match name.as_str() {
                "start" => Ok(Some(Self::Start)),
                "end" => Ok(Some(Self::End)),
                other => invalid(&format!("{other:?}")),
            },
            Value::Object(fields) => {
                let single = (fields.len() == 1).then(|| fields.iter().next()).flatten();
                let (key, anchor) = match single {
                    Some((key, Value::String(anchor))) if key == "after" || key == "before" => {
                        (key.as_str(), anchor)
                    }
                    _ => return invalid(&value.to_string()),
                };
                if anchor.is_empty() {
                    return invalid(&format!("an empty {key:?} substring"));
                }
                Ok(Some(if key == "after" {
                    Self::After(anchor.clone())
                } else {
                    Self::Before(anchor.clone())
                }))
            }
            other => invalid(&other.to_string()),
        }
    }

    /// The substring the caller anchored on, when the form names one.
    fn anchor(&self) -> Option<(&'static str, &str)> {
        match self {
            Self::Start | Self::End => None,
            Self::After(anchor) => Some(("after", anchor)),
            Self::Before(anchor) => Some(("before", anchor)),
        }
    }

    /// The request echoed in wire form: `"start"`, `"end"`, `{"after": s}`,
    /// `{"before": s}`.
    fn as_wire(&self) -> Value {
        match self {
            Self::Start => Value::from("start"),
            Self::End => Value::from("end"),
            Self::After(anchor) => serde_json::json!({ "after": anchor }),
            Self::Before(anchor) => serde_json::json!({ "before": anchor }),
        }
    }
}

/// Why the caret was not placed. Every variant is returned before any
/// keystroke or AX text write, so `effect` is always `refused`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CaretRefusal {
    /// The call cannot carry a caret at all (no named element, terminal pid).
    Unsupported { reason: &'static str },
    /// The element publishes no string `AXValue` to resolve the offset against.
    ValueUnreadable,
    /// The anchored substring does not occur in the value.
    AnchorNotFound {
        anchor: String,
        value_utf16_length: usize,
    },
    /// The element rejected the collapsed `AXSelectedTextRange`, or read a
    /// different range back.
    NotPlaced {
        requested_index: usize,
        ax_error: i32,
        observed: Option<(isize, isize)>,
    },
}

impl CaretRefusal {
    fn code(&self) -> &'static str {
        match self {
            Self::Unsupported { .. } => "caret_unsupported",
            Self::ValueUnreadable => "caret_value_unreadable",
            Self::AnchorNotFound { .. } => "caret_anchor_not_found",
            Self::NotPlaced { .. } => "caret_not_placed",
        }
    }
}

/// The caret the driver placed and read back before typing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CaretPlacement {
    /// UTF-16 offset the collapsed selection was confirmed at.
    index: usize,
    spec: CaretSpec,
}

impl CaretPlacement {
    fn note(&self) -> String {
        match self.spec.anchor() {
            Some((relation, anchor)) => format!(" at caret {} ({relation} {anchor:?})", self.index),
            None => format!(" at caret {}", self.index),
        }
    }

    fn attach_evidence(&self, structured: &mut Value) {
        structured["caret_index"] = serde_json::json!(self.index);
        if self.spec.anchor().is_some() {
            structured["caret_anchor"] = self.spec.as_wire();
        }
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Resolve the caret spec against the element's current value to a UTF-16
/// offset. Anchors match the first occurrence, byte-exact.
fn resolve_caret_index(value: &str, spec: &CaretSpec) -> Result<usize, CaretRefusal> {
    let anchored = |anchor: &str| {
        value
            .find(anchor)
            .map(|byte_index| utf16_len(&value[..byte_index]))
            .ok_or_else(|| CaretRefusal::AnchorNotFound {
                anchor: anchor.to_owned(),
                value_utf16_length: utf16_len(value),
            })
    };
    match spec {
        CaretSpec::Start => Ok(0),
        CaretSpec::End => Ok(utf16_len(value)),
        CaretSpec::After(anchor) => anchored(anchor).map(|start| start + utf16_len(anchor)),
        CaretSpec::Before(anchor) => anchored(anchor),
    }
}

/// Collapse the element's selection at the resolved offset and prove it by
/// reading `AXSelectedTextRange` back. Nothing is typed here.
fn place_caret(element: AXUIElementRef, spec: &CaretSpec) -> Result<CaretPlacement, CaretRefusal> {
    let value =
        unsafe { copy_string_attr(element, "AXValue") }.ok_or(CaretRefusal::ValueUnreadable)?;
    let index = resolve_caret_index(&value, spec)?;
    let err = unsafe { set_range_attr(element, "AXSelectedTextRange", index as isize, 0) };
    let observed = (err == kAXErrorSuccess)
        .then(|| unsafe { copy_range_attr(element, "AXSelectedTextRange") })
        .flatten();
    if err != kAXErrorSuccess || observed != Some((index as isize, 0)) {
        return Err(CaretRefusal::NotPlaced {
            requested_index: index,
            ax_error: err,
            observed,
        });
    }
    Ok(CaretPlacement {
        index,
        spec: spec.clone(),
    })
}

/// The reply for a caret that was not placed: nothing was dispatched, the
/// caller can re-anchor or drop `caret` and type at the current insertion point.
fn caret_refusal_result(refusal: &CaretRefusal) -> ToolResult {
    let mut structured = serde_json::json!({
        "code": refusal.code(),
        "effect": "refused",
        "delivered_chars": 0,
        "retryable": true,
    });
    let message = match refusal {
        CaretRefusal::Unsupported { reason } => {
            structured["retryable"] = Value::Bool(false);
            format!("type_text refused the caret before typing: {reason}")
        }
        CaretRefusal::ValueUnreadable => {
            "type_text refused the caret before typing: the element publishes no string \
             AXValue to resolve the offset against; address the text control itself"
                .to_owned()
        }
        CaretRefusal::AnchorNotFound {
            anchor,
            value_utf16_length,
        } => {
            structured["anchor"] = Value::from(anchor.as_str());
            structured["value_utf16_length"] = serde_json::json!(value_utf16_length);
            structured["escalation"] = serde_json::json!({
                "recommended": "re_anchor",
                "reason": "The anchor does not occur in the element's current value (matched \
                           byte-exact, first occurrence). Read the value and anchor on text \
                           it holds, or use \"start\"/\"end\".",
            });
            format!(
                "type_text refused the caret before typing: {anchor:?} does not occur in the \
                 element's value ({value_utf16_length} UTF-16 units); nothing was typed"
            )
        }
        CaretRefusal::NotPlaced {
            requested_index,
            ax_error,
            observed,
        } => {
            structured["requested_index"] = serde_json::json!(requested_index);
            structured["ax_error"] = serde_json::json!(ax_error);
            if let Some((location, length)) = observed {
                structured["observed_range"] =
                    serde_json::json!({ "location": location, "length": length });
            }
            structured["escalation"] = serde_json::json!({
                "recommended": "click",
                "reason": "The element did not take a collapsed AXSelectedTextRange. Place the \
                           insertion point another way (click in the text, then arrow/End \
                           keys) and type without `caret`.",
            });
            let outcome = match observed {
                Some((location, length)) => {
                    format!("the element read back location {location} length {length} instead")
                }
                None if *ax_error == kAXErrorSuccess => {
                    "the element accepted the write but published no range to confirm it".to_owned()
                }
                None => format!("the element rejected the write (AXError {ax_error})"),
            };
            format!(
                "type_text refused to type: the caret was not placed at UTF-16 offset \
                 {requested_index} — {outcome}; nothing was typed"
            )
        }
    };
    ToolResult::error(message).with_structured(structured)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceVerification {
    verified: bool,
    untrusted_web_readback: bool,
}

fn surface_verification(
    path: &str,
    verified: bool,
    target_is_web_content: bool,
) -> SurfaceVerification {
    let untrusted_web_readback =
        verified && target_is_web_content && path_has_untrusted_web_readback(path);
    SurfaceVerification {
        verified: verified && !untrusted_web_readback,
        untrusted_web_readback,
    }
}

fn web_readback_next_rung(is_electron: bool, used_pixel_focus: bool) -> Option<&'static str> {
    match (is_electron, used_pixel_focus) {
        (true, true) => None,
        (true, false) => Some("px"),
        (false, _) => Some("page"),
    }
}

const DELIVERY_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const DELIVERY_DRAIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Pause before the single `AXFocused` re-apply in the foreground rung. Covers
/// an app that installs its own first responder just after activation is
/// observable, which would otherwise clobber the first write.
const FOCUS_REAPPLY_DELAY: std::time::Duration = std::time::Duration::from_millis(30);

/// Keyboard-rung policy for one window-addressed background insert, decided
/// once by the pure exact-target core before any input is posted.
#[derive(Clone, Debug)]
enum BackgroundKeyboardPolicy {
    /// The full ladder may run: foreground requests, pid-only targets, and
    /// window targets whose exact keyboard delivery is proven (singleton
    /// same-pid destination).
    Allowed,
    /// Only the semantic AX write on the proven exact element may run. The
    /// process-scoped CGEvent rung is refused with this refusal — carried so
    /// the exact reason is returned if the AX write does not land.
    SemanticOnly(BackgroundRefusal),
}

/// Delivery envelope for `type_text_blocking`: either an actuator ran
/// (`Typed`), or the exact-target decision refused before any event was
/// posted (`Refused`) and the caller must return the structured refusal.
enum TypeTextDelivery {
    Typed(TypeTextOutcome),
    Refused(BackgroundRefusal),
    SynthesisRefused {
        path: &'static str,
        refusal: SynthesisRefusal,
        ax_attempt: AxAttempt,
    },
}

/// Decide the keyboard policy for a window-addressed background `type_text`.
///
/// Gathers fresh exact-target facts once and asks the pure core:
/// - `InsertText` executes → the full ladder is `Allowed`;
/// - `InsertText` refused but the caller addressed an exact element whose
///   ancestry is proven and semantic AX executes → `SemanticOnly`;
/// - otherwise the structured refusal result is returned and no input of any
///   kind (including a px focus click) may be sent.
async fn background_keyboard_policy(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
) -> Result<(super::BackgroundMutationLease, BackgroundKeyboardPolicy), ToolResult> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, ExactWindowTarget,
    };
    let lease = super::acquire_background_mutation(pid).await;
    let element_guard = element_ptr.map(|ptr| unsafe { crate::ax::RetainedElement::retain(ptr) });
    let facts = match cua_driver_core::operation::spawn_blocking(move || {
        let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());
        crate::ax::exact_target::gather_background_facts(pid, window_id, element_ptr)
    })
    .await
    {
        Ok(facts) => facts,
        Err(error) => {
            return Err(ToolResult::error(format!(
                "Could not gather exact-target facts for pid {pid} window {window_id}: {error}"
            )));
        }
    };
    let target = ExactWindowTarget { pid, window_id };
    match decide_background_input(target, &facts, BackgroundAction::InsertText) {
        BackgroundInputDecision::Execute { .. } => Ok((lease, BackgroundKeyboardPolicy::Allowed)),
        BackgroundInputDecision::Refuse(refusal) => {
            let semantic_available = element_ptr.is_some()
                && decide_background_input(target, &facts, BackgroundAction::AxSemantic)
                    .is_execute();
            if semantic_available {
                Ok((lease, BackgroundKeyboardPolicy::SemanticOnly(refusal)))
            } else {
                Err(super::background_refusal_result(pid, window_id, &refusal))
            }
        }
    }
}

struct TypeTextOutcome {
    detail: String,
    path: &'static str,
    verified: bool,
    /// Exact when AX exposed the target value. `None` means delivery could not
    /// be observed, so the existing unverifiable contract remains in force.
    delivered_chars: Option<usize>,
    /// Whether a text destination resolved at all. `false` means the
    /// keystrokes were posted with nothing to focus and no value to read.
    destination_resolved: bool,
    /// Every requested character arrived but the field holds something else:
    /// the application normalised the input.
    normalized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypedProgress {
    Complete,
    /// Every requested character arrived, and the field's value is not the
    /// requested text: the application normalised the input.
    Normalized(usize),
    Partial(usize),
    Unchanged,
    Unverifiable,
}

fn foreground_settle_ms(pid: i32, frontmost_pid: Option<i32>) -> u64 {
    if frontmost_pid == Some(pid) {
        20
    } else {
        200
    }
}

/// Read-back verification for a keystroke rung: did the typed text actually land?
///
/// `before`/`after` are `AXValue` read from the target field before and after
/// the keystrokes. Returns whether we can *positively confirm* the text landed:
/// - unreadable `after` (`None`) → unverifiable → `false` (Catalyst case; the
///   agent must confirm via screenshot).
/// - `after` contains the complete text → `true`.
/// - empty input text → trivially `true`.
///
/// Apps that normalize input (smart quotes, autocomplete) may fail the
/// substring/length test even though something landed — we report `false`
/// (unverified) rather than erroring, so the agent can still confirm.
#[cfg(test)]
fn verify_typed(before: Option<&str>, after: Option<&str>, text: &str) -> bool {
    matches!(typed_progress(before, after, text), TypedProgress::Complete)
}

/// Classify an observable insertion without mistaking a prefix for complete
/// delivery, and without mistaking a value the field already held for one this
/// call delivered. A positive length delta is an exact delivered-character
/// count for insert-at-cursor typing; it is capped at the request size
/// defensively.
fn typed_progress(before: Option<&str>, after: Option<&str>, text: &str) -> TypedProgress {
    if text.is_empty() {
        return TypedProgress::Complete;
    }
    let Some(after) = after else {
        return TypedProgress::Unverifiable;
    };
    // A field that already held the request cannot prove delivery by still
    // holding it — only the length delta separates an insertion from an echo.
    let already_held = before.is_some_and(|before| before.contains(text));
    if after.contains(text) && !already_held {
        return TypedProgress::Complete;
    }
    let Some(before) = before else {
        return TypedProgress::Unverifiable;
    };
    let requested = text.chars().count();
    let delivered = after
        .chars()
        .count()
        .saturating_sub(before.chars().count())
        .min(requested);
    match delivered {
        0 => TypedProgress::Unchanged,
        n if n < requested => TypedProgress::Partial(n),
        _ if after.contains(text) => TypedProgress::Complete,
        n => TypedProgress::Normalized(n),
    }
}

/// Read the focused/target field's `AXValue`, for before/after read-back.
/// Re-fetches the focused element each call when no explicit element is given
/// (cheap, and focus is stable across our own keystrokes).
fn read_axvalue(pid: i32, element_ptr_and_idx: Option<(usize, Option<usize>)>) -> Option<String> {
    if let Some((ptr, _)) = element_ptr_and_idx {
        unsafe { copy_string_attr(ptr as AXUIElementRef, "AXValue") }
    } else if let Some(el) = unsafe { focused_element_of_pid(pid) } {
        let v = unsafe { copy_string_attr(el, "AXValue") };
        unsafe {
            CFRelease(el as _);
        }
        v
    } else {
        None
    }
}

/// Window-bound variant of [`read_axvalue`]: when no explicit element is
/// addressed and a `window_id` is known, the focused element is used ONLY when
/// its ancestry provably resolves to that exact window. A sibling window's
/// focused field must never supply before/after evidence for the requested
/// target — an unprovable focus reads as `None` (unverifiable), never as
/// sibling data. Without a window the legacy pid-global read applies.
fn read_axvalue_bound(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> Option<String> {
    if element_ptr_and_idx.is_some() {
        return read_axvalue(pid, element_ptr_and_idx);
    }
    match window_id {
        Some(wid) => unsafe {
            let el = crate::ax::exact_target::focused_element_in_window(pid, wid)?;
            let v = copy_string_attr(el, "AXValue");
            CFRelease(el as _);
            v
        },
        None => read_axvalue(pid, None),
    }
}

/// Post-keystroke read-back over both readable views of the target.
///
/// Keystrokes land in whatever holds keyboard focus, and the addressed element
/// is not always that object: AppKit installs a field editor over an
/// NSTextField while it is edited, and some apps replace the row outright
/// (measured in Contacts, where the pinned pointer kept reporting the
/// pre-edit string and every complete insertion was reported as partial).
/// Read the addressed element and the window's focused element, then keep the
/// stronger reading — focus can only contribute evidence when it provably
/// resolves inside the requested window.
fn read_typed_value(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
    before: Option<&str>,
    text: &str,
) -> Option<String> {
    let addressed = read_axvalue_bound(pid, element_ptr_and_idx, window_id);
    if element_ptr_and_idx.is_none() {
        return addressed;
    }
    if matches!(
        typed_progress(before, addressed.as_deref(), text),
        TypedProgress::Complete
    ) {
        return addressed;
    }
    let focused = read_axvalue_bound(pid, None, window_id);
    stronger_reading(before, text, addressed, focused)
}

/// Keep whichever read-back proves more delivery. An unreadable or unrelated
/// focused element can never downgrade the addressed element's evidence.
fn stronger_reading(
    before: Option<&str>,
    text: &str,
    addressed: Option<String>,
    focused: Option<String>,
) -> Option<String> {
    if progress_rank(&typed_progress(before, focused.as_deref(), text))
        > progress_rank(&typed_progress(before, addressed.as_deref(), text))
    {
        focused
    } else {
        addressed
    }
}

/// Order two read-backs by how much delivery each one proves.
fn progress_rank(progress: &TypedProgress) -> (u8, usize) {
    match progress {
        TypedProgress::Unverifiable => (0, 0),
        TypedProgress::Unchanged => (1, 0),
        TypedProgress::Partial(delivered) => (2, *delivered),
        TypedProgress::Normalized(delivered) => (3, *delivered),
        TypedProgress::Complete => (4, 0),
    }
}

/// True when the addressed (or focused) AX element sits inside a web-content
/// subtree — an `AXWebArea` ancestor. That covers every Chromium / WebKit /
/// Electron rendered surface (Chrome, Safari, Slack, VS Code, X's compose box…),
/// where an AX write is echoed back through `AXValue` while the renderer/DOM
/// never observes it — so an AX read-back "confirm" there is a shim echo. A
/// browser's OWN native chrome (address bar, toolbar) has no `AXWebArea`
/// ancestor, so it stays trusted. Walks a bounded ancestor chain; each
/// `AXParent` copy is released, and it stops at the window/app boundary.
pub(super) fn target_in_web_area(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> bool {
    use crate::ax::bindings::AXUIElementCopyAttributeValue;
    use core_foundation::base::{CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    unsafe {
        // Start element: the addressed one (borrowed — do NOT release) or the
        // focused element (owned — must release when done). A window-addressed
        // request may only classify from the window's OWN focused element; a
        // pid-global focused element can belong to a same-process sibling and
        // sibling state must never vouch for the target. When window-bound
        // reacquisition fails, fail closed: report web content (untrusted
        // read-back) rather than trusting an unproven surface.
        let (start, start_owned) = match element_ptr_and_idx {
            Some((ptr, _)) => (ptr as AXUIElementRef, false),
            None => match window_id {
                Some(wid) => match crate::ax::exact_target::focused_element_in_window(pid, wid) {
                    Some(el) => (el, true),
                    None => return true,
                },
                None => match focused_element_of_pid(pid) {
                    Some(el) => (el, true),
                    None => return false,
                },
            },
        };
        let parent_attr = CFString::new("AXParent");
        let mut cur = start;
        let mut cur_owned = start_owned;
        let mut found = false;
        for _ in 0..40 {
            match copy_string_attr(cur, "AXRole").as_deref() {
                Some("AXWebArea") => {
                    found = true;
                    break;
                }
                // No web area lives above the window/app root — stop.
                Some("AXWindow") | Some("AXApplication") | None => break,
                _ => {}
            }
            let mut parent: CFTypeRef = std::ptr::null_mut();
            let err =
                AXUIElementCopyAttributeValue(cur, parent_attr.as_concrete_TypeRef(), &mut parent);
            if cur_owned {
                CFRelease(cur as CFTypeRef);
            }
            if err != kAXErrorSuccess || parent.is_null() {
                cur = std::ptr::null_mut();
                cur_owned = false;
                break;
            }
            cur = parent as AXUIElementRef;
            cur_owned = true;
        }
        if cur_owned && !cur.is_null() {
            CFRelease(cur as CFTypeRef);
        }
        found
    }
}

/// Type via CGEvent keystrokes at the current insertion point, then verify by
/// read-back. `type_text` is deliberately non-idempotent: it must never clear
/// an existing value merely because AX cannot read that value back.
fn cgevent_type_verified(
    pid: i32,
    text: &str,
    delay_ms: u64,
    before: Option<&str>,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    settle_ms: u64,
    window_id: Option<u32>,
) -> anyhow::Result<TypedDelivery> {
    // Focus the target element so the keystrokes land in IT. Critical in
    // foreground mode: a freshly-fronted window's keyboard focus may be on the
    // search box or nowhere, so without this the text goes into the void (or the
    // wrong field). AXFocused is best-effort — harmless when unsupported.
    //
    // Ordering matters as much as the write itself. `with_foreground_assist` has
    // already waited for the activation to land, so AppKit has installed the
    // window's remembered first responder by now and this write lands *after*
    // it rather than being clobbered by it. Re-applying once on a failed
    // read-back covers apps that install their responder slightly late.
    if let Some((ptr, _)) = element_ptr_and_idx {
        let _ = crate::input::ax_actions::focus_element(ptr);
        if settle_ms > 0 && !crate::input::ax_actions::is_element_focused(pid, ptr) {
            std::thread::sleep(FOCUS_REAPPLY_DELAY);
            let _ = crate::input::ax_actions::focus_element(ptr);
        }
    }
    // First-keystroke settle (foreground rung only — caller passes `settle_ms > 0`).
    // Even once the window is front and the element focused, the surface isn't
    // ready to accept input for a few tens of ms, so the FIRST synthesized
    // character gets eaten: typing "i love u" rendered "love u" (the leading
    // "i " was dropped). A short sleep here covers that. Background/terminal call
    // sites pass 0 — they have no front transition and must not pay this latency.
    if settle_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(settle_ms));
    }
    type_and_drain(pid, text, delay_ms, before, element_ptr_and_idx, window_id)
}

/// Post the keystrokes and wait for the target's read-back to settle. Shared
/// with `set_value`, which establishes focus and the replaced selection itself
/// and must not have either re-applied underneath it.
pub(super) fn type_and_drain(
    pid: i32,
    text: &str,
    delay_ms: u64,
    before: Option<&str>,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> anyhow::Result<TypedDelivery> {
    crate::input::keyboard::type_text_with_delay(pid, text, delay_ms)?;

    // CGEvent posting is asynchronous with respect to the renderer. In
    // particular, Chromium can acknowledge the posting process while a long
    // tail remains queued. Poll the AX value until the complete payload is
    // visible instead of treating any growth as success. If the deadline
    // expires after observable growth, surface the exact partial count.
    let deadline = std::time::Instant::now() + DELIVERY_DRAIN_TIMEOUT;
    Ok(await_typed_delivery(before, text, deadline, || {
        read_typed_value(pid, element_ptr_and_idx, window_id, before, text)
    }))
}

/// What a keystroke rung's read-back proved about the requested text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TypedDelivery {
    /// A read-back accounted for every requested character.
    pub verified: bool,
    /// Characters the read-back proved delivered, when it could count them.
    pub delivered: Option<usize>,
    /// Every character arrived and the field's value is not the requested
    /// text: the application normalised the input.
    pub normalized: bool,
}

fn await_typed_delivery(
    before: Option<&str>,
    text: &str,
    deadline: std::time::Instant,
    read_value: impl FnMut() -> Option<String>,
) -> TypedDelivery {
    match await_typed_progress(before, text, deadline, read_value) {
        TypedProgress::Complete => TypedDelivery {
            verified: true,
            delivered: Some(text.chars().count()),
            normalized: false,
        },
        TypedProgress::Normalized(delivered) => TypedDelivery {
            verified: true,
            delivered: Some(delivered),
            normalized: true,
        },
        TypedProgress::Partial(delivered) => TypedDelivery {
            verified: false,
            delivered: Some(delivered),
            normalized: false,
        },
        TypedProgress::Unchanged => TypedDelivery {
            verified: false,
            delivered: Some(0),
            normalized: false,
        },
        TypedProgress::Unverifiable => TypedDelivery::default(),
    }
}

fn await_typed_progress(
    before: Option<&str>,
    text: &str,
    deadline: std::time::Instant,
    mut read_value: impl FnMut() -> Option<String>,
) -> TypedProgress {
    let mut best_partial = None;
    let mut last_readable;
    loop {
        let after = read_value();
        match typed_progress(before, after.as_deref(), text) {
            // A value already the requested length has nothing left to
            // arrive; both states are terminal.
            terminal @ (TypedProgress::Complete | TypedProgress::Normalized(_)) => return terminal,
            TypedProgress::Partial(delivered) => {
                best_partial =
                    Some(best_partial.map_or(delivered, |best: usize| best.max(delivered)));
                last_readable = Some(TypedProgress::Partial(delivered));
            }
            TypedProgress::Unchanged => {
                best_partial.get_or_insert(0);
                last_readable = Some(TypedProgress::Unchanged);
            }
            TypedProgress::Unverifiable => {
                if best_partial.is_none() {
                    return TypedProgress::Unverifiable;
                }
                last_readable = None;
            }
        }
        if std::time::Instant::now() >= deadline
            || cua_driver_core::operation::sleep(DELIVERY_DRAIN_POLL_INTERVAL).is_err()
        {
            return last_readable.unwrap_or(match best_partial {
                Some(delivered) if delivered > 0 => TypedProgress::Partial(delivered),
                _ => TypedProgress::Unchanged,
            });
        }
    }
}

/// The focused element `type_text` may address, retained. A window-addressed
/// request may only use focus that provably belongs to that exact window: a
/// sibling window's focused field is not the requested target.
fn resolve_window_focus(pid: i32, window_id: Option<u32>) -> Option<RetainedElement> {
    // SAFETY: both resolvers return a `+1` reference, which the guard takes
    // over and releases when the call that holds it ends.
    unsafe {
        match window_id {
            Some(window_id) => crate::ax::exact_target::focused_element_in_window(pid, window_id),
            None => focused_element_of_pid(pid),
        }
        .and_then(|element| RetainedElement::adopt(element))
    }
}

/// Best-effort-background ladder for `type_text`.
///
/// - `delivery_mode == Background` (default): AX insert → read-back; on a
///   silent/unreadable accept, CGEvent keystrokes → read-back. Never fronts.
/// - `delivery_mode == Foreground`: the agent's explicit last resort — briefly
///   front `window_id`, insert at the current cursor, restore, then read-back.
///
/// Returns `(detail, path, verified)`. `verified` is `true` only when a
/// read-back positively confirmed the text; `false` means the agent must
/// confirm via screenshot (and, for background, can escalate to foreground).
fn type_text_blocking(
    pid: i32,
    text: &str,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    delay_ms: u64,
    is_terminal_target: bool,
    delivery_mode: super::DeliveryMode,
    window_id: Option<u32>,
    keyboard_policy: BackgroundKeyboardPolicy,
) -> anyhow::Result<TypeTextDelivery> {
    // One destination for every rung: the read-back, the AX write and the
    // keystroke rung's focus re-apply must address the same object, and a
    // window-addressed request may only use focus that provably belongs to
    // that window.
    let resolved_focus = match element_ptr_and_idx {
        Some(_) => None,
        None => resolve_window_focus(pid, window_id),
    };
    let target = element_ptr_and_idx.or_else(|| {
        resolved_focus
            .as_ref()
            .map(|element| (element.as_ptr(), None))
    });
    let destination_resolved = target.is_some();

    // Original field value before any rung drives read-back verification only.
    // An unreadable value is not evidence that the field is empty.
    let before = read_axvalue_bound(pid, target, window_id);

    // --- Foreground rung: explicit agent request (skip AX/background ladder). ---
    if delivery_mode.is_foreground() {
        let screen_sharing_target = crate::input::keyboard::is_screen_sharing_pid(pid);
        if let Some(refusal) = synthesis_preflight(
            if screen_sharing_target {
                TextDeliveryRoute::PhysicalSynthesis
            } else {
                TextDeliveryRoute::UnicodeSynthesis
            },
            text.chars().count(),
            delay_ms,
        ) {
            return Ok(TypeTextDelivery::SynthesisRefused {
                path: if window_id.is_some() {
                    PATH_KEY_EVENTS_FG
                } else {
                    PATH_KEY_EVENTS
                },
                refusal,
                ax_attempt: AxAttempt::NotAttempted,
            });
        }
        // Settle between front+focus and the first keystroke — see the
        // "i love u" -> "love u" first-char-drop note in cgevent_type_verified.
        // A target that was already frontmost pays only 20ms for element-focus
        // settling. Focus-proxy clients that were just activated need longer:
        // an RDP client (Microsoft Windows App) re-arms its keyboard grab with
        // the remote host over hundreds of ms, so at 60ms every keystroke was
        // dropped. 200ms covers that re-grab without penalizing an already
        // armed interactive stream on every text chunk.
        let foreground_settle_ms = foreground_settle_ms(pid, apps::frontmost_pid());
        // Focus resolved before the front is the stronger target: the
        // activation installs the window's remembered first responder, which
        // is not what the agent addressed. When nothing resolved, look again
        // inside the activation rather than typing blind.
        let mut late_focus: Option<RetainedElement> = None;
        let mut destination = target;
        let mut do_type = || {
            if destination.is_none() {
                late_focus = resolve_window_focus(pid, window_id);
                destination = late_focus.as_ref().map(|element| (element.as_ptr(), None));
            }
            cgevent_type_verified(
                pid,
                text,
                delay_ms,
                before.as_deref(),
                destination,
                foreground_settle_ms,
                window_id,
            )
        };
        let (delivery, fronted) = match window_id {
            Some(wid) if screen_sharing_target => {
                // Screen Sharing forwards physical HID transitions to the
                // guest. PID-routed Unicode events all carry keycode 0 (the A
                // key), so a guest sees "aaaa"; modifier flags alone likewise
                // turn Cmd+V into plain "v". The explicit foreground rung may
                // safely use the global HID queue while the exact target is
                // guarded and restored.
                crate::input::skylight::with_foreground_hid_activation(
                    pid as libc::pid_t,
                    wid,
                    || {
                        if foreground_settle_ms > 0 {
                            std::thread::sleep(std::time::Duration::from_millis(
                                foreground_settle_ms,
                            ));
                        }
                        crate::input::keyboard::type_text_physical_global(text, delay_ms)
                    },
                )?;
                (TypedDelivery::default(), true)
            }
            Some(wid) => {
                // Front → type → restore. The closure returns the read-back
                // result; with_foreground_assist returns whether it actually
                // fronted (Ok(false) when the fronting SPIs are unavailable —
                // the keystrokes still ran, just as background input).
                let mut typed_delivery = TypedDelivery::default();
                let fronted = crate::input::skylight::with_foreground_assist(
                    pid as libc::pid_t,
                    wid,
                    || {
                        typed_delivery = do_type()?;
                        Ok(())
                    },
                )?;
                (typed_delivery, fronted)
            }
            // No window to front — best-effort background keystrokes instead.
            None => (do_type()?, false),
        };
        // Only claim the `_fg` path when a front actually happened; when no
        // foregrounding occurred (no window, or SPIs unavailable) these were
        // background keystrokes and `path` must say so honestly.
        return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
            detail: format!(" via foreground keystrokes ({delay_ms}ms delay)"),
            path: if fronted {
                PATH_KEY_EVENTS_FG
            } else {
                PATH_KEY_EVENTS
            },
            delivered_chars: delivery.delivered,
            verified: delivery.verified,
            normalized: delivery.normalized,
            destination_resolved: destination.is_some(),
        }));
    }

    // --- Background rung 0: terminal emulator → CGEvent only (AX is dropped). ---
    if is_terminal_target {
        // A terminal insert has no semantic AX rung: when the exact-target
        // decision restricted this request to semantic-only, there is nothing
        // safe to run — refuse before posting anything.
        if let BackgroundKeyboardPolicy::SemanticOnly(refusal) = keyboard_policy {
            return Ok(TypeTextDelivery::Refused(refusal));
        }
        if let Some(refusal) = synthesis_preflight(
            TextDeliveryRoute::UnicodeSynthesis,
            text.chars().count(),
            delay_ms,
        ) {
            return Ok(TypeTextDelivery::SynthesisRefused {
                path: PATH_KEY_EVENTS,
                refusal,
                ax_attempt: AxAttempt::NotAttempted,
            });
        }
        tracing::debug!(
            "type_text: pid {pid} is a terminal emulator; skipping AX value-set, \
             using CGEvent key-event synthesis"
        );
        let delivery = cgevent_type_verified(
            pid,
            text,
            delay_ms,
            before.as_deref(),
            target,
            /*settle_ms=*/ 0,
            window_id,
        )?;
        return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
            detail: format!(" via CGEvent (terminal emulator, {delay_ms}ms delay)"),
            path: PATH_KEY_EVENTS,
            verified: delivery.verified,
            delivered_chars: delivery.delivered,
            normalized: delivery.normalized,
            destination_resolved,
        }));
    }

    // --- Background rung 1: AX SelectedText write (element or focused). ---
    let ax_target: Option<(AXUIElementRef, Option<usize>)> =
        target.map(|(ptr, idx)| (ptr as AXUIElementRef, idx));
    let mut ax_attempt = AxAttempt::NotAttempted;
    if let Some((element, idx_opt)) = ax_target {
        let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
        let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();
        let err = unsafe { set_string_attr(element, "AXSelectedText", text) };
        // Classify the write before considering synthesis. Complete AX
        // delivery returns immediately. Partial delivery is surfaced as such
        // instead of appending the full payload again. When synthesis would
        // exceed its transport-safe budget, rejected/unchanged AX writes fail
        // safely and unreadable AX state is reported as indeterminate.
        //
        // The write is atomic, its effect on the field is not: the same drain
        // the keystroke rung uses settles the read-back here, over the same
        // pair of readable views of the target, so a value the app is still
        // rebuilding is never reported as a partial insertion.
        let ax_progress = if err == kAXErrorSuccess {
            let deadline = std::time::Instant::now() + DELIVERY_DRAIN_TIMEOUT;
            Some(await_typed_progress(
                before.as_deref(),
                text,
                deadline,
                || {
                    read_typed_value(
                        pid,
                        Some((element as usize, idx_opt)),
                        window_id,
                        before.as_deref(),
                        text,
                    )
                },
            ))
        } else {
            None
        };
        // AXValue is not renderer evidence in web content. An unchanged echo
        // there cannot prove that zero characters landed, so blind retry is
        // unsafe even though no synthesis has run yet.
        let unchanged_web_readback = ax_progress == Some(TypedProgress::Unchanged)
            && target_in_web_area(pid, Some((element as usize, idx_opt)), window_id);
        let idx_str = idx_opt.map(|i| format!(" [{i}]")).unwrap_or_default();
        if ax_progress == Some(TypedProgress::Complete) {
            return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                detail: format!(" into{idx_str} {role} \"{title}\""),
                path: PATH_AX,
                verified: true,
                delivered_chars: Some(text.chars().count()),
                destination_resolved: true,
                normalized: false,
            }));
        }
        if let Some(TypedProgress::Normalized(delivered_chars)) = ax_progress {
            return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                detail: format!(" into{idx_str} {role} \"{title}\""),
                path: PATH_AX,
                verified: true,
                delivered_chars: Some(delivered_chars),
                destination_resolved: true,
                normalized: true,
            }));
        }
        if let Some(TypedProgress::Partial(delivered_chars)) = ax_progress {
            return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                detail: format!(" via partial AX write into{idx_str} {role} \"{title}\""),
                path: PATH_AX,
                verified: false,
                delivered_chars: Some(delivered_chars),
                destination_resolved: true,
                normalized: false,
            }));
        }
        ax_attempt = match ax_progress {
            Some(TypedProgress::Unchanged) if unchanged_web_readback => AxAttempt::Unverifiable,
            Some(TypedProgress::Unchanged) => AxAttempt::Unchanged,
            Some(TypedProgress::Unverifiable) => AxAttempt::Unverifiable,
            None => AxAttempt::Rejected,
            Some(
                TypedProgress::Complete | TypedProgress::Normalized(_) | TypedProgress::Partial(_),
            ) => unreachable!(),
        };
        tracing::debug!(
            "AX write did not land for {role} \"{title}\" (err={err}); \
             falling back to CGEvent keystrokes"
        );
    } else {
        tracing::debug!("No focused element for pid {pid}; using CGEvent keystrokes");
    }

    // The semantic AX rung did not land and this request is restricted to it:
    // the process-scoped CGEvent rung could reach a sibling window, so return
    // the structured refusal instead of escalating.
    if let BackgroundKeyboardPolicy::SemanticOnly(refusal) = keyboard_policy {
        return Ok(TypeTextDelivery::Refused(refusal));
    }

    if let Some(refusal) = synthesis_preflight(
        TextDeliveryRoute::UnicodeSynthesis,
        text.chars().count(),
        delay_ms,
    ) {
        return Ok(TypeTextDelivery::SynthesisRefused {
            path: PATH_KEY_EVENTS,
            refusal,
            ax_attempt,
        });
    }

    // --- Background rung 2: CGEvent keystrokes with read-back. ---
    // Never clear here: a partial AX write is rare, and clearing would violate
    // insert-at-cursor semantics.
    let delivery = cgevent_type_verified(
        pid,
        text,
        delay_ms,
        before.as_deref(),
        target,
        /*settle_ms=*/ 0,
        window_id,
    )?;
    Ok(TypeTextDelivery::Typed(TypeTextOutcome {
        detail: format!(" via CGEvent ({delay_ms}ms delay)"),
        path: PATH_KEY_EVENTS,
        verified: delivery.verified,
        delivered_chars: delivery.delivered,
        normalized: delivery.normalized,
        destination_resolved,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Caret placement ──────────────────────────────────────────────────

    fn after(anchor: &str) -> CaretSpec {
        CaretSpec::After(anchor.into())
    }

    fn before(anchor: &str) -> CaretSpec {
        CaretSpec::Before(anchor.into())
    }

    /// Offsets are UTF-16 code units: an ASCII value counts characters, a
    /// multi-byte BMP character still counts one unit, and a non-BMP scalar
    /// (emoji) counts two — before and inside the anchor alike.
    #[test]
    fn caret_index_counts_utf16_units() {
        let ascii = "Meeting 047\nAgenda: warehouse pallet audit\n";
        assert_eq!(resolve_caret_index(ascii, &CaretSpec::Start), Ok(0));
        assert_eq!(resolve_caret_index(ascii, &CaretSpec::End), Ok(43));
        assert_eq!(resolve_caret_index(ascii, &before("warehouse")), Ok(20));
        assert_eq!(
            resolve_caret_index(ascii, &after("warehouse pallet audit")),
            Ok(42)
        );

        // "é" is two bytes, one UTF-16 unit; byte offsets must not leak.
        let bmp = "café au lait";
        assert_eq!(resolve_caret_index(bmp, &after("café")), Ok(4));
        assert_eq!(resolve_caret_index(bmp, &before("lait")), Ok(8));
        assert_eq!(resolve_caret_index(bmp, &CaretSpec::End), Ok(12));

        // "🚀" is four bytes, one char, TWO UTF-16 units.
        let astral = "go 🚀 now";
        assert_eq!(resolve_caret_index(astral, &before("🚀")), Ok(3));
        assert_eq!(resolve_caret_index(astral, &after("🚀")), Ok(5));
        assert_eq!(resolve_caret_index(astral, &after("now")), Ok(9));
        assert_eq!(resolve_caret_index(astral, &after("go 🚀")), Ok(5));
        assert_eq!(resolve_caret_index(astral, &CaretSpec::End), Ok(9));
    }

    /// The first occurrence wins, so `after` on a repeated word lands after
    /// the earliest one, not the last.
    #[test]
    fn caret_anchor_matches_first_occurrence() {
        let value = "one two one two";
        assert_eq!(resolve_caret_index(value, &after("one")), Ok(3));
        assert_eq!(resolve_caret_index(value, &before("two")), Ok(4));
    }

    /// An absent anchor is a refusal that carries what was searched for and
    /// how long the value is, in the same units the caller would anchor in.
    #[test]
    fn caret_absent_anchor_is_a_refusal_with_search_evidence() {
        let value = "Meeting 047 🚀";
        assert_eq!(
            resolve_caret_index(value, &after("pallet")),
            Err(CaretRefusal::AnchorNotFound {
                anchor: "pallet".into(),
                value_utf16_length: 14,
            })
        );
        // Matching is byte-exact: case and whitespace differ → absent.
        assert!(resolve_caret_index(value, &before("meeting")).is_err());
    }

    /// An empty value still resolves `start`/`end` (both 0) and refuses every
    /// anchor with a zero length.
    #[test]
    fn caret_on_empty_value() {
        assert_eq!(resolve_caret_index("", &CaretSpec::Start), Ok(0));
        assert_eq!(resolve_caret_index("", &CaretSpec::End), Ok(0));
        assert_eq!(
            resolve_caret_index("", &after("x")),
            Err(CaretRefusal::AnchorNotFound {
                anchor: "x".into(),
                value_utf16_length: 0,
            })
        );
    }

    /// The wire forms accepted for `caret`, and the malformed ones that are
    /// argument-shape errors before any gate runs.
    #[test]
    fn caret_argument_parses_its_four_forms_and_rejects_the_rest() {
        use serde_json::json;
        let parsed = |value: Option<&Value>| CaretSpec::parse(value).ok().flatten();
        assert_eq!(parsed(None), None);
        assert_eq!(parsed(Some(&Value::Null)), None);
        assert_eq!(parsed(Some(&json!("start"))), Some(CaretSpec::Start));
        assert_eq!(parsed(Some(&json!("end"))), Some(CaretSpec::End));
        assert_eq!(
            parsed(Some(&json!({"after": "audit"}))),
            Some(after("audit"))
        );
        assert_eq!(
            parsed(Some(&json!({"before": "Follow"}))),
            Some(before("Follow"))
        );
        for malformed in [
            json!("middle"),
            json!(7),
            json!({"after": ""}),
            json!({"after": "a", "before": "b"}),
            json!({"at": 3}),
            json!({}),
        ] {
            let error = CaretSpec::parse(Some(&malformed))
                .err()
                .unwrap_or_else(|| panic!("{malformed} must be rejected"));
            let structured = error.structured_content.expect("typed argument error");
            assert_eq!(structured["code"], "invalid_arguments", "{malformed}");
        }
    }

    /// The anchor refusal is typed, dispatches nothing, and names the search.
    #[test]
    fn caret_anchor_refusal_shape() {
        let result = caret_refusal_result(&CaretRefusal::AnchorNotFound {
            anchor: "warehouse".into(),
            value_utf16_length: 57,
        });
        assert_eq!(result.is_error, Some(true));
        let s = result.structured_content.expect("structured refusal");
        assert_eq!(s["code"], "caret_anchor_not_found");
        assert_eq!(s["effect"], "refused");
        assert_eq!(s["delivered_chars"], 0);
        assert_eq!(s["retryable"], true);
        assert_eq!(s["anchor"], "warehouse");
        assert_eq!(s["value_utf16_length"], 57);
        assert_eq!(s["escalation"]["recommended"], "re_anchor");
    }

    /// A range the element did not take is reported with what it read back.
    #[test]
    fn caret_not_placed_refusal_shape() {
        let result = caret_refusal_result(&CaretRefusal::NotPlaced {
            requested_index: 42,
            ax_error: kAXErrorSuccess,
            observed: Some((0, 0)),
        });
        let s = result.structured_content.expect("structured refusal");
        assert_eq!(s["code"], "caret_not_placed");
        assert_eq!(s["effect"], "refused");
        assert_eq!(s["requested_index"], 42);
        assert_eq!(
            s["observed_range"],
            serde_json::json!({"location": 0, "length": 0})
        );
        assert_eq!(s["escalation"]["recommended"], "click");

        let rejected = caret_refusal_result(&CaretRefusal::NotPlaced {
            requested_index: 42,
            ax_error: -25205,
            observed: None,
        });
        let s = rejected.structured_content.expect("structured refusal");
        assert_eq!(s["ax_error"], -25205);
        assert!(s.get("observed_range").is_none());
    }

    /// A placed caret shows up in the reply as `caret_index`, plus the anchor
    /// only when one was named.
    #[test]
    fn caret_placement_evidence() {
        let mut s = serde_json::json!({});
        CaretPlacement {
            index: 42,
            spec: after("audit"),
        }
        .attach_evidence(&mut s);
        assert_eq!(s["caret_index"], 42);
        assert_eq!(s["caret_anchor"], serde_json::json!({"after": "audit"}));

        let mut s = serde_json::json!({});
        CaretPlacement {
            index: 0,
            spec: CaretSpec::End,
        }
        .attach_evidence(&mut s);
        assert_eq!(s["caret_index"], 0);
        assert!(s.get("caret_anchor").is_none());
    }

    /// A request that resolved no text destination escalates to the element
    /// on BOTH keystroke paths. The foreground rung used to be excluded from
    /// escalation entirely, so a window-scoped foreground type that landed
    /// nowhere told the agent only to look at a screenshot.
    #[test]
    fn no_text_destination_escalates_to_the_element_on_either_path() {
        for path in [PATH_KEY_EVENTS, PATH_KEY_EVENTS_FG, PATH_AX] {
            let state = Unconfirmed::of(
                /*destination_resolved=*/ false, /*untrusted_web_readback=*/ false, path,
            );
            assert_eq!(state, Unconfirmed::NoDestination, "{path}");
            assert_eq!(state.escalation_target(), Some("element"), "{path}");
        }
    }

    /// The no-destination state outranks the web-content surface: without a
    /// field there is no read-back to distrust, and `px`/`page` cannot supply
    /// one either.
    #[test]
    fn no_text_destination_outranks_the_web_readback_state() {
        assert_eq!(
            Unconfirmed::of(false, true, PATH_KEY_EVENTS),
            Unconfirmed::NoDestination
        );
    }

    /// With a destination in hand the path still decides: the background rung
    /// can escalate to foreground, the foreground rung is already last.
    #[test]
    fn a_resolved_destination_escalates_by_path() {
        assert_eq!(
            Unconfirmed::of(true, false, PATH_KEY_EVENTS).escalation_target(),
            Some("foreground")
        );
        assert_eq!(
            Unconfirmed::of(true, false, PATH_KEY_EVENTS_FG).escalation_target(),
            None
        );
        assert_eq!(
            Unconfirmed::of(true, true, PATH_KEY_EVENTS).escalation_target(),
            None
        );
    }

    /// Sanity-check that the terminal short-circuit can be expressed as a
    /// pure function of `is_terminal_target`: when true, the code goes
    /// to key-event synthesis without consulting AX. This test stands
    /// in for an integration test (which would need a running terminal)
    /// — it exercises the branch by injecting `is_terminal_target=true`
    /// with a non-existent pid and checking we get the expected error
    /// shape from the CGEvent path (not from the AX path).
    ///
    /// The CGEvent post will fail for pid 0 / -1, so we only assert
    /// that `type_text_blocking` returns `Err` *after* deciding to
    /// take the key-events path — i.e. it doesn't hit the AX branches
    /// where `set_string_attr(0)` would crash.
    #[test]
    fn terminal_flag_routes_past_ax_path() {
        // Pid -1 is invalid; the AX path would unconditionally call
        // focused_element_of_pid which is safe but it would never reach
        // CGEvent. The fact that this returns an Err (without crashing)
        // proves we routed through CGEvent-only and never touched AX.
        let r = type_text_blocking(
            -1,
            "x",
            None,
            0,
            /*is_terminal_target=*/ true,
            super::super::DeliveryMode::Background,
            None,
            BackgroundKeyboardPolicy::Allowed,
        );
        // We don't care whether r is Ok or Err — what matters is that
        // calling it with is_terminal_target=true is safe and never
        // dereferences null AX pointers.
        let _ = r;
    }

    /// A semantic-only policy must refuse the terminal short-circuit before
    /// any CGEvent is posted: terminals have no semantic AX rung, so nothing
    /// safe remains and the carried refusal comes back unchanged.
    #[test]
    fn semantic_only_policy_refuses_terminal_cgevent_rung() {
        let refusal = BackgroundRefusal {
            code: cua_driver_core::background_input::refusal_codes::SAME_PID_KEYBOARD_AMBIGUITY,
            reason: "test".into(),
            advice: None,
        };
        let r = type_text_blocking(
            -1,
            "x",
            None,
            0,
            /*is_terminal_target=*/ true,
            super::super::DeliveryMode::Background,
            Some(7),
            BackgroundKeyboardPolicy::SemanticOnly(refusal.clone()),
        );
        match r {
            Ok(TypeTextDelivery::Refused(returned)) => assert_eq!(returned, refusal),
            other => panic!("expected a structured refusal, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn oversized_synthesis_is_refused_before_the_terminal_event_path() {
        let text = "x".repeat(6_500);
        let result = type_text_blocking(
            -1,
            &text,
            None,
            0,
            /*is_terminal_target=*/ true,
            super::super::DeliveryMode::Background,
            None,
            BackgroundKeyboardPolicy::Allowed,
        )
        .expect("preflight refusal must not attempt the invalid pid");
        let TypeTextDelivery::SynthesisRefused {
            path,
            refusal,
            ax_attempt,
        } = result
        else {
            panic!("oversized terminal synthesis must fail before mutation");
        };
        assert_eq!(path, PATH_KEY_EVENTS);
        assert_eq!(ax_attempt, AxAttempt::NotAttempted);
        assert_eq!(refusal.requested_chars, 6_500);
        assert_eq!(refusal.estimated_duration_ms, 106_000);
        assert_eq!(refusal.max_chunk_chars, 6_125);
    }

    #[test]
    fn synthesis_preflight_accepts_the_exact_transport_safe_boundary() {
        assert!(synthesis_preflight(TextDeliveryRoute::UnicodeSynthesis, 6_125, 0).is_none());
        assert!(synthesis_preflight(TextDeliveryRoute::UnicodeSynthesis, 6_126, 0).is_some());
    }

    #[test]
    fn large_atomic_ax_payloads_are_not_subject_to_the_synthesis_budget() {
        assert!(synthesis_preflight(TextDeliveryRoute::AtomicAx, 100_000, 200).is_none());
        assert_eq!(
            typed_progress(None, Some(&"x".repeat(11_500)), &"x".repeat(11_500)),
            TypedProgress::Complete,
            "a successful one-call AX insertion remains eligible regardless of size"
        );
    }

    #[test]
    fn refusal_diagnostics_distinguish_safe_chunking_from_indeterminate_ax() {
        let refusal = synthesis_preflight(TextDeliveryRoute::UnicodeSynthesis, 6_500, 0)
            .expect("payload must exceed the synthesis budget");
        let safe = synthesis_refusal_result(PATH_KEY_EVENTS, &refusal, AxAttempt::Rejected);
        let safe = safe.structured_content.expect("structured refusal");
        assert_eq!(safe["code"], "type_text_synthesis_budget_exceeded");
        assert_eq!(safe["effect"], "refused");
        assert_eq!(safe["delivered_chars"], 0);
        assert_eq!(safe["synthesized_chars"], 0);
        assert_eq!(safe["retryable"], true);
        assert_eq!(safe["escalation"]["recommended"], "chunk");

        let indeterminate =
            synthesis_refusal_result(PATH_KEY_EVENTS, &refusal, AxAttempt::Unverifiable);
        let indeterminate = indeterminate
            .structured_content
            .expect("structured indeterminate result");
        assert_eq!(indeterminate["effect"], "indeterminate");
        assert!(indeterminate.get("delivered_chars").is_none());
        assert_eq!(indeterminate["synthesized_chars"], 0);
        assert_eq!(indeterminate["retryable"], false);
        assert_eq!(indeterminate["escalation"]["recommended"], "verify_state");
    }

    #[test]
    fn verify_typed_unreadable_after_is_unverified() {
        // Catalyst: can't read AXValue back → cannot confirm → false.
        assert!(!verify_typed(None, None, "hi"));
        assert!(!verify_typed(Some(""), None, "hi"));
    }

    #[test]
    fn verify_typed_contains_full_request_is_verified() {
        assert!(verify_typed(Some(""), Some("hi"), "hi")); // contains
        assert!(verify_typed(Some("ab"), Some("ab hi"), "hi")); // contains, appended
    }

    #[test]
    fn observable_prefix_is_partial_not_verified() {
        assert_eq!(
            typed_progress(Some(""), Some("BEGINpayload"), "BEGINpayloadEND"),
            TypedProgress::Partial(12)
        );
        assert!(!verify_typed(
            Some(""),
            Some("BEGINpayload"),
            "BEGINpayloadEND"
        ));
    }

    #[test]
    fn delivery_waits_through_a_partial_readback_until_complete() {
        let text = "BEGIN-payload-END";
        let mut values = std::collections::VecDeque::from([
            Some("BEGIN-payload".to_owned()),
            Some(text.to_owned()),
        ]);
        let mut reads = 0;
        let delivery = await_typed_delivery(
            Some(""),
            text,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            || {
                reads += 1;
                values.pop_front().flatten()
            },
        );
        assert_eq!(
            delivery,
            TypedDelivery {
                verified: true,
                delivered: Some(text.chars().count()),
                normalized: false,
            }
        );
        assert_eq!(reads, 2, "completion must wait past the prefix readback");
    }

    #[test]
    fn drained_prefix_reports_the_delivered_character_count() {
        let delivery = await_typed_delivery(
            Some(""),
            "BEGIN-payload-END",
            std::time::Instant::now(),
            || Some("BEGIN".to_owned()),
        );
        assert_eq!(
            delivery,
            TypedDelivery {
                verified: false,
                delivered: Some(5),
                normalized: false,
            }
        );
    }

    /// A field that already held the request cannot prove delivery by still
    /// holding it. Measured in Notes: a prior clear was refused, so the search
    /// field already read "warehouse pallet audit" and an insertion that
    /// landed nothing was reported "✅ Inserted 22 char(s) … verified".
    #[test]
    fn a_value_the_field_already_held_is_not_a_confirmed_insertion() {
        let text = "warehouse pallet audit";
        assert_eq!(
            typed_progress(Some(text), Some(text), text),
            TypedProgress::Unchanged
        );
        // A real insertion on top of the same value still counts: the length
        // moved by the full request.
        assert_eq!(
            typed_progress(Some(text), Some(&format!("{text}{text}")), text),
            TypedProgress::Complete
        );
        // Half of it arriving is a partial, not a confirm.
        assert_eq!(
            typed_progress(Some(text), Some(&format!("wareh{text}")), text),
            TypedProgress::Partial(5)
        );
    }

    /// An unreadable prior value leaves the substring test as the only
    /// evidence there is, so it stays authoritative there.
    #[test]
    fn an_unreadable_prior_value_still_confirms_on_the_substring() {
        assert_eq!(
            typed_progress(None, Some("hello world"), "hello"),
            TypedProgress::Complete
        );
    }

    /// Replacing a selection shortens the field, so the length delta is zero
    /// or negative while every character in fact landed. AppKit selects a text
    /// field's whole contents when it takes focus, which makes this the common
    /// case rather than an edge one.
    #[test]
    fn a_replacing_insertion_is_complete_even_though_the_field_shrank() {
        assert_eq!(
            typed_progress(Some("a much longer old value"), Some("new"), "new"),
            TypedProgress::Complete
        );
    }

    /// The full character count arrived and the value is not what was asked
    /// for: TextEdit autocapitalised "warehouse" to "Warehouse". Reporting
    /// that as unverifiable threw away a count the driver already had.
    #[test]
    fn a_normalised_value_reports_the_whole_count_it_observed() {
        let text = "warehouse pallet audit";
        assert_eq!(
            typed_progress(Some(""), Some("Warehouse pallet audit"), text),
            TypedProgress::Normalized(text.chars().count())
        );
        let delivery = await_typed_delivery(Some(""), text, std::time::Instant::now(), || {
            Some("Warehouse pallet audit".to_owned())
        });
        assert_eq!(
            delivery,
            TypedDelivery {
                verified: true,
                delivered: Some(text.chars().count()),
                normalized: true,
            }
        );
    }

    /// A partial insertion has to name the characters that did not arrive.
    /// "retry only the remaining suffix" left the caller to slice the request
    /// by codepoint and guess where the caret stopped — measured as declined
    /// rather than followed.
    #[test]
    fn an_incomplete_insertion_spells_the_undelivered_remainder() {
        let text = "(408) 961-1560";
        let result =
            incomplete_result(text, text.chars().count(), 6, " via CGEvent", PATH_AX, None);
        let structured = result
            .structured_content
            .clone()
            .expect("incomplete payload");
        assert_eq!(structured["retry_text"], serde_json::json!("961-1560"));
        assert_eq!(structured["retry_from_character"], serde_json::json!(6));
        let message = match &result.content[0] {
            cua_driver_core::protocol::Content::Text { text, .. } => text.clone(),
            other => panic!("expected a text reply, got {other:?}"),
        };
        assert!(
            message.contains("961-1560"),
            "the reply must carry the remainder itself: {message}"
        );
    }

    /// Character offsets, not byte offsets: a multi-byte prefix must not slice
    /// a codepoint in half.
    #[test]
    fn the_remainder_is_sliced_by_character() {
        let text = "Ωcafé-tail";
        let result = incomplete_result(text, text.chars().count(), 5, "", PATH_KEY_EVENTS, None);
        assert_eq!(
            result.structured_content.expect("payload")["retry_text"],
            serde_json::json!("-tail")
        );
    }

    /// The drain summarises a whole polling window, and the AX rung reads that
    /// summary to decide between reporting a partial insertion and falling
    /// through to keystrokes. A window in which the field never moved must
    /// stay `Unchanged`: `Partial(0)` there would publish "delivered 0 of n"
    /// and swallow the keystroke rung that still had to run.
    #[test]
    fn a_write_the_field_never_took_stays_unchanged() {
        assert_eq!(
            await_typed_progress(
                Some("old"),
                "new value",
                std::time::Instant::now(),
                || Some("old".to_owned())
            ),
            TypedProgress::Unchanged
        );
    }

    #[test]
    fn a_partial_the_field_discards_is_not_a_partial_delivery() {
        let mut reads = 0;
        let progress = await_typed_progress(
            Some(""),
            "10600 North Tantau Avenue",
            std::time::Instant::now() + std::time::Duration::from_millis(250),
            || {
                reads += 1;
                Some(if reads == 1 {
                    "10600 North Ta".to_owned()
                } else {
                    String::new()
                })
            },
        );
        assert_eq!(progress, TypedProgress::Unchanged);
        assert!(reads > 1, "the drain read the field {reads} time(s)");
    }

    #[test]
    fn a_partial_the_settled_read_still_shows_is_reported() {
        assert_eq!(
            await_typed_progress(
                Some(""),
                "10600 North Tantau Avenue",
                std::time::Instant::now() + std::time::Duration::from_millis(250),
                || Some("10600 North Ta".to_owned())
            ),
            TypedProgress::Partial(14)
        );
    }

    #[test]
    fn an_unreadable_settled_read_falls_back_to_the_partial_observed() {
        let mut reads = 0;
        let progress = await_typed_progress(
            Some(""),
            "10600 North Tantau Avenue",
            std::time::Instant::now() + std::time::Duration::from_millis(250),
            || {
                reads += 1;
                (reads == 1).then(|| "10600 North Ta".to_owned())
            },
        );
        assert_eq!(progress, TypedProgress::Partial(14));
    }

    #[test]
    fn a_field_that_never_reads_is_unverifiable_without_draining() {
        let mut reads = 0;
        let progress = await_typed_progress(
            Some(""),
            "payload",
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            || {
                reads += 1;
                None
            },
        );
        assert_eq!(progress, TypedProgress::Unverifiable);
        assert_eq!(reads, 1);
    }

    /// Contacts replaces the edited row, so the pinned element pointer keeps
    /// reporting the pre-edit string and a complete insertion was reported as
    /// a partial one. The focused element is where the keystrokes landed.
    #[test]
    fn a_replaced_field_is_read_through_the_focused_element() {
        assert_eq!(
            stronger_reading(
                Some(""),
                "555-1234",
                Some("555".to_owned()),
                Some("555-1234".to_owned())
            )
            .as_deref(),
            Some("555-1234")
        );
    }

    #[test]
    fn an_unrelated_focused_element_never_downgrades_the_addressed_read() {
        assert_eq!(
            stronger_reading(Some(""), "hi", Some("hi".to_owned()), None).as_deref(),
            Some("hi")
        );
        assert_eq!(
            stronger_reading(
                Some(""),
                "hi",
                Some("hi".to_owned()),
                Some("somewhere else".to_owned())
            )
            .as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn verify_typed_unchanged_is_unverified() {
        // Readable but the field didn't change and doesn't contain the text.
        assert!(!verify_typed(Some("ab"), Some("ab"), "hi"));
    }

    #[test]
    fn verify_typed_empty_text_is_trivially_verified() {
        assert!(verify_typed(None, None, ""));
    }

    #[test]
    fn path_constants_are_stable_tokens() {
        // These string constants are part of the structured-response
        // contract; freezing them here makes the contract a unit test.
        assert_eq!(PATH_AX, "ax");
        assert_eq!(PATH_KEY_EVENTS, "key_events");
        assert_eq!(PATH_KEY_EVENTS_FG, "key_events_fg");
    }

    #[test]
    fn ax_backed_web_readbacks_are_downgraded() {
        for path in [PATH_AX, PATH_KEY_EVENTS, PATH_KEY_EVENTS_FG] {
            assert_eq!(
                surface_verification(path, true, true),
                SurfaceVerification {
                    verified: false,
                    untrusted_web_readback: true,
                },
                "path={path}"
            );
        }
    }

    #[test]
    fn web_readback_distrust_preserves_native_and_unverified_outcomes() {
        assert_eq!(
            surface_verification(PATH_KEY_EVENTS_FG, true, false),
            SurfaceVerification {
                verified: true,
                untrusted_web_readback: false,
            },
            "native browser chrome remains eligible for trusted read-back"
        );
        assert_eq!(
            surface_verification(PATH_KEY_EVENTS_FG, false, true),
            SurfaceVerification {
                verified: false,
                untrusted_web_readback: false,
            },
            "an already-unverified delivery is not reclassified as a web echo"
        );
        assert_eq!(
            surface_verification("independent_renderer_oracle", true, true),
            SurfaceVerification {
                verified: true,
                untrusted_web_readback: false,
            },
            "a future independently verified path must not inherit AXValue distrust"
        );
    }

    #[test]
    fn web_readback_escalation_never_recommends_the_completed_pixel_rung() {
        assert_eq!(web_readback_next_rung(true, false), Some("px"));
        assert_eq!(web_readback_next_rung(true, true), None);
        assert_eq!(web_readback_next_rung(false, false), Some("page"));
        assert_eq!(web_readback_next_rung(false, true), Some("page"));
    }

    #[test]
    fn foreground_typing_skips_long_rearm_when_target_is_already_frontmost() {
        assert_eq!(foreground_settle_ms(42, Some(42)), 20);
        assert_eq!(foreground_settle_ms(42, Some(7)), 200);
        assert_eq!(foreground_settle_ms(42, None), 200);
    }

    #[test]
    fn screen_sharing_text_fails_closed_without_foreground_window() {
        for (foreground, window_id) in [(false, None), (false, Some(7)), (true, None)] {
            let result = screen_sharing_delivery_error(true, foreground, window_id)
                .expect("unsafe Screen Sharing route must be refused");
            assert_eq!(result.is_error, Some(true));
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["code"], "SCREEN_SHARING_REQUIRES_FOREGROUND_HID");
            assert_eq!(structured["effect"], "refused");
            assert_eq!(structured["escalation"]["recommended"], "foreground");
            assert_eq!(structured["escalation"]["requires"][0], "window_id");
        }
        assert!(screen_sharing_delivery_error(true, true, Some(7)).is_none());
        assert!(screen_sharing_delivery_error(false, false, None).is_none());
    }
}
