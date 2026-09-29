//! Foreground pointer envelope: raise one exact window, prove it is the window
//! under a screen point, run pointer input there, then put everything back —
//! unless the user took the pointer or the foreground over meanwhile.
//!
//! A view that scrolls whatever sits under the real pointer (iPhone
//! Mirroring's phone screen is the measured case, but any such view) receives
//! a wheel only when the real cursor is over it and its window is the topmost
//! window at that point. `AXFrontmost` on the application element was measured
//! to front and raise in 13-21 ms, where the SkyLight front-process SPI
//! activates without raising and `AXRaise` alone does nothing. So this
//! envelope activates with `AXFrontmost`, waits for WindowServer to front the
//! process, and refuses — before any input — when another ordinary (layer-0)
//! window still covers the point. Windows above layer 0 are not treated as
//! covering: click-through status overlays live there, and window metadata
//! cannot tell which of them intercept input. It arms no focus suppression:
//! the activation is the point of the envelope, and a suppressor would undo
//! it, or undo the user's own switch.
//!
//! The body calls [`Envelope::takeover`] between input events and
//! [`Envelope::check`] before each burst. After a takeover — the pointer left
//! where it was parked, or another process came to the front — input stops and
//! nothing is restored: the user owns both now.

use std::sync::atomic::{AtomicBool, Ordering};

use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::operation;

use crate::ax::bindings::{
    ax_get_window_id, copy_ax_windows, kAXErrorSuccess, perform_action, set_bool_attr_true,
    AXUIElementCreateApplication, AXUIElementSetMessagingTimeout,
};
use crate::windows::{WindowBounds, WindowInfo};

/// How long the envelope waits for the target to be front and uncovered.
const RAISE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);
const RAISE_POLL: std::time::Duration = std::time::Duration::from_millis(10);
/// The parked cursor may drift this far (points) before it counts as moved.
const CURSOR_SLACK: f64 = 2.0;

/// The window that stayed on top at the point, or `None` when no window was
/// found there at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Covered {
    pub owner: Option<(String, i32)>,
}

#[derive(Debug)]
pub enum RaisedPointerError {
    /// The target never became the topmost window at the point; no input was
    /// sent.
    Covered(Covered),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for RaisedPointerError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

/// What the body can ask of the envelope.
pub struct Envelope {
    window_id: u32,
    point: (f64, f64),
    bounds: WindowBounds,
    own_pid: i32,
    /// The front process right after the gate passed.
    front: Option<[u8; 8]>,
    /// The target's own process, when the gate passed before it was front:
    /// its activation landing later is not a takeover.
    target_psn: Option<[u8; 8]>,
    taken_over: AtomicBool,
}

impl Envelope {
    /// The target window's frame when the envelope proved it uncovered.
    pub fn bounds(&self) -> &WindowBounds {
        &self.bounds
    }

    /// `Some(reason)` once the user moved the parked pointer or fronted
    /// another process; from then on nothing is restored. Cheap: one event
    /// read and one WindowServer call.
    pub fn takeover(&self) -> Option<String> {
        let cursor = crate::input::mouse::cursor_location().ok().map(|p| (p.x, p.y));
        let front = crate::input::skylight::front_process_serial();
        let reason = takeover(self.point, cursor, self.front, front, self.target_psn)?;
        self.taken_over.store(true, Ordering::SeqCst);
        Some(reason)
    }

