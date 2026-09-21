//! MCP tool implementations for macOS.

mod bring_to_front;
mod click;
mod clipboard;
pub(crate) mod delivery_probe;
mod double_click;
mod drag;
mod get_window_state;
mod hotkey;
mod invoke_menu;
mod kill_app;
mod launch_app;
mod list_apps;
mod list_windows;
mod press_key;
mod right_click;
mod scroll;
mod set_value;
mod set_window_frame;
mod type_text;
// `screenshot` / `screenshot_compat` modules removed in PR #1692 —
// `get_window_state` capture_mode:"vision" is the canonical screenshot
// path. The capture functions they wrapped (ScreenCaptureKit, CGWindow,
// etc.) live elsewhere under CuaDriverCore::Capture and are reached
// through GetWindowStateTool.
mod check_permissions;
mod cursor_tools;
mod get_accessibility_tree;
mod get_config;
mod get_cursor_position;
mod get_desktop_state;
pub(crate) mod get_screen_size;
mod health_report;
mod move_cursor;
mod page;
pub(crate) mod px_frame;
mod set_config;
mod type_text_chars;
mod zoom;

use cua_driver_core::{
    tool::{Tool, ToolRegistry},
    window_target::{PidOnlyWindowTargetGuard, WindowTargetCandidate, WindowTargetCandidates},
};
use std::collections::HashMap;
use std::sync::Arc;

use crate::{ax::cache::ElementCache, cursor::state::CursorRegistry};

fn native_window_id(
    window_id: Option<u64>,
) -> Result<Option<u32>, cua_driver_core::protocol::ToolResult> {
    window_id.map(u32::try_from).transpose().map_err(|_| {
        cua_driver_core::protocol::ToolResult::error("window_id is out of range for macOS.")
    })
}

#[cfg(test)]
mod snapshot_window_id_tests {
    #[test]
    fn native_window_conversion_never_discards_high_bits() {
        assert_eq!(super::native_window_id(None).unwrap(), None);
        assert_eq!(
            super::native_window_id(Some(u32::MAX as u64)).unwrap(),
            Some(u32::MAX)
        );
        assert!(super::native_window_id(Some((1_u64 << 32) | 7)).is_err());
    }
}

fn pid_window_target_candidates(pid: i64) -> Vec<WindowTargetCandidate> {
    let Ok(pid) = i32::try_from(pid) else {
        return Vec::new();
    };
    window_target_candidates_for_pid(crate::windows::all_windows(), pid)
}

fn window_target_candidates_for_pid(
    windows: impl IntoIterator<Item = crate::windows::WindowInfo>,
    pid: i32,
) -> Vec<WindowTargetCandidate> {
    windows
        .into_iter()
        .filter(|window| window.pid == pid)
        .map(|window| WindowTargetCandidate {
            window_id: u64::from(window.window_id),
            title: window.title,
            app_name: Some(window.app_name),
            is_on_screen: window.is_on_screen,
        })
        .collect()
}

#[cfg(test)]
mod pid_window_target_tests {
    use super::*;
    use cua_driver_core::window_target::{resolve_pid_window_target, PidWindowTargetResolution};

    fn window(window_id: u32, pid: i32) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: "Editor".into(),
            title: format!("Document {window_id}"),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width: 640.0,
                height: 480.0,
            },
            layer: 0,
            z_index: 1,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn same_pid_sibling_windows_are_ambiguous() {
        let candidates =
            window_target_candidates_for_pid([window(7, 42), window(8, 42), window(9, 99)], 42);
        assert!(matches!(
            resolve_pid_window_target(candidates),
            PidWindowTargetResolution::Ambiguous(windows)
                if windows.iter().map(|window| window.window_id).collect::<Vec<_>>() == [7, 8]
        ));
    }
}

#[cfg(test)]
mod background_input_regression_tests;

fn pid_window_guarded<T: Tool + 'static>(
    tool: T,
    candidates: &WindowTargetCandidates,
) -> Box<dyn Tool> {
    Box::new(PidOnlyWindowTargetGuard::new(
        Box::new(tool),
        candidates.clone(),
    ))
}

pub use check_permissions::{
    request_from_launchservices_host as request_permissions_from_launchservices_host,
    PERMISSIONS_HOST_REQUEST_ARG,
};

/// Per-process zoom context — stores the padded crop origin and resize scale
/// from the most recent `zoom` call, so `click(from_zoom=true)` can translate
/// zoom-image pixel coordinates back to full-window coordinates.
#[derive(Clone, Copy, Debug)]
pub struct ZoomContext {
    /// Padded crop X origin in full-window pixel space.
    pub origin_x: f64,
    /// Padded crop Y origin in full-window pixel space.
    pub origin_y: f64,
    /// Inverse resize scale: `cw / out_w` (1.0 = no downscale).
    pub scale_inv: f64,
}

impl ZoomContext {
    /// Translate a zoom-image coordinate `(px, py)` to full-window pixel coordinates.
    pub fn zoom_to_window(&self, px: f64, py: f64) -> (f64, f64) {
        (
            self.origin_x + px * self.scale_inv,
            self.origin_y + py * self.scale_inv,
        )
    }
}

/// Input delivery modality — the agent-selected rung of the best-effort-background
/// ladder, passed per call (never a stored/config setting).
///
/// - `Background` (default): post synthetic input to the pid without fronting.
/// - `Foreground`: briefly front the target window, act, then restore the prior
///   frontmost (see [`crate::input::skylight::with_foreground_assist`]). The
///   agent's vision-driven last resort — and the only way `click` reaches a
///   foreground rung. Orthogonal to addressing (`element_index` vs `x/y`, which
///   selects AX vs pixel).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeliveryMode {
    #[default]
    Background,
    Foreground,
}

impl DeliveryMode {
    /// Parse the per-call `delivery_mode` argument. Anything other than an
    /// explicit case-insensitive `"foreground"` resolves to `Background` — the
    /// correct default, so an omitted/garbage value never silently fronts.
    pub fn parse(arg: Option<&str>) -> Self {
        match arg {
            Some(s) if s.eq_ignore_ascii_case("foreground") => Self::Foreground,
            _ => Self::Background,
        }
    }

    pub fn is_foreground(self) -> bool {
        matches!(self, Self::Foreground)
    }
}

/// A window one process has drawn in front of another of its own windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObscuringWindow {
    pub window_id: u32,
    pub title: String,
    pub layer: i32,
    pub ax_backed: Option<bool>,
    pub role: Option<String>,
    pub subrole: Option<String>,
    /// `AXModal`: the application's own report that this window blocks every
    /// other window of its process. `None` when unread or unanswered.
    pub modal: Option<bool>,
}

impl ObscuringWindow {
    /// Resolve one window's identity: WindowServer's title and layer, plus the
    /// AX window the owning process publishes for it, if any.
    pub(crate) fn resolve(pid: i32, window_id: u32) -> Option<Self> {
        let info = crate::windows::window_info_by_id(window_id)?;
        let mut resolved = Self {
            window_id,
            title: info.title,
            layer: info.layer,
            ax_backed: None,
            role: None,
            subrole: None,
            modal: None,
        };
        resolved.resolve_ax_identity(pid);
        Some(resolved)
    }

