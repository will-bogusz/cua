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
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_legacy_click_input,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_action_names, copy_children, copy_string_attr, element_at_screen_position,
    element_screen_rect, kAXErrorSuccess, AXUIElementPerformAction, AXUIElementRef,
};
use crate::focus_guard;
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
#[derive(Debug, thiserror::Error)]
#[error("AXUIElementPerformAction({action}) returned {code}")]
struct AxActionReplyError {
    action: &'static str,
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
            _ => "AXError",
        }
    }
}

fn dispatch_ax_action(
    action: &'static str,
    dispatch: impl FnOnce() -> crate::ax::bindings::AXError,
) -> Result<(), AxActionReplyError> {
    let code = dispatch();
    if code == kAXErrorSuccess {
        Ok(())
    } else {
        Err(AxActionReplyError { action, code })
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

fn ax_action_error(error: anyhow::Error) -> ToolResult {
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

    fn advice(self) -> &'static str {
        match self {
            NextRung::PixelForeground => {
                "An AX press never carries pointer events, and fronting the app does not change \
                 that. To deliver a real click, take a fresh get_window_state screenshot and \
                 click this control's pixel center with delivery_mode:\"foreground\"."
            }
            NextRung::Foreground => {
                "A PID-routed click carries no trusted pointer events. Re-run this same pixel \
                 click with delivery_mode:\"foreground\" so macOS delivers it at the HID tap."
            }
        }
    }
}

/// Fold the background delivery probe's verdict into a click's reply, staying
/// inside the closed public `ActionResult` vocabulary
/// (`confirmed | partial | unverifiable | suspected_noop | refused`).
///
/// The probe answers "did the target react", never "did the click do what the
/// caller wanted". So:
/// * `Changed` → keep `unverifiable` (delivery is not the intended
///   postcondition) and publish the reaction as `window_change` evidence.
/// * `Unchanged` → `suspected_noop`, said loudly, escalated to the rung that
///   can still deliver. Never retried here and never silently re-routed: a
///   dispatched action can take effect invisibly, and acting twice is worse
///   than reporting an unproven one.
/// * `Unusable` → leave the existing contract alone; the probe had nothing
///   comparable to offer, and says so in the text.
fn apply_delivery_evidence(
    msg: &mut String,
    structured: &mut serde_json::Value,
    outcome: delivery_probe::ProbeOutcome,
    rung: NextRung,
    chromium_family: bool,
) {
    let probe_ms = outcome.probe.as_millis();
    let waited_ms = outcome.waited.as_millis();
    structured["delivery_probe"] = serde_json::json!({
        "signal": outcome.evidence.signal(),
        "probe_ms": probe_ms,
        "waited_ms": waited_ms,
    });
    match outcome.evidence {
        delivery_probe::Evidence::Changed(signal) => {
            structured["evidence"] = serde_json::json!([
                { "kind": "window_change", "detail": signal }
            ]);
            msg.push_str(&format!(
                "\n🔎 Delivered: {signal} changed after the dispatch, so the app reacted. \
                 That is delivery, not the intended result — check the postcondition you \
                 wanted."
            ));
        }
        delivery_probe::Evidence::Unchanged => {
            structured["effect"] = serde_json::json!("suspected_noop");
            structured["escalation"] = serde_json::json!({
                "recommended": rung.recommended(),
                "reason": if chromium_family {
                    "no observable change after background delivery; it cannot produce \
                     trusted pointer events, so controls driven by pointerdown \
                     (Chromium/Electron UIs) ignore it"
                } else {
                    "no observable change after background delivery; the probe reads \
                     element state, app focus, window contents and new windows only, so \
                     an effect it cannot see is still possible"
                }
            });
            msg.push_str(&format!(
                "\n⚠️ Unverified: the target was watched for {waited_ms} ms after the \
                 dispatch and nothing changed (element state, app focus, window contents, \
                 new windows) — re-observe before repeating. The dispatch may still have \
                 landed, so a second call could act twice.{} {}",
                if chromium_family {
                    " A control driven by pointerdown, common in Chromium/Electron UIs, \
                     ignores background delivery."
                } else {
                    ""
                },
                rung.advice()
            ));
        }
        delivery_probe::Evidence::Unusable => {
            msg.push_str(
                "\n❔ Delivery unverified: the target exposed no stable state to compare \
                 (element gone from the tree, or the window changes on its own). Confirm the \
                 postcondition yourself.",
            );
        }
    }
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
                "action":        { "type": "string",  "description": "AX action: press, show_menu, pick, confirm, cancel, open." },
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
                }
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
        let window_id_arg = args.opt_u64("window_id").map(|v| v as u32);
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
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
        let (element_index, window_id, _via_token) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None => (None, window_id_arg, false),
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: wid,
                element_index: idx,
                via_token,
            } => (Some(idx), wid, via_token),
        };
        let x = args
            .opt_f64("x")
            .or_else(|| args.opt_i64("x").map(|i| i as f64));
        let y = args
            .opt_f64("y")
            .or_else(|| args.opt_i64("y").map(|i| i as f64));
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
        let indexed_guard = if let (Some(idx), Some(wid), 2) = (element_index, window_id, count) {
            match self.state.element_cache.get_element_retained(pid, wid, idx) {
                Some(element) => Some(element),
                None => return ToolResult::error(
                    "Element is no longer cached; observe the exact window before double-clicking.",
                ),
            }
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

        if let (Some(idx), Some(wid), None) = (element_index, window_id, indexed_point) {
            // ── AX element path ────────────────────────────────────────────
            // Retain the element out of the cache so it can't be freed by a
            // concurrent get_window_state on the same (pid, window_id) while
            // this click is mid-flight (use-after-free → daemon crash). The
            // guard lives to the end of this method, past the AX action below.
            let element_guard = match self.state.element_cache.get_element_retained(pid, wid, idx) {
                Some(e) => e,
                None => {
                    return ToolResult::error(format!(
                        "Element index {idx} not found in cache for pid={pid} window_id={wid}. \
                     Call get_window_state first."
                    ))
                }
            };
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

            if !delivery_mode.is_foreground() && map_action(&effective_action) == "AXOpen" {
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
            let center_ptr = element_ptr;
            let center = cua_driver_core::operation::spawn_blocking(move || unsafe {
                crate::ax::bindings::element_screen_center(center_ptr as AXUIElementRef)
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
            }

            // Finder icon/list items can expose a readable AXSelected state
            // while refusing both AXSelected writes and AXPress. Resolve a
            // verified coordinate frame only for those collection-like
            // elements so perform_ax_click can cross that one failed semantic
            // rung internally and confirm the result by AX read-back.
            let selection_candidate = if effective_action == "press" {
                cua_driver_core::operation::spawn_blocking(move || {
                    crate::input::ax_actions::nearest_container_selection_state(element_ptr)
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
                        if foreground {
                            let mut outcome = None;
                            let has_modifiers = !selection_modifiers.is_empty();
                            let action = || {
                                outcome = Some(perform_ax_click(
                                    element_ptr,
                                    idx,
                                    pid,
                                    wid,
                                    &action_clone,
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
                                &action_clone,
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
                Ok(Ok((
                    AxClickOutcome {
                        summary: mut msg,
                        needs_webkit_delay,
                        suspected_noop,
                        selection_verified,
                        selection_via_pixel,
                        unverified,
                    },
                    fronted,
                ))) => {
                    // For text inputs, wait 800ms for WebKit DOM focus to settle
                    // before returning — matches the Swift reference behaviour.
                    if needs_webkit_delay {
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                    msg.push_str(&changes.result_suffix());
                    // A window that appeared during the action (sheet, dialog,
                    // popover) is delivery evidence on its own — no second
                    // probe sample needed for it.
                    let evidence = if selection_verified {
                        None
                    } else if changes.needs_restore() {
                        probe.map(|probe| {
                            probe.settled(delivery_probe::Evidence::Changed("window_change"))
                        })
                    } else if let Some(probe) = probe {
                        cua_driver_core::operation::spawn_blocking(move || probe.compare())
                            .await
                            .ok()
                    } else {
                        None
                    };
                    // AX dispatch went through, but AXPerformAction returning
                    // success does not confirm the on-screen effect (many elements
                    // no-op silently). A click is never driver-verifiable (no
                    // read-back) → verified:false stays for back-compat. The
                    // `effect` string is the richer signal:
                    //   * suspected_noop — the element didn't advertise the action,
                    //     so the press likely did nothing → cross to vision/pixel.
                    //   * unverifiable — dispatched fine, driver just can't confirm;
                    //     the caller verifies via screenshot.
                    //   * delivered / no_observed_change — the background delivery
                    //     probe below saw the target react, or saw nothing react.
                    let mut structured = serde_json::json!({
                        "path": if selection_via_pixel {
                            if fronted { "cgevent_fg" } else { "cgevent" }
                        } else if fronted {
                            "ax_fg"
                        } else {
                            "ax"
                        },
                        "verified": selection_verified,
                        "effect": if selection_verified {
                            "confirmed"
                        } else if suspected_noop {
                            "suspected_noop"
                        } else {
                            "unverifiable"
                        },
                    });
                    if selection_verified {
                        structured["evidence"] = serde_json::json!([
                            { "kind": "accessibility_readback" }
                        ]);
                    }
                    // The application answered the dispatch with a code that
                    // establishes neither delivery nor a no-op, so the mode
                    // the action was delivered in is not known either.
                    if unverified.is_some() {
                        structured["delivery_mode"] = serde_json::json!("unknown");
                    }
                    if suspected_noop {
                        structured["escalation"] = serde_json::json!({
                            "recommended": "px",
                            "reason": "element does not advertise this action — the \
                                       AX press likely no-op'd. Do an element px \
                                       action: click by pixel (x,y) off the \
                                       screenshot from get_window_state."
                        });
                    }
                    if let Some(outcome) = evidence {
                        let chromium_family =
                            matches!(outcome.evidence, delivery_probe::Evidence::Unchanged)
                                && cua_driver_core::operation::spawn_blocking(move || {
                                    crate::browser::is_chromium_family(pid)
                                })
                                .await
                                .unwrap_or(false);
                        apply_delivery_evidence(
                            &mut msg,
                            &mut structured,
                            outcome,
                            // The selection fallback already delivered real
                            // pixels; everything else on this branch is an AX
                            // action, which foreground cannot upgrade.
                            if selection_via_pixel {
                                NextRung::Foreground
                            } else {
                                NextRung::PixelForeground
                            },
                            chromium_family,
                        );
                    }
                    ToolResult::text(msg).with_structured(structured)
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
                            probe.settled(delivery_probe::Evidence::Changed("window_change"))
                        })
                    } else if let Some(probe) = probe {
                        cua_driver_core::operation::spawn_blocking(move || probe.compare())
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
                        let chromium_family =
                            matches!(outcome.evidence, delivery_probe::Evidence::Unchanged)
                                && cua_driver_core::operation::spawn_blocking(move || {
                                    crate::browser::is_chromium_family(pid)
                                })
                                .await
                                .unwrap_or(false);
                        apply_delivery_evidence(
                            &mut msg,
                            &mut structured,
                            outcome,
                            NextRung::Foreground,
                            chromium_family,
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

fn perform_ax_click(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    action_str: &str,
    cursor_key: &str,
    selection_pixel: Option<SelectionPixelTarget>,
    menu_pixel: Option<SelectionPixelTarget>,
    modifiers: &[String],
    foreground: bool,
) -> anyhow::Result<AxClickOutcome> {
    let ax_action = map_action(action_str);
    let element = element_ptr as AXUIElementRef;

    // Check the live value immediately before dispatch. Foreground assist can
    // enable menu items that were disabled in the cached snapshot, while a
    // background transition can disable them after that snapshot. macOS may
    // otherwise return success for a disabled action that did nothing.
    crate::input::ax_actions::ensure_ax_action_enabled(element_ptr, ax_action)?;

    // Capture advertised actions BEFORE dispatching so we can detect silent no-ops
    // (AX returns success even when the element doesn't advertise the action).
    let advertised = unsafe { copy_action_names(element) };

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();

    // A click on an AppKit collection item is frequently represented by a
    // label child or row that does not advertise AXPress. Prefer a bounded,
    // read-back-verified AXSelected write over dispatching a known hollow press
    // or forcing the caller onto a less stable pixel coordinate.
    if ax_action == "AXPress" && !advertised.iter().any(|action| action == ax_action) {
        if modifiers.is_empty() {
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr)
            {
                return Ok(AxClickOutcome {
                    summary: format!(
                        "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\"; \
                         confirmed AXSelected=true."
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
                                         AXSelected read-back."
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
    }

    let menus_before = (ax_action == "AXShowMenu")
        .then(|| crate::windows::accessory_window_ids(pid))
        .unwrap_or_default();

    let reply = dispatch_ax_action(ax_action, || unsafe {
        crate::ax::bindings::perform_action(element, ax_action)
    });
    let unverified = match reply {
        Ok(()) => None,
        Err(reply) => {
            let err = reply.code;
            // Some collection rows claim a click-like action but Finder returns
            // kAXErrorCannotComplete. Use the same verified selection fallback
            // before surfacing the dispatch error.
            if ax_action == "AXPress" && modifiers.is_empty() {
                if let Some(selected_role) =
                    crate::input::ax_actions::select_nearest_container(element_ptr)
                {
                    return Ok(AxClickOutcome {
                        summary: format!(
                            "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\" \
                             after AXPress returned {err}; confirmed AXSelected=true."
                        ),
                        selection_verified: true,
                        ..AxClickOutcome::default()
                    });
                }
            }
            if !reply.outcome_unverifiable() {
                return Err(reply.into());
            }
            Some(reply)
        }
    };

    let mut summary = ax_reply_summary(unverified.as_ref(), ax_action, idx, &role, &title);

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

    // AXPopUpButton: list available options, redirect to set_value.
    if role == "AXPopUpButton" {
        let children = unsafe { copy_children(element) };
        if !children.is_empty() {
            let options: Vec<String> = children
                .iter()
                .filter_map(|&child| {
                    let t = unsafe { copy_string_attr(child, "AXTitle") }.unwrap_or_default();
                    let v = unsafe { copy_string_attr(child, "AXValue") }.unwrap_or_default();
                    if t.is_empty() && v.is_empty() {
                        return None;
                    }
                    Some(if v.is_empty() || v == t {
                        format!("\"{t}\"")
                    } else {
                        format!("\"{t}\" (value: {v})")
                    })
                })
                .collect();
            for &child in &children {
                unsafe {
                    CFRelease(child as _);
                }
            }

            if !options.is_empty() {
                let opt_list = options.join(", ");
                summary.push_str(
                    "\n\n⚠️ This is a popup/select button. The native macOS menu closes \
                     immediately when the window is in the background. Do NOT use click \
                     again — instead, use:\n  set_value(pid, window_id, element_index, value)\n\
                     Available options: [",
                );
                summary.push_str(&opt_list);
                summary.push(']');
            }
        }
    }

    // Advertised-action warning: non-fatal but surfaces likely no-ops. Also the
    // machine-readable `suspected_noop` signal returned to the caller.
    let suspected_noop = !advertised.contains(&ax_action.to_string());
    if suspected_noop {
        let adv_list = if advertised.is_empty() {
            "none".into()
        } else {
            advertised.join(", ")
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

fn map_action(action: &str) -> &'static str {
    match action.to_lowercase().as_str() {
        "press" | "click" => "AXPress",
        "show_menu" | "right_click" => "AXShowMenu",
        "pick" => "AXPick",
        "confirm" => "AXConfirm",
        "cancel" => "AXCancel",
        "open" => "AXOpen",
        _ => "AXPress",
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(evidence: delivery_probe::Evidence, waited_ms: u64) -> delivery_probe::ProbeOutcome {
        delivery_probe::ProbeOutcome {
            evidence,
            probe: std::time::Duration::from_millis(waited_ms + 250),
            waited: std::time::Duration::from_millis(waited_ms),
        }
    }

    /// The probe reads four signals; an effect outside them is invisible to
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
            outcome(delivery_probe::Evidence::Unchanged, 2000),
            NextRung::PixelForeground,
            false,
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
        assert!(ax_msg.contains("re-observe before repeating"), "{ax_msg}");
        assert!(!ax_msg.contains("NOT delivered"), "{ax_msg}");
        assert!(ax_msg.contains("delivery_mode:\"foreground\""), "{ax_msg}");

        let mut pixel_msg = "✅ Posted click to pid 1.".to_owned();
        let mut pixel = serde_json::json!({ "path": "cgevent", "effect": "unverifiable" });
        apply_delivery_evidence(
            &mut pixel_msg,
            &mut pixel,
            outcome(delivery_probe::Evidence::Unchanged, 2000),
            NextRung::Foreground,
            false,
        );
        assert_eq!(pixel["escalation"]["recommended"], "foreground");
    }

    /// Pointerdown advice belongs to Chromium-family targets: on an AppKit app
    /// it sent the model hunting a cause that does not exist there.
    #[test]
    fn pointerdown_advice_is_reserved_for_chromium_family_targets() {
        let mut native_msg = String::new();
        let mut native = serde_json::json!({ "path": "ax" });
        apply_delivery_evidence(
            &mut native_msg,
            &mut native,
            outcome(delivery_probe::Evidence::Unchanged, 2000),
            NextRung::PixelForeground,
            false,
        );
        assert!(!native_msg.contains("pointerdown"), "{native_msg}");
        assert!(
            !native["escalation"]["reason"]
                .as_str()
                .expect("reason")
                .contains("pointerdown"),
            "{native}"
        );

        let mut chromium_msg = String::new();
        let mut chromium = serde_json::json!({ "path": "ax" });
        apply_delivery_evidence(
            &mut chromium_msg,
            &mut chromium,
            outcome(delivery_probe::Evidence::Unchanged, 2000),
            NextRung::PixelForeground,
            true,
        );
        assert!(chromium_msg.contains("pointerdown"), "{chromium_msg}");
    }

    #[test]
    fn observed_change_is_published_as_evidence_without_claiming_the_postcondition() {
        let mut msg = "✅ Performed AXPress on [3] AXButton \"B7\".".to_owned();
        let mut structured = serde_json::json!({ "path": "ax", "effect": "unverifiable" });
        apply_delivery_evidence(
            &mut msg,
            &mut structured,
            outcome(delivery_probe::Evidence::Changed("element_state"), 120),
            NextRung::PixelForeground,
            false,
        );
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["evidence"][0]["kind"], "window_change");
        assert!(structured["escalation"].is_null());
        assert!(msg.contains("Delivered: element_state changed"), "{msg}");
    }

    #[test]
    fn unusable_probe_leaves_the_existing_effect_alone() {
        let mut msg = "✅ Performed AXPress on [3] AXButton \"B7\".".to_owned();
        let mut structured = serde_json::json!({ "path": "ax", "effect": "suspected_noop" });
        apply_delivery_evidence(
            &mut msg,
            &mut structured,
            outcome(delivery_probe::Evidence::Unusable, 2000),
            NextRung::PixelForeground,
            false,
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

    /// A reply the framework produced rather than an application answering an
    /// action — a dead element, disabled API, a messaging timeout — keeps the
    /// error contract and its reconcile-first advice: nothing there says the
    /// request ever reached the receiver.
    #[test]
    fn framework_reply_errors_stay_failures_that_ask_for_reconciliation() {
        let mut receiver_commits = 0;
        let failure = dispatch_ax_action("AXPress", || {
            receiver_commits += 1;
            crate::ax::bindings::kAXErrorInvalidUIElement
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
        assert_eq!(data["ax_error"], -25202);

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