    /// `Err(reason)` when input must stop now: a takeover, or the target is
    /// no longer the topmost ordinary window at the point with its bounds.
    pub fn check(&self) -> Result<(), String> {
        if let Some(reason) = self.takeover() {
            return Err(reason);
        }
        let enumeration = crate::windows::visible_windows_with_space_snapshot();
        destination_holds(
            &enumeration.windows,
            &enumeration.alphas,
            self.point,
            self.own_pid,
            self.window_id,
            &self.bounds,
        )
    }
}

/// Whether the user took over: the parked cursor moved, or the front process
/// is not the one observed right after the gate (nor `late_target`, the
/// target itself activating after a gate that passed on expiry).
fn takeover(
    parked: (f64, f64),
    cursor: Option<(f64, f64)>,
    gate_front: Option<[u8; 8]>,
    front: Option<[u8; 8]>,
    late_target: Option<[u8; 8]>,
) -> Option<String> {
    if let Some((x, y)) = cursor {
        if (x - parked.0).abs() > CURSOR_SLACK || (y - parked.1).abs() > CURSOR_SLACK {
            return Some(format!(
                "the pointer moved to ({x:.0}, {y:.0}) during the scroll; the user has it"
            ));
        }
    }
    match (gate_front, front) {
        (Some(before), Some(now)) if before != now && Some(now) != late_target => {
            Some("another application came to the front during the scroll".into())
        }
        _ => None,
    }
}

/// The target is still the topmost ordinary window at the point and has not
/// moved.
fn destination_holds(
    windows: &[WindowInfo],
    alphas: &[f64],
    point: (f64, f64),
    own_pid: i32,
    window_id: u32,
    bounds: &WindowBounds,
) -> Result<(), String> {
    let Some(top) = topmost_window_at(windows, alphas, point.0, point.1, own_pid) else {
        return Err(format!("window {window_id} is no longer on screen at the point"));
    };
    if top.window_id != window_id {
        return Err(format!(
            "window {window_id} became covered by {} (pid {}) at the point",
            top.app_name, top.pid
        ));
    }
    let b = &top.bounds;
    let moved = (b.x - bounds.x).abs() > 0.5
        || (b.y - bounds.y).abs() > 0.5
        || (b.width - bounds.width).abs() > 0.5
        || (b.height - bounds.height).abs() > 0.5;
    if moved {
        return Err(format!("window {window_id} moved or resized during the scroll"));
    }
    Ok(())
}

/// Raise `window_id` of `pid` so it is the topmost window at screen point
/// `(x, y)`, park the real cursor there, and run `body`. Afterwards — also on
/// error, refusal or cancellation — the cursor goes back where it was and the
/// previously frontmost application is re-fronted, unless a takeover was seen
/// at any point, including one last check right before restoring.
pub fn with_raised_pointer<T>(
    pid: i32,
    window_id: u32,
    x: f64,
    y: f64,
    body: impl FnOnce(&Envelope) -> anyhow::Result<T>,
) -> Result<T, RaisedPointerError> {
    let prior_app =
        crate::input::skylight::frontmost_app_pid().or_else(crate::apps::frontmost_pid);
    let prior_cursor = crate::input::mouse::cursor_location().ok();
    // The window the user has key in the target app, so a raise of a sibling
    // can be put back.
    let prior_key_window = (prior_app == Some(pid))
        .then(|| crate::ax::bindings::focused_window_id_of_pid(pid))
        .flatten();
    let own_pid = std::process::id() as i32;
    if prior_app != Some(pid) {
        set_app_frontmost(pid);
    }
    let mut deadline = std::time::Instant::now() + RAISE_TIMEOUT;
    let mut raised = false;
    let mut passed_front = true;
    let bounds = loop {
        let enumeration = crate::windows::visible_windows_with_space_snapshot();
        let top = topmost_window_at(&enumeration.windows, &enumeration.alphas, x, y, own_pid);
        let on_top = top.filter(|window| window.window_id == window_id);
        // Activation raises the app, not this window over its own siblings:
        // raise the exact window only when a sibling covers the point. Any
        // error is ignored (iPhone Mirroring answers -25205 and still comes
        // forward) — the hit test decides.
        if !raised && top.is_some_and(|w| w.pid == pid && w.window_id != window_id) {
            raise_window(pid, window_id);
            raised = true;
            deadline = std::time::Instant::now() + RAISE_TIMEOUT;
            continue;
        }
        let front = crate::input::skylight::front_process_matches(pid, window_id) != Some(false);
        let expired = std::time::Instant::now() >= deadline;
        if let Some(window) = on_top.filter(|_| front || expired) {
            passed_front = front;
            break window.bounds.clone();
        }
        if expired {
            restore(pid, window_id, prior_app, None, raised.then_some(prior_key_window).flatten());
            return Err(RaisedPointerError::Covered(Covered {
                owner: top.map(|window| (window.app_name.clone(), window.pid)),
            }));
        }
        if let Err(error) = operation::sleep(RAISE_POLL) {
            restore(pid, window_id, prior_app, None, raised.then_some(prior_key_window).flatten());
            return Err(RaisedPointerError::Failed(error.into()));
        }
    };
    crate::input::mouse::move_cursor_desktop(x, y)?;
    // A gate that passed on expiry, with the target on top but not yet the
    // front process, may see the target's own activation land later; that is
    // not the user taking over.
    let target_psn = (!passed_front)
        .then(|| {
            let mut psn = [0u8; 8];
            crate::input::skylight::get_process_psn_for_window(window_id, pid, &mut psn)
                .then_some(psn)
        })
        .flatten();
    let envelope = Envelope {
        window_id,
        point: (x, y),
        bounds,
        own_pid,
        front: crate::input::skylight::front_process_serial(),
        target_psn,
        taken_over: AtomicBool::new(false),
    };
    let _restore = operation::ReleaseOnDrop::new(|| {
        if !envelope.taken_over.load(Ordering::SeqCst) && envelope.takeover().is_none() {
            restore(
                pid,
                window_id,
                prior_app,
                prior_cursor.map(|p| (p.x, p.y)),
                raised.then_some(prior_key_window).flatten(),
            );
        }
    });
    Ok(body(&envelope)?)
}

/// Put the pointer, the key window of the target app (when a sibling was
/// raised over it) and the prior frontmost app back.
fn restore(
    pid: i32,
    window_id: u32,
    prior_app: Option<i32>,
    cursor: Option<(f64, f64)>,
    key_window: Option<u32>,
) {
    if let Some((x, y)) = cursor {
        let _ = crate::input::mouse::move_cursor_desktop(x, y);
    }
    if let Some(key) = key_window.filter(|key| *key != window_id) {
        raise_window(pid, key);
    }
    if let Some(prior) = prior_app.filter(|prior| *prior != pid) {
        if crate::input::skylight::front_process_matches(pid, window_id) != Some(false) {
            set_app_frontmost(prior);
        }
    }
}

/// Raise one exact window of `pid` and make it main and key within its app,
/// with a 0.25 s messaging timeout on every request. Errors are ignored.
fn raise_window(pid: i32, window_id: u32) {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return;
        }
        AXUIElementSetMessagingTimeout(app, 0.25);
        let mut target = None;
        for window in copy_ax_windows(app) {
            AXUIElementSetMessagingTimeout(window, 0.25);
            if target.is_none() && ax_get_window_id(window) == Some(window_id) {
                target = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
        CFRelease(app as CFTypeRef);
        if let Some(window) = target {
            let _ = perform_action(window, "AXRaise");
            let _ = set_bool_attr_true(window, "AXMain");
            let _ = set_bool_attr_true(window, "AXFocused");
            CFRelease(window as CFTypeRef);
        }
    }
}

/// Front `pid` the way a user's click on its window would, raising its
/// windows. Returns whether the request was accepted.
fn set_app_frontmost(pid: i32) -> bool {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        AXUIElementSetMessagingTimeout(app, 0.25);
        let accepted = set_bool_attr_true(app, "AXFrontmost") == kAXErrorSuccess;
        CFRelease(app as CFTypeRef);
        accepted
    }
}