    /// Attribute the CGWindowID to one of the process's own `AXWindow`
    /// elements. A layer-0 panel that publishes none can never become the AX
    /// focused window, which is what separates "acquire it" from "dismiss it".
    pub(crate) fn resolve_ax_identity(&mut self, pid: i32) {
        // SAFETY: the application element and every window it hands back are
        // created and released inside this block.
        unsafe {
            let app = crate::ax::bindings::AXUIElementCreateApplication(pid);
            if app.is_null() {
                return;
            }
            self.ax_backed = Some(false);
            for window in crate::ax::bindings::copy_ax_windows(app) {
                if self.ax_backed != Some(true)
                    && crate::ax::bindings::ax_get_window_id(window) == Some(self.window_id)
                {
                    self.ax_backed = Some(true);
                    self.role = crate::ax::bindings::copy_string_attr(window, "AXRole");
                    self.subrole = crate::ax::bindings::copy_string_attr(window, "AXSubrole");
                    self.modal = crate::ax::bindings::copy_bool_attr(window, "AXModal");
                }
                core_foundation::base::CFRelease(window as core_foundation::base::CFTypeRef);
            }
            core_foundation::base::CFRelease(app as core_foundation::base::CFTypeRef);
        }
    }

    /// The `obscured_by` payload a refusal or a verified-behind reply carries.
    pub(crate) fn payload(&self) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "window_id": self.window_id,
            "title": self.title,
            "layer": self.layer,
        });
        if let Some(ax_backed) = self.ax_backed {
            payload["ax_backed"] = serde_json::json!(ax_backed);
        }
        if let Some(role) = &self.role {
            payload["role"] = serde_json::json!(role);
        }
        if let Some(subrole) = &self.subrole {
            payload["subrole"] = serde_json::json!(subrole);
        }
        if let Some(modal) = self.modal {
            payload["modal"] = serde_json::json!(modal);
        }
        payload
    }

    /// Whether this window keeps the keyboard after the window behind it is
    /// raised and made key. A window with no `AXWindow` cannot be moved past by
    /// giving another window key status (the process never publishes a key
    /// window to move it to), and a window the application reports modal
    /// refuses key status to every other window of the process. Any other
    /// window in front is simply the process's current front window, which
    /// the foreground rung overtakes: the SkyLight make-key records followed
    /// by `AXRaise` on the exact target window (`with_foreground_assist`).
    pub(crate) fn holds_keyboard_through_raise(&self) -> bool {
        !self.is_focusable() || self.modal == Some(true)
    }

    /// Whether the window publishes an `AXWindow` the caller could focus.
    pub(crate) fn is_focusable(&self) -> bool {
        self.ax_backed != Some(false)
    }

    /// How prose names the window: its title, then its AX identity.
    pub(crate) fn describe(&self) -> String {
        let title = if self.title.trim().is_empty() {
            "titleless".to_owned()
        } else {
            format!("titled {:?}", self.title)
        };
        match (&self.role, &self.subrole) {
            (Some(role), Some(subrole)) => format!("{title}, {role}/{subrole}"),
            (Some(role), None) => format!("{title}, {role}"),
            (None, _) if self.ax_backed == Some(false) => format!("{title}, no AX surface"),
            (None, _) => title,
        }
    }
}

/// Where an exact target sits in its own process's front-to-back order.
pub(crate) struct ProcessFrontOrder {
    pub target_is_front: bool,
    pub in_front: Option<ObscuringWindow>,
}

/// Resolve [`ProcessFrontOrder`] from the window roster `list_windows`
/// classifies: every layer-0 row, off-screen ones included.
pub(crate) fn process_front_order(pid: i32, window_id: u32) -> ProcessFrontOrder {
    let windows = crate::windows::all_windows();
    let mut order = resolve_process_front_order(
        &windows,
        pid,
        window_id,
        crate::window_kind::runs_indicator_provider,
    );
    if let Some(in_front) = &mut order.in_front {
        in_front.resolve_ax_identity(pid);
    }
    order
}

/// Which of the process's own windows is drawn in front, over one captured
/// enumeration and without consulting the accessibility tree.
///
/// `windows` is the whole roster, off-screen rows included, because that is
/// what the classification rule needs: the capture-lease indicator is named by
/// the provider view it encloses, and that view is enumerated off-screen while
/// the indicator hosting it is on-screen. Only on-screen rows are candidates
/// for the window in front.
///
/// A row the window roster classifies `system_overlay` is excluded: the
/// capture-lease indicator carries the captured application's pid on layer 0,
/// but the system draws it on the process's behalf, so it is never a window
/// the application put in front of its own.
fn resolve_process_front_order(
    windows: &[crate::windows::WindowInfo],
    pid: i32,
    window_id: u32,
    is_indicator_provider_pid: impl Fn(i32) -> bool,
) -> ProcessFrontOrder {
    let system_overlays =
        crate::window_kind::window_sharing_indicator_candidates(windows, is_indicator_provider_pid);
    let front = windows
        .iter()
        .filter(|window| {
            window.pid == pid
                && window.layer == 0
                && is_process_owned_window(window, &system_overlays)
        })
        .max_by_key(|window| window.z_index);
    let Some(front) = front else {
        return ProcessFrontOrder {
            target_is_front: false,
            in_front: None,
        };
    };
    if front.window_id == window_id {
        return ProcessFrontOrder {
            target_is_front: true,
            in_front: None,
        };
    }
    ProcessFrontOrder {
        target_is_front: false,
        in_front: Some(ObscuringWindow {
            window_id: front.window_id,
            title: front.title.clone(),
            layer: front.layer,
            ax_backed: None,
            role: None,
            subrole: None,
            modal: None,
        }),
    }
}

/// Whether a row may be attributed to the process that owns it as a window it
/// drew in front. Off-screen rows are enumerated only so the roster can
/// classify the on-screen ones; the driver's own cursor overlay and the rows
/// the window roster classifies `system_overlay` are drawn over an
/// application's windows without being one of them.
pub(crate) fn is_process_owned_window(
    window: &crate::windows::WindowInfo,
    system_overlays: &[u32],
) -> bool {
    window.is_on_screen
        && !system_overlays.contains(&window.window_id)
        && !crate::cursor::overlay::is_overlay_window(window.window_id)
}

#[cfg(test)]
mod process_front_order_tests {
    use super::*;
    use crate::windows::{WindowBounds, WindowInfo};

    const NOTES_PID: i32 = 84264;
    const PROVIDER_PID: i32 = 22402;
    const LEASED_NOTES_PID: i32 = 21552;
    const LEASE_PROVIDER_PID: i32 = 21772;

    fn window(
        window_id: u32,
        z_index: usize,
        title: &str,
        bounds: (f64, f64, f64, f64),
    ) -> WindowInfo {
        let (x, y, width, height) = bounds;
        WindowInfo {
            window_id,
            pid: NOTES_PID,
            app_name: "Notes".into(),
            title: title.to_owned(),
            bounds: WindowBounds {
                x,
                y,
                width,
                height,
            },
            layer: 0,
            z_index,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    /// The measured layout: the 66x20 capture-lease indicator carries Notes'
    /// own pid, sits above its document window, and hosts the indicator
    /// provider's view.
    fn notes_with_capture_indicator() -> Vec<WindowInfo> {
        let mut provider_view = window(19099, 6, "", (200.0, 96.0, 14.0, 14.0));
        provider_view.pid = PROVIDER_PID;
        provider_view.app_name = "ThemeWidgetControlViewService".into();
        vec![
            window(19080, 1, "Notes", (0.0, 34.0, 1696.0, 1083.0)),
            window(19083, 5, "Window", (191.0, 90.0, 66.0, 20.0)),
            provider_view,
        ]
    }

    #[test]
    fn a_capture_lease_indicator_is_not_the_window_in_front() {
        let order =
            resolve_process_front_order(&notes_with_capture_indicator(), NOTES_PID, 19080, |pid| {
                pid == PROVIDER_PID
            });
        assert!(order.target_is_front, "{:?}", order.in_front);
        assert_eq!(order.in_front, None);
    }

    /// The same geometry without a provider view inside it is an ordinary
    /// window of the process, and is still named as the one in front.
    #[test]
    fn an_unclassified_small_window_in_front_is_still_named() {
        let mut windows = notes_with_capture_indicator();
        windows.pop();
        let order =
            resolve_process_front_order(&windows, NOTES_PID, 19080, |pid| pid == PROVIDER_PID);
        assert!(!order.target_is_front);
        assert_eq!(
            order.in_front.map(|in_front| in_front.window_id),
            Some(19083)
        );
    }

    #[test]
    fn the_indicator_does_not_hide_a_real_panel_behind_it() {
        let mut windows = notes_with_capture_indicator();
        windows.push(window(19091, 3, "Print", (300.0, 200.0, 620.0, 480.0)));
        let order =
            resolve_process_front_order(&windows, NOTES_PID, 19080, |pid| pid == PROVIDER_PID);
        assert!(!order.target_is_front);
        let in_front = order.in_front.expect("the app's own panel");
        assert_eq!(in_front.window_id, 19091);
        assert_eq!(in_front.title, "Print");
    }

    /// The enumeration measured under a live capture lease (pid 21552): the
    /// 66x20 indicator is on-screen, and the provider view whose enclosure
    /// names it is enumerated off-screen, as is one more window of Notes.
    fn notes_under_a_capture_lease() -> Vec<WindowInfo> {
        let mut document = window(19177, 3, "All iCloud", (100.0, 100.0, 1363.0, 850.0));
        document.pid = LEASED_NOTES_PID;
        let mut off_screen = window(19178, 2, "", (0.0, 617.0, 500.0, 500.0));
        off_screen.pid = LEASED_NOTES_PID;
        off_screen.is_on_screen = false;
        let mut indicator = window(19180, 5, "Window", (116.0, 116.0, 66.0, 20.0));
        indicator.pid = LEASED_NOTES_PID;
        let mut provider_view = window(19179, 6, "", (116.0, 116.0, 66.0, 20.0));
        provider_view.pid = LEASE_PROVIDER_PID;
        provider_view.app_name = "ThemeWidgetControlViewService".into();
        provider_view.is_on_screen = false;
        vec![document, off_screen, indicator, provider_view]
    }

    #[test]
    fn an_off_screen_provider_view_still_names_the_indicator_it_sits_in() {
        let order = resolve_process_front_order(
            &notes_under_a_capture_lease(),
            LEASED_NOTES_PID,
            19177,
            |pid| pid == LEASE_PROVIDER_PID,
        );
        assert!(order.target_is_front, "{:?}", order.in_front);
        assert_eq!(order.in_front, None);
    }

    /// A row the roster carries only so the indicator can be classified is not
    /// drawn at all, so it is never the window the process put in front.
    #[test]
    fn an_off_screen_row_of_the_process_is_never_the_window_in_front() {
        let mut windows = notes_under_a_capture_lease();
        windows
            .iter_mut()
            .find(|window| window.window_id == 19178)
            .expect("the off-screen row")
            .z_index = 9;
        let order = resolve_process_front_order(&windows, LEASED_NOTES_PID, 19177, |pid| {
            pid == LEASE_PROVIDER_PID
        });
        assert!(order.target_is_front, "{:?}", order.in_front);
        assert_eq!(order.in_front, None);
    }
}

/// Convert a pure background-input refusal into the structured refusal result
/// shape shared by exact-target tools: `code`, `effect: "refused"`, the
/// requested target, and the safe next route when one exists. No actuator ran.
pub(crate) fn background_refusal_result(
    pid: i32,
    window_id: u32,
    refusal: &cua_driver_core::background_input::BackgroundRefusal,
) -> cua_driver_core::protocol::ToolResult {
    let mut structured = serde_json::json!({
        "code": refusal.code,
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "reason": refusal.reason,
    });
    if let Some(advice) = refusal.advice {
        structured["advice"] = serde_json::json!(advice.as_str());
        if let Some(target) = advice.escalation_target() {
            structured["escalation"] = serde_json::json!({
                "target": target,
                "reason": "route_unavailable",
            });
        }
    }
    cua_driver_core::protocol::ToolResult::error(format!(
        "Background input refused ({}): {}",
        refusal.code, refusal.reason
    ))
    .with_structured(structured)
}

/// Exclusive per-process ownership of one background mutation. Callers must
/// keep this value alive through actuator dispatch, focus restoration, and
/// target-bound verification.
pub(crate) struct BackgroundMutationLease {
    pid: i32,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl BackgroundMutationLease {
    /// Revalidate another actuator class while retaining the same per-PID
    /// lease. This supports explicit ladders without deadlocking by attempting
    /// to reacquire the coordinator recursively.
    pub(crate) async fn gate_again(
        &self,
        window_id: u32,
        element_ptr: Option<usize>,
        action: cua_driver_core::background_input::BackgroundAction,
    ) -> Result<(), cua_driver_core::protocol::ToolResult> {
        decide_background_window_action(self.pid, window_id, element_ptr, action).await
    }
}

async fn decide_background_window_action(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
    action: cua_driver_core::background_input::BackgroundAction,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundInputDecision, ExactWindowTarget,
    };
    let element_guard = element_ptr.map(|ptr| unsafe { crate::ax::RetainedElement::retain(ptr) });
    let facts = match tokio::task::spawn_blocking(move || {
        let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());
        crate::ax::exact_target::gather_background_facts(pid, window_id, element_ptr)
    })
    .await
    {
        Ok(facts) => facts,
        Err(error) => {
            return Err(cua_driver_core::protocol::ToolResult::error(format!(
                "Could not gather exact-target facts for pid {pid} window {window_id}: {error}"
            )));
        }
    };
    match decide_background_input(ExactWindowTarget { pid, window_id }, &facts, action) {
        BackgroundInputDecision::Execute { .. } => Ok(()),
        BackgroundInputDecision::Refuse(refusal) => {
            Err(background_refusal_result(pid, window_id, &refusal))
        }
    }
}

/// Acquire the per-PID mutation coordinator, gather fresh exact-target facts,
/// and ask the pure core for one background decision. `element_ptr` must stay
/// retained by the caller until the returned lease is dropped.
pub(crate) async fn gate_background_window_action(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
    action: cua_driver_core::background_input::BackgroundAction,
) -> Result<BackgroundMutationLease, cua_driver_core::protocol::ToolResult> {
    let lease = acquire_background_mutation(pid).await;
    lease.gate_again(window_id, element_ptr, action).await?;
    Ok(lease)
}

pub(crate) async fn acquire_background_mutation(pid: i32) -> BackgroundMutationLease {
    BackgroundMutationLease {
        pid,
        _guard: crate::background_mutation::acquire(pid).await,
    }
}

/// Finish the post-action observation window. A caller that enumerates windows
/// itself may opt out with `detect_window_change: false` and keep the up-to-one-
/// second poll off its action latency; embedded interactive clients opt out
/// through the private registry argument. Otherwise the full observer runs.
pub(crate) async fn finish_window_observation(
    snapshot: crate::window_change_detector::Snapshot,
    args: &serde_json::Value,
) -> crate::window_change_detector::Changes {
    if window_change_detection_declined(args) {
        drop(snapshot);
        crate::window_change_detector::Changes::not_polled()
    } else {
        snapshot.detect_async().await
    }
}

pub(crate) fn window_change_detection_declined(args: &serde_json::Value) -> bool {
    args.get("_skip_window_change_detection")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || args
            .get("detect_window_change")
            .and_then(serde_json::Value::as_bool)
            .is_some_and(|detect| !detect)
}

#[cfg(test)]
mod interactive_observation_tests {
    use super::*;