/// The frontmost visible layer-0 window containing `(x, y)`, skipping
/// transparent windows and this process's own (the agent-cursor overlay).
/// `alphas` is parallel to `windows`.
fn topmost_window_at<'a>(
    windows: &'a [WindowInfo],
    alphas: &[f64],
    x: f64,
    y: f64,
    own_pid: i32,
) -> Option<&'a WindowInfo> {
    windows
        .iter()
        .enumerate()
        .filter(|(index, window)| {
            let bounds = &window.bounds;
            window.layer == 0
                && window.pid != own_pid
                && alphas.get(*index).copied().unwrap_or(1.0) > 0.0
                && x >= bounds.x
                && x < bounds.x + bounds.width
                && y >= bounds.y
                && y < bounds.y + bounds.height
        })
        .max_by_key(|(_, window)| window.z_index)
        .map(|(_, window)| window)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(window_id: u32, pid: i32, z_index: usize, x: f64, width: f64) -> WindowInfo {
        WindowInfo {
            window_id,
            pid,
            app_name: format!("app{pid}"),
            title: String::new(),
            bounds: WindowBounds {
                x,
                y: 0.0,
                width,
                height: 500.0,
            },
            layer: 0,
            z_index,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn the_frontmost_window_containing_the_point_wins() {
        let windows = vec![window(1, 10, 1, 0.0, 400.0), window(2, 20, 2, 300.0, 400.0)];
        let alphas = vec![1.0, 1.0];
        assert_eq!(topmost_window_at(&windows, &alphas, 350.0, 10.0, 99).unwrap().window_id, 2);
        assert_eq!(topmost_window_at(&windows, &alphas, 100.0, 10.0, 99).unwrap().window_id, 1);
        assert!(topmost_window_at(&windows, &alphas, 800.0, 10.0, 99).is_none());
    }

    #[test]
    fn the_driver_overlay_and_transparent_windows_never_cover_the_target() {
        let windows = vec![
            window(1, 10, 1, 0.0, 400.0),
            window(2, 99, 3, 0.0, 2000.0),
            window(3, 30, 2, 0.0, 400.0),
        ];
        assert_eq!(
            topmost_window_at(&windows, &[1.0, 1.0, 0.0], 100.0, 10.0, 99).unwrap().window_id,
            1
        );
        assert_eq!(
            topmost_window_at(&windows, &[1.0, 1.0, 1.0], 100.0, 10.0, 99).unwrap().window_id,
            3
        );
    }

    /// Audit N1: a click-through status overlay at layer 1000 over the point
    /// refused every scroll there as covered.
    #[test]
    fn an_overlay_above_layer_zero_does_not_cover_the_target() {
        let target = window(1, 10, 1, 0.0, 400.0);
        let bounds = target.bounds.clone();
        let mut overlay = window(5, 50, 9, 0.0, 400.0);
        overlay.layer = 1000;
        assert!(destination_holds(&[target, overlay], &[1.0, 1.0], (100.0, 10.0), 99, 1, &bounds)
            .is_ok());
    }

    /// Between chunks another window came over the point, then the target
    /// moved: each stops the gesture instead of wheeling on.
    #[test]
    fn a_window_covering_or_the_target_moving_between_chunks_stops_the_gesture() {
        let target = window(1, 10, 1, 0.0, 400.0);
        let bounds = target.bounds.clone();
        assert!(destination_holds(&[target.clone()], &[1.0], (100.0, 10.0), 99, 1, &bounds).is_ok());

        let cover = window(5, 50, 2, 50.0, 100.0);
        let covered =
            destination_holds(&[target.clone(), cover], &[1.0, 1.0], (100.0, 10.0), 99, 1, &bounds);
        assert!(covered.unwrap_err().contains("covered by app50"));

        let mut moved = target;
        moved.bounds.x = 40.0;
        let moved = destination_holds(&[moved], &[1.0], (100.0, 10.0), 99, 1, &bounds);
        assert!(moved.unwrap_err().contains("moved or resized"));
    }

    /// Audit N2: a target that is on top but slow to become the front
    /// process is not a takeover; only a change of front process after the
    /// gate is.
    #[test]
    fn a_moved_pointer_or_a_changed_front_process_is_a_takeover() {
        let a = Some([1u8; 8]);
        let b = Some([2u8; 8]);
        assert_eq!(takeover((100.0, 100.0), Some((101.0, 100.0)), a, a, None), None);
        assert!(takeover((100.0, 100.0), Some((180.0, 90.0)), a, a, None)
            .unwrap()
            .contains("pointer moved"));
        assert!(takeover((100.0, 100.0), Some((100.0, 100.0)), a, b, None)
            .unwrap()
            .contains("another application"));
        assert_eq!(takeover((100.0, 100.0), None, None, b, None), None);
    }

    /// Audit S1: after a gate that passed on expiry, the target's own
    /// activation landing mid-scroll is not the user taking over.
    #[test]
    fn the_targets_own_late_activation_is_not_a_takeover() {
        let prior = Some([1u8; 8]);
        let target = Some([2u8; 8]);
        assert_eq!(takeover((100.0, 100.0), Some((100.0, 100.0)), prior, target, target), None);
        assert!(takeover((100.0, 100.0), Some((100.0, 100.0)), prior, Some([3u8; 8]), target).is_some());
    }
}