    #[test]
    fn only_an_explicit_false_declines_the_window_poll() {
        assert!(window_change_detection_declined(
            &serde_json::json!({"detect_window_change": false})
        ));
        assert!(window_change_detection_declined(
            &serde_json::json!({"_skip_window_change_detection": true})
        ));
        for accepted in [
            serde_json::json!({}),
            serde_json::json!({"detect_window_change": true}),
            serde_json::json!({"detect_window_change": "false"}),
            serde_json::json!({"_skip_window_change_detection": false}),
        ] {
            assert!(!window_change_detection_declined(&accepted), "{accepted}");
        }
    }

    #[tokio::test]
    async fn embedded_interactive_input_can_finish_without_polling() {
        let snapshot = crate::window_change_detector::WindowChangeDetector::snapshot(None);
        let changes = finish_window_observation(
            snapshot,
            &serde_json::json!({"_skip_window_change_detection": true}),
        )
        .await;
        assert!(!changes.needs_restore());
    }
}

/// px-focus for the keyboard family (type_text / press_key / hotkey): focus the
/// element at (x,y) before a keystroke — the *element px action* form of a
/// keyboard tool. Prefer non-destructive AX focus so an existing selection is
/// retained; the foreground rung falls back to a real pixel click when needed.
/// Reuses ClickTool's exact coordinate translation and delivery mode.
/// `Ok(())` on success; `Err(ToolResult)` short-circuits the caller.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn focus_by_pixel(
    state: &Arc<ToolState>,
    pid: i32,
    window_id: Option<u32>,
    x: f64,
    y: f64,
    foreground: bool,
    session: Option<String>,
    session_id: Option<String>,
    from_zoom: bool,
    mutation_lease: Option<&BackgroundMutationLease>,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    use cua_driver_core::tool::Tool;
    let mut click_args = serde_json::json!({
        "pid": pid, "x": x, "y": y,
        "delivery_mode": "background",
        "action": "focus",
    });
    if let Some(wid) = window_id {
        click_args["window_id"] = serde_json::json!(wid);
        if let Some(lease) = mutation_lease {
            lease
                .gate_again(
                    wid,
                    None,
                    cua_driver_core::background_input::BackgroundAction::WindowPointer,
                )
                .await?;
        }
    }
    if let Some(ref s) = session {
        click_args["session"] = serde_json::json!(s);
    }
    if let Some(ref s) = session_id {
        click_args["_session_id"] = serde_json::json!(s);
    }
    if from_zoom {
        click_args["from_zoom"] = serde_json::json!(true);
    }
    let click_tool = click::ClickTool::new(state.clone());
    let click = click_tool.invoke(click_args);
    let focus = if let Some(lease) = mutation_lease {
        crate::background_mutation::with_held_lease(lease.pid, click).await
    } else {
        click.await
    };
    if focus.is_error != Some(true) {
        // AXFocused is non-destructive: unlike a second real click, it keeps a
        // Cmd+A selection intact before a follow-up type_text or Cmd+V.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // A background focus-click is `effect:"unverifiable"` by construction:
        // it cannot prove the renderer moved its first responder. Returning on
        // "it didn't error" therefore skipped the real-click fallback below
        // whenever the click was a silent no-op — advancing on transport
        // success alone, which is exactly what the ladder forbids. Confirm the
        // focus actually moved before claiming this rung worked.
        if !foreground || pixel_focus_landed(pid, window_id, x, y).await {
            return Ok(());
        }
    } else if !foreground {
        return Err(cua_driver_core::protocol::ToolResult::error(format!(
            "focus pixel-click at ({x:.0},{y:.0}) failed."
        )));
    }

    // Some renderer surfaces do not expose a usable AX focus action. The
    // explicit foreground rung retains its real-click fallback for them.
    let mut click_args = serde_json::json!({
        "pid": pid, "x": x, "y": y,
        "delivery_mode": "foreground",
        "action": "press",
    });
    if let Some(wid) = window_id {
        click_args["window_id"] = serde_json::json!(wid);
    }
    if let Some(ref s) = session {
        click_args["session"] = serde_json::json!(s);
    }
    if let Some(ref s) = session_id {
        click_args["_session_id"] = serde_json::json!(s);
    }
    if from_zoom {
        click_args["from_zoom"] = serde_json::json!(true);
    }
    let focus = click::ClickTool::new(state.clone())
        .invoke(click_args)
        .await;
    if focus.is_error == Some(true) {
        return Err(cua_driver_core::protocol::ToolResult::error(format!(
            "focus pixel-click at ({x:.0},{y:.0}) failed."
        )));
    }
    // Brief settle so the renderer registers focus before the keystrokes.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    Ok(())
}

/// Confirm that a focus pixel-click actually moved the application's focused
/// element onto the clicked point.
///
/// The background focus-click reports `effect:"unverifiable"`, so this is the
/// read-back that lets [`focus_by_pixel`] decide whether the cheap rung worked
/// or the real-click fallback is still required. It reuses the same
/// window-local-pixels → screen translation the click itself used, so the
/// comparison is in one coordinate space.
///
/// Returns `false` whenever the answer cannot be established (no window id,
/// untranslatable frame, unreadable focused element or rect). That is the
/// conservative direction: an unprovable focus escalates to the stronger rung
/// rather than being reported as success.
async fn pixel_focus_landed(pid: i32, window_id: Option<u32>, x: f64, y: f64) -> bool {
    let Some(wid) = window_id else {
        return false;
    };
    tokio::task::spawn_blocking(move || {
        let Ok(frame) = px_frame::resolve_window_px_frame(wid) else {
            return false;
        };
        let (screen_x, screen_y, _, _) = frame.to_screen(x, y);
        unsafe {
            let Some(focused) = crate::ax::bindings::focused_element_of_pid(pid) else {
                return false;
            };
            let rect = crate::ax::bindings::element_screen_rect(focused);
            core_foundation::base::CFRelease(focused as core_foundation::base::CFTypeRef);
            let Some(rect) = rect else {
                return false;
            };
            point_within_rect(rect, screen_x, screen_y)
        }
    })
    .await
    .unwrap_or(false)
}

/// Whether `[x, y, width, height]` (screen coordinates, top-left origin)
/// contains the point. A degenerate rect never contains anything, so an app
/// reporting a zero-sized focused element escalates rather than false-confirms.
fn point_within_rect([rx, ry, rw, rh]: [f64; 4], x: f64, y: f64) -> bool {
    rw > 0.0 && rh > 0.0 && x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

/// Thread-safe per-pid zoom context registry.
pub struct ZoomRegistry {
    inner: std::sync::Mutex<HashMap<i32, ZoomContext>>,
}

impl Default for ZoomRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ZoomRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn set(&self, pid: i32, ctx: ZoomContext) {
        self.inner.lock().unwrap().insert(pid, ctx);
    }

    pub fn get(&self, pid: i32) -> Option<ZoomContext> {
        self.inner.lock().unwrap().get(&pid).copied()
    }
}

/// Tracks the per-(pid, window_id) ratio applied by `max_image_dimension`
/// downscaling.
///
/// `ratio = original_dim / resized_dim` — multiply resized image coordinates
/// by this to recover original (native) window-local pixel coordinates.
/// Mirrors Swift's `ImageResizeRegistry`.
///
/// Keyed per window, matching the element cache and the element-token
/// registry. A pid-only key leaked the ratio recorded while snapshotting
/// window A into pixel clicks aimed at window B of the same pid, sending them
/// off-target (issue #2237).
pub struct ResizeRegistry {
    inner: std::sync::Mutex<HashMap<(i32, u32), f64>>,
}

impl Default for ResizeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ResizeRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Record that (pid, window_id)'s screenshot was downscaled by `ratio`.
    pub fn set_ratio(&self, pid: i32, window_id: u32, ratio: f64) {
        self.inner.lock().unwrap().insert((pid, window_id), ratio);
    }

    /// Remove the ratio entry for one window (no active downscale).
    pub fn clear_ratio(&self, pid: i32, window_id: u32) {
        self.inner.lock().unwrap().remove(&(pid, window_id));
    }

    /// The ratio for a window, or `None` if no downscale happened.
    ///
    /// `window_id: None` is the screen-scope (legacy) path: it returns a ratio
    /// only when every window recorded for `pid` agrees on one, so a
    /// window-less caller can never inherit some other window's scale. That
    /// preserves today's behaviour for the single-window case without guessing
    /// across windows.
    pub fn ratio(&self, pid: i32, window_id: Option<u32>) -> Option<f64> {
        let inner = self.inner.lock().unwrap();
        match window_id {
            Some(wid) => inner.get(&(pid, wid)).copied(),
            None => {
                let mut agreed: Option<f64> = None;
                for (_, ratio) in inner.iter().filter(|((p, _), _)| *p == pid) {
                    match agreed {
                        None => agreed = Some(*ratio),
                        Some(seen) if (seen - *ratio).abs() < 1e-9 => {}
                        Some(_) => return None,
                    }
                }
                agreed
            }
        }
    }
}

/// Runtime-mutable driver configuration persisted across calls within a session.
pub struct DriverConfig {
    /// Max screenshot dimension (0 = no limit). Applied during screenshot/zoom.
    /// Default 1568 matches Swift's `CuaDriverConfig.defaultMaxImageDimension` —
    /// the long edge is downscaled to this before encoding.
    pub max_image_dimension: u32,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            max_image_dimension: 1568,
        }
    }
}

/// Path to the persistent JSON config file shared by the CLI and MCP session.
pub fn config_file_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(format!("{home}/.cua-driver/config.json"))
}

/// Load `DriverConfig` from `~/.cua-driver/config.json`, falling back to
/// defaults for any missing or unrecognised keys.  Called at MCP startup so
/// that `cua-driver config set capture_mode vision` (CLI) carries over into
/// the next MCP session without requiring a per-call `set_config`.
pub fn load_driver_config() -> DriverConfig {
    let mut cfg = DriverConfig::default();
    let path = config_file_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return cfg, // no file yet — use defaults
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return cfg, // malformed file — use defaults
    };
    // `capture_mode` is per-call now; old on-disk `capture_mode` and
    // `capture_scope` keys are intentionally inert.
    if let Some(v) = json.get("max_image_dimension").and_then(|v| v.as_u64()) {
        if let Ok(v32) = u32::try_from(v) {
            cfg.max_image_dimension = v32;
        }
    }
    cfg
}

/// Convert a desktop action's points with one verified primary-display frame.
/// The observation validates its PNG against this same native mode geometry;
/// no hidden capture or guessed 1x fallback is needed during input delivery.
pub async fn desktop_screenshot_points<const N: usize>(
    points: [(f64, f64); N],
) -> Result<[(f64, f64); N], cua_driver_core::protocol::ToolResult> {
    use cua_driver_core::protocol::ToolResult;
    let screen = tokio::task::spawn_blocking(get_screen_size::main_screen_geometry)
        .await
        .map_err(|error| ToolResult::error(format!("Desktop geometry task failed: {error}")))?
        .ok_or_else(|| ToolResult::error("Primary display identity or geometry unavailable; observe again before desktop input."))?;
    let mut logical = points;
    for point in &mut logical {
        *point = screen.logical_point(point.0, point.1).ok_or_else(|| {
            ToolResult::error(
                "Desktop coordinates must be finite and inside the current primary display PNG.",
            )
        })?;
    }
    Ok(logical)
}

pub async fn desktop_screenshot_point(
    x: f64,
    y: f64,
) -> Result<(f64, f64), cua_driver_core::protocol::ToolResult> {
    Ok(desktop_screenshot_points([(x, y)]).await?[0])
}

/// Persist a single key/value pair to `~/.cua-driver/config.json`.
/// Merges with any existing file contents so other keys are preserved.
/// Returns `Err` if the directory cannot be created or the file cannot be written.
pub fn write_driver_config_key(key: &str, value: &serde_json::Value) -> Result<(), String> {
    let path = config_file_path();
    let mut json: serde_json::Value = path
        .exists()
        .then(|| std::fs::read_to_string(&path).ok())
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    json[key] = value.clone();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(())
}

/// Per-session config overrides layered over the global persisted `DriverConfig`.
///
/// The cua-driver daemon is one shared process: every `cua-driver mcp` proxy
/// connects to it and shares its `ToolState`. `DriverConfig` is therefore
/// multi-tenant AND persisted to disk — so without session scoping, session A's
/// `set_config capture_mode=vision` clobbers session B's value and flips the
/// on-disk default under everyone. These overrides fix that: a named MCP session
/// gets an in-memory, non-persisted override keyed by its `_session_id`; the
/// anonymous session (CLI / one-shot `call`) still writes the shared global +
/// disk. `None` fields mean "fall through to the global layer".
#[derive(Clone, Default)]
pub struct ConfigOverrides {
    pub max_image_dimension: Option<u32>,
}

/// Thread-safe map of `session_id` → `ConfigOverrides`, mirroring
/// `CursorRegistry`'s registry shape. Cleared per session on `session_end`.
pub struct SessionConfigRegistry {
    inner: std::sync::Mutex<HashMap<String, ConfigOverrides>>,
}

impl SessionConfigRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Merge `delta` into `session`'s overrides (only the `Some` fields of
    /// `delta` overwrite; existing overrides for unset fields are preserved).
    pub fn set(&self, session: &str, delta: ConfigOverrides) {
        // Write-boundary resurrection guard: keyed by session_id, so an
        // in-flight set_config that lands AFTER session_end (passed the dispatch
        // gate, then the proxy died and the reaper cleared this session's
        // overrides) must NOT re-create the entry — it would be invisible and
        // never reaped again. `fire_session_end` marks ENDED_SESSIONS *before*
        // running the config-clear hook, so this check is authoritative.
        if cua_driver_core::session::is_session_ended(session) {
            return;
        }
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(session.to_owned()).or_default();
        if delta.max_image_dimension.is_some() {
            entry.max_image_dimension = delta.max_image_dimension;
        }
    }

    /// Resolve the effective `max_image_dimension` for `session`, layering its
    /// override over the global `DriverConfig`. `session = None` (anonymous)
    /// returns the global value verbatim.
    pub fn effective_max_image_dimension(
        &self,
        session: Option<&str>,
        global: &DriverConfig,
    ) -> u32 {
        let ov = session.and_then(|s| self.inner.lock().unwrap().get(s).cloned());
        match ov {
            Some(ov) => ov.max_image_dimension.unwrap_or(global.max_image_dimension),
            None => global.max_image_dimension,
        }
    }

    /// Drop `session`'s overrides. No-op for an unknown id (so `session_end`
    /// for an anonymous / never-set session is harmless).
    pub fn clear(&self, session: &str) {
        self.inner.lock().unwrap().remove(session);
    }
}

impl Default for SessionConfigRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared state passed to all tools.
pub struct ToolState {
    pub(crate) rendering_leases: Arc<crate::capture_lease::WindowRenderingLeases>,
    pub element_cache: Arc<ElementCache>,
    pub cursor_registry: Arc<CursorRegistry>,
    pub zoom_registry: Arc<ZoomRegistry>,
    pub resize_registry: Arc<ResizeRegistry>,
    /// Global, disk-persisted config — the base layer and the only one the
    /// anonymous session / CLI writes.
    pub config: Arc<std::sync::RwLock<DriverConfig>>,
    /// Per-MCP-session in-memory config overrides layered over `config`.
    pub session_config: Arc<SessionConfigRegistry>,
    /// Open CDP connections, one per port, reused across `insert_text` /
    /// `type_keystrokes` calls instead of reconnecting fresh every time —
    /// see `CdpSessionCache` for why (Chrome's "allow remote debugging"
    /// popup fires on every new connection, not once per session).
    pub cdp_sessions: Arc<crate::browser::CdpSessionCache>,
    /// Whether the runtime owner installed the AppKit main-thread cursor
    /// overlay facility. Imported SDK runtimes deliberately leave this false;
    /// explicit cursor-overlay methods must refuse instead of reporting a
    /// successful no-op.
    pub cursor_overlay_available: bool,
    /// Direct and embedded hosts own TCC request UX. Their runtime may inspect
    /// permission state but must not raise Cua-owned prompts.
    pub host_owns_permission_ux: bool,
    /// Advisory host identity for permission diagnostics only.
    pub host_bundle_id: Option<String>,
}

impl Default for ToolState {
    fn default() -> Self {
        Self::new(false, false, None)
    }
}

impl ToolState {
    fn new(
        cursor_overlay_available: bool,
        host_owns_permission_ux: bool,
        host_bundle_id: Option<String>,
    ) -> Self {
        Self {
            element_cache: Arc::new(ElementCache::new()),
            rendering_leases: Arc::new(crate::capture_lease::WindowRenderingLeases::default()),
            cursor_registry: Arc::new(CursorRegistry::new()),
            zoom_registry: Arc::new(ZoomRegistry::new()),
            resize_registry: Arc::new(ResizeRegistry::new()),
            // Load persisted config from ~/.cua-driver/config.json so that
            // `cua-driver config set` changes carry over into MCP sessions.
            config: Arc::new(std::sync::RwLock::new(load_driver_config())),
            session_config: Arc::new(SessionConfigRegistry::new()),
            cdp_sessions: Arc::new(crate::browser::CdpSessionCache::new()),
            cursor_overlay_available,
            host_owns_permission_ux,
            host_bundle_id,
        }
    }
}

pub(crate) fn cursor_overlay_unavailable() -> cua_driver_core::protocol::ToolResult {
    let message = "macOS agent cursor overlay is unavailable: this runtime owner has no certified \
                   AppKit main-thread host adapter or no Window Server graphic-session access; \
                   use a GUI private worker or standalone service for cursor-overlay controls";
    cua_driver_core::protocol::ToolResult::error(message).with_structured(serde_json::json!({
        "status": "refused",
        "refusal": {
            "code": "facility_unavailable",
            "facility": "macos_cursor_overlay",
            "message": message,
        }
    }))
}

/// Register all macOS tools into the registry. `compat=true` swaps the
/// regular `screenshot` tool for the Claude Code computer-use compat
/// variant — same name, stricter args, window-scoped JPEG @ 85% + a text
/// note telling the caller to use pixel-addressed tools.
pub fn register_all(
    registry: &mut ToolRegistry,
    compat: bool,
    cursor_overlay_available: bool,
    host_owns_permission_ux: bool,
    host_bundle_id: Option<String>,
) {
    let state = Arc::new(ToolState::new(
        cursor_overlay_available,
        host_owns_permission_ux,
        host_bundle_id,
    ));
    {
        let leases = state.rendering_leases.clone();
        registry.retain_session_end_hook(
            cua_driver_core::session::register_scoped_fallible_session_end_hook(
                "macos_window_rendering_lease",
                move |session| leases.end(session),
            ),
        );
        let leases = state.rendering_leases.clone();
        registry.retain_fallible_runtime_cleanup("macos_window_rendering_leases", move || {
            leases.close()
        });
    }
    let cursor_outcome_reader = {
        let cursor_registry = state.cursor_registry.clone();
        cua_driver_core::session::register_scoped_cursor_outcome_reader(std::sync::Arc::new(
            move |session_id| {
                let state = cursor_registry.get(session_id);
                let motion_customized = state.is_some()
                    && crate::cursor::overlay::current_motion(session_id)
                        != cursor_overlay::MotionConfig::default();
                let active_cursor_count = cursor_registry
                    .all_states()
                    .iter()
                    .filter(|state| state.config.cursor_id != "default")
                    .count()
                    .max(1);
                match state {
                    Some(state) => cua_driver_core::session::bounded_cursor_outcome(
                        true,
                        state.config.enabled,
                        crate::cursor::overlay::is_visible_for_session(session_id),
                        Some(state.config.theme_id.as_str()),
                        motion_customized,
                        active_cursor_count,
                    ),
                    None => cua_driver_core::session::bounded_cursor_outcome(
                        false,
                        false,
                        false,
                        None,
                        false,
                        active_cursor_count,
                    ),
                }
            },
        ))
    };
    registry.retain_cursor_outcome_reader(cursor_outcome_reader);
    if let Some(runtime_scope) = cua_driver_core::tool::current_dispatch_runtime_scope() {
        let prefix = format!("__cua_runtime_{runtime_scope}:");
        let cursor_registry = state.cursor_registry.clone();
        registry.retain_runtime_cleanup(move || {
            for cursor in cursor_registry
                .all_states()
                .into_iter()
                .filter(|cursor| cursor.config.cursor_id.starts_with(&prefix))
            {
                cursor_registry.remove(&cursor.config.cursor_id);
                crate::cursor::overlay::remove_cursor(cursor.config.cursor_id);
            }
        });
    }
    // Share the element cache with the recording-hook layer so it can
    // resolve element_index → window-local screenshot coords for click.png.
    crate::recording_hooks::set_element_cache(state.element_cache.clone());

    // Drop a disconnecting session's config overrides + owned cursor on
    // `session_end`. The daemon fans the session id out to this hook;
    // recording ownership is handled separately on the core RecordingSession.
    {
        let session_config = state.session_config.clone();
        let cursor_registry = state.cursor_registry.clone();
        let registration =
            cua_driver_core::session::register_scoped_session_end_hook(move |session_id| {
                session_config.clear(session_id);
                // Per-session agent cursor: the session_id is the cursor key when
                // the caller gave no explicit cursor_id, so dropping it here both
                // prunes the metadata registry and stops the overlay painting that
                // session's cursor. Both paths guard "default" so the anonymous /
                // one-shot cursor survives. Anonymous sessions that never created a
                // cursor are a harmless no-op.
                cursor_registry.remove(session_id);
                crate::cursor::overlay::remove_cursor(session_id.to_owned());
            });
        registry.retain_session_end_hook(registration);
        let revive_registration =
            cua_driver_core::session::register_scoped_session_revive_hook(move |session_id| {
                crate::cursor::overlay::revive_cursor(session_id.to_owned());
            });
        registry.retain_session_revive_hook(revive_registration);
    }

    registry.register(Box::new(list_apps::ListAppsTool));
    registry.register(Box::new(list_windows::ListWindowsTool));
    registry.register(Box::new(get_window_state::GetWindowStateTool::new(
        state.clone(),
    )));
    registry.register(Box::new(
        cua_driver_core::expectation::VerifyStateTool::new(Arc::new(
            cua_driver_core::expectation::ToolObservationProvider::new(
                Arc::new(list_windows::ListWindowsTool),
                Arc::new(get_window_state::GetWindowStateTool::new(state.clone())),
            ),
        )),
    ));
    registry.register(Box::new(launch_app::LaunchAppTool));
    registry.register(Box::new(kill_app::KillAppTool));
    let pid_window_candidates: WindowTargetCandidates = Arc::new(pid_window_target_candidates);
    registry.register(pid_window_guarded(
        bring_to_front::BringToFrontTool,
        &pid_window_candidates,
    ));
    registry.register(Box::new(set_window_frame::SetWindowFrameTool));
    registry.register(Box::new(invoke_menu::InvokeMenuTool));
    registry.register(pid_window_guarded(
        click::ClickTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        double_click::DoubleClickTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        right_click::RightClickTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        drag::DragTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        type_text::TypeTextTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        press_key::PressKeyTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        hotkey::HotkeyTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        set_value::SetValueTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        scroll::ScrollTool::new(state.clone()),
        &pid_window_candidates,
    ));
    cua_driver_core::clipboard::register_clipboard_tools(
        registry,
        Arc::new(clipboard::MacosClipboard::new()),
    );
    // The standalone `screenshot` tool was removed (#1692). The pixel-grounding
    // screenshot the Claude Code computer-use compat loop relies on now comes
    // from `get_window_state` (which always returns BOTH the tree AND a
    // screenshot — perception is mode-agnostic; `capture_mode` is deprecated/
    // ignored) for a window, or `get_desktop_state` for the whole screen.
    // `compat` no longer gates a tool swap here — the flag's live purpose is to
    // register the MCP server under the `cua-computer-use` name, which is what
    // triggers Claude Code's computer-use beta-tool injection (see cli.rs).
    let _ = compat;
    registry.register(Box::new(get_screen_size::GetScreenSizeTool));
    registry.register(Box::new(get_desktop_state::GetDesktopStateTool));
    registry.register(Box::new(get_cursor_position::GetCursorPositionTool));
    registry.register(Box::new(move_cursor::MoveCursorTool::new(state.clone())));
    registry.register(Box::new(cursor_tools::SetAgentCursorEnabledTool::new(
        state.clone(),
    )));
    registry.register(Box::new(cursor_tools::SetAgentCursorMotionTool::new(
        state.clone(),
    )));
    registry.register(Box::new(cursor_tools::SetAgentCursorThemeTool::new(
        state.clone(),
    )));
    registry.register(Box::new(cursor_tools::GetAgentCursorStateTool::new(
        state.clone(),
    )));
    registry.register(Box::new(check_permissions::CheckPermissionsTool::new(
        state.clone(),
    )));
    // `health_report` — single-call end-to-end diagnostics. Stable
    // schema_version="1" contract aimed at downstream consumers who must
    // not have to know cua-driver internals. Provider is platform-specific; tool plumbing is in
    // `cua_driver_core::health_report`.
    registry.register(Box::new(
        cua_driver_core::health_report::HealthReportTool::new(Arc::new(
            health_report::MacosHealthProvider,
        )),
    ));
    registry.register(Box::new(get_config::GetConfigTool::new(state.clone())));
    registry.register(Box::new(set_config::SetConfigTool::new(state.clone())));
    registry.register(Box::new(
        get_accessibility_tree::GetAccessibilityTreeTool::new(state.clone()),
    ));
    registry.register(Box::new(zoom::ZoomTool {
        state: state.clone(),
    }));
    // `type_text_chars` is intentionally NOT registered — Swift treats it as
    // a deprecated alias for `type_text` resolved at invoke time in
    // mcp-server's `ToolRegistry::invoke`. Keeping it out of the registry
    // means it doesn't show up in `tools/list` either, matching Swift's
    // ToolRegistry.swift (`type_text_chars` not in `handlers`) and the
    // platform-windows::build_registry which uses the same convention.
    // Touch the struct so it stays in this crate for the alias resolver.
    let _: &type_text_chars::TypeTextCharsTool =
        &type_text_chars::TypeTextCharsTool::new(state.clone());
    // Cross-platform `page` tool definition lives in mcp-server; macOS plugs in
    // its Apple-Events / CDP / AX-tree backend here.
    registry.register(Box::new(cua_driver_core::page::PageTool::new(Arc::new(
        page::MacOsPageBackend::new(state.clone()),
    ))));
    let browser_engine = cua_driver_core::browser::BrowserEngine::new_with_runtime_services(
        Arc::new(crate::browser::MacOsBrowserPlatform::new(
            state.cursor_registry.clone(),
        )),
        registry.approval_broker(),
        registry.protected_resource_ownership(),
    );
    cua_driver_core::browser::register_browser_tools(&browser_engine, registry);
    // Recording / replay + session-lifecycle tools are platform-independent.
    registry.register_recording_tools();
    registry.register_session_tools();
}

#[cfg(test)]
mod session_config_guard_tests {
    use super::*;
    use cua_driver_core::session::fire_session_end;

    fn overrides(max_dim: u32) -> ConfigOverrides {
        ConfigOverrides {
            max_image_dimension: Some(max_dim),
        }
    }

    #[test]
    fn ended_session_config_set_is_noop() {
        // THE FIX (config side): an ended session id keys the overrides map, so
        // an in-flight set_config after session_end must not re-create the entry
        // the reaper's clear hook removed. effective then falls back to global.
        let reg = SessionConfigRegistry::new();
        let global = DriverConfig::default();
        let sid = "wb-config-ended-Q9R8S7";
        fire_session_end(sid);
        assert!(cua_driver_core::session::is_session_ended(sid));

        reg.set(sid, overrides(800));
        let dim = reg.effective_max_image_dimension(Some(sid), &global);
        assert_eq!(
            dim, global.max_image_dimension,
            "ended session must not get an override entry"
        );
    }

    #[test]
    fn live_session_config_set_takes_effect() {
        let reg = SessionConfigRegistry::new();
        let global = DriverConfig::default();
        let sid = "wb-config-live-T1U2V3";
        assert!(!cua_driver_core::session::is_session_ended(sid));
        reg.set(sid, overrides(800));
        let dim = reg.effective_max_image_dimension(Some(sid), &global);
        assert_eq!(dim, 800, "live session override must apply");
    }
}

#[cfg(test)]
mod resize_registry_tests {
    use super::ResizeRegistry;

    /// Issue #2237: the registry was keyed by pid alone, so the downscale
    /// ratio recorded while snapshotting one window was applied to pixel
    /// clicks aimed at another window of the same app.
    #[test]
    fn resize_ratio_is_keyed_per_window() {
        let reg = ResizeRegistry::new();
        reg.set_ratio(800, 11, 2.0);
        reg.set_ratio(800, 22, 1.25);
        assert_eq!(reg.ratio(800, Some(11)), Some(2.0));
        assert_eq!(reg.ratio(800, Some(22)), Some(1.25));
    }

    #[test]
    fn undownscaled_window_reports_no_ratio() {
        let reg = ResizeRegistry::new();
        reg.set_ratio(800, 11, 2.0);
        assert_eq!(
            reg.ratio(800, Some(22)),
            None,
            "window 22 was never downscaled; it must not inherit window 11's ratio"
        );
    }

    #[test]
    fn clearing_one_window_keeps_the_other() {
        let reg = ResizeRegistry::new();
        reg.set_ratio(800, 11, 2.0);
        reg.set_ratio(800, 22, 1.25);
        reg.clear_ratio(800, 11);
        assert_eq!(reg.ratio(800, Some(11)), None);
        assert_eq!(reg.ratio(800, Some(22)), Some(1.25));
    }

    #[test]
    fn distinct_pids_with_the_same_window_id_do_not_collide() {
        let reg = ResizeRegistry::new();
        reg.set_ratio(800, 11, 2.0);
        reg.set_ratio(900, 11, 3.0);
        assert_eq!(reg.ratio(800, Some(11)), Some(2.0));
        assert_eq!(reg.ratio(900, Some(11)), Some(3.0));
    }

    /// Screen-scope callers pass no window_id. One window (the common case)
    /// keeps working; disagreeing windows refuse to guess.
    #[test]
    fn screen_scope_lookup_only_answers_when_windows_agree() {
        let reg = ResizeRegistry::new();
        assert_eq!(reg.ratio(800, None), None, "nothing recorded yet");
        reg.set_ratio(800, 11, 2.0);
        assert_eq!(
            reg.ratio(800, None),
            Some(2.0),
            "single window is unambiguous"
        );
        reg.set_ratio(800, 22, 2.0);
        assert_eq!(reg.ratio(800, None), Some(2.0), "agreeing windows answer");
        reg.set_ratio(800, 33, 1.25);
        assert_eq!(
            reg.ratio(800, None),
            None,
            "disagreeing windows must not pick one arbitrarily"
        );
    }
}

#[cfg(test)]
mod cursor_overlay_facility_tests {
    use super::*;
    use cua_driver_core::tool::Tool;

    fn assert_facility_unavailable(result: cua_driver_core::protocol::ToolResult) {
        assert_eq!(result.is_error, Some(true));
        let refusal = result
            .structured_content
            .and_then(|value| value.get("refusal").cloned())
            .expect("structured refusal");
        assert_eq!(refusal["code"], "facility_unavailable");
        assert_eq!(refusal["facility"], "macos_cursor_overlay");
    }

    #[tokio::test]
    async fn cursor_control_refuses_without_main_thread_host_facility() {
        let state = Arc::new(ToolState::new(false, true, None));
        let result = cursor_tools::SetAgentCursorEnabledTool::new(state)
            .invoke(serde_json::json!({"enabled": true, "session": "test"}))
            .await;
        assert_facility_unavailable(result);
    }

    #[tokio::test]
    async fn window_cursor_move_refuses_without_main_thread_host_facility() {
        let state = Arc::new(ToolState::new(false, true, None));
        let result = move_cursor::MoveCursorTool::new(state)
            .invoke(serde_json::json!({"x": 10, "y": 20, "session": "test"}))
            .await;
        assert_facility_unavailable(result);
    }
}

// RecordingSession lives in cua-driver-core, but its `start()` pulls in the
// macOS cursor sampler (CoreGraphics), so the start-guard test runs here in
// platform-macos where build.rs links the frameworks — the core crate's test
// binary has no CoreGraphics linkage.
#[cfg(test)]
mod pixel_focus_readback_tests {
    use super::point_within_rect;

    #[test]
    fn confirms_a_point_inside_the_focused_element() {
        let composer = [100.0, 800.0, 250.0, 24.0];
        assert!(point_within_rect(composer, 220.0, 812.0));
        assert!(
            point_within_rect(composer, 100.0, 800.0),
            "top-left is inside"
        );
    }

    #[test]
    fn rejects_a_point_outside_the_focused_element() {
        // The WhatsApp case: the click targeted the composer but focus stayed on
        // the transcript above it, so the clicked point is not inside the
        // focused element's rect and the caller must escalate.
        let transcript = [100.0, 100.0, 640.0, 690.0];
        assert!(!point_within_rect(transcript, 220.0, 812.0));
    }

    #[test]
    fn excludes_the_far_edges() {
        let rect = [0.0, 0.0, 10.0, 10.0];
        assert!(!point_within_rect(rect, 10.0, 5.0));
        assert!(!point_within_rect(rect, 5.0, 10.0));
    }

    #[test]
    fn a_degenerate_rect_never_confirms() {
        assert!(!point_within_rect([5.0, 5.0, 0.0, 0.0], 5.0, 5.0));
        assert!(!point_within_rect([5.0, 5.0, -3.0, 10.0], 5.0, 6.0));
    }
}

#[cfg(test)]
mod recording_start_guard_tests {
    use cua_driver_core::recording::RecordingSession;
    use cua_driver_core::session::fire_session_end;

    #[test]
    fn start_refuses_for_ended_session_owner() {
        // THE FIX (recording side): an in-flight start_recording owned by a
        // session that already ended would leak an ffmpeg/SCStream process owned
        // by a dead session that is never reaped. start() must refuse.
        let rec = RecordingSession::new();
        let sid = "wb-recording-ended-W4X5Y6";
        fire_session_end(sid);
        assert!(cua_driver_core::session::is_session_ended(sid));

        let dir = std::env::temp_dir().join("wb-rec-ended");
        let err = rec.start(dir.to_str().unwrap(), false, Some(sid));
        assert!(err.is_err(), "start for an ended session owner must error");
        assert!(
            !rec.current_state().enabled,
            "no recording may start for a dead session"
        );
    }

    #[test]
    fn start_succeeds_for_live_session_owner() {
        let rec = RecordingSession::new();
        let sid = "wb-recording-live-Z7A8B9";
        assert!(!cua_driver_core::session::is_session_ended(sid));
        let dir = std::env::temp_dir().join("wb-rec-live");
        // record_video=false avoids spawning ffmpeg in the test.
        let ok = rec.start(dir.to_str().unwrap(), false, Some(sid));
        assert!(ok.is_ok(), "start for a live session owner must succeed");
        assert!(rec.current_state().enabled);
        let _ = rec.stop_owner(Some(sid));
    }

    #[test]
    fn start_succeeds_for_anonymous_owner() {
        // owner = None (CLI one-shot / legacy shim) is never gated.
        let rec = RecordingSession::new();
        let dir = std::env::temp_dir().join("wb-rec-anon");
        let ok = rec.start(dir.to_str().unwrap(), false, None);
        assert!(ok.is_ok(), "anonymous start must never be gated");
        assert!(rec.current_state().enabled);
        let _ = rec.stop_owner(None);
    }
}
