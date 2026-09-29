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
//! process, and refuses — before any input — when another window still covers
//! the point. It arms no focus suppression: the activation is the point of the
//! envelope, and a suppressor would undo it, or undo the user's own switch.
//!
//! The body must call [`Envelope::check`] before each burst of input: it stops
//! the gesture when the user moved the pointer or fronted another app (then
//! nothing is restored — the user owns both now), or when the destination
//! stopped being the target window at the point.

use std::sync::atomic::{AtomicBool, Ordering};

use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::operation;

use crate::ax::bindings::{
    kAXErrorSuccess, set_bool_attr_true, AXUIElementCreateApplication,
    AXUIElementSetMessagingTimeout,
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

/// Why a raised gesture must stop sending input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interruption {
    /// The user moved the pointer or fronted another application.
    TakenOver(String),
    /// The target is no longer the window under the point, or moved.
    DestinationChanged(String),
}

impl Interruption {
    pub fn reason(&self) -> &str {
        match self {
            Self::TakenOver(reason) | Self::DestinationChanged(reason) => reason,
        }
    }
}

/// What the body can ask of the envelope.
pub struct Envelope {
    pid: i32,
    window_id: u32,
    point: (f64, f64),
    bounds: WindowBounds,
    own_pid: i32,
    taken_over: AtomicBool,
}

impl Envelope {
    /// The target window's frame when the envelope proved it uncovered.
    pub fn bounds(&self) -> &WindowBounds {
        &self.bounds
    }

    /// `Err` when input must stop now. A takeover also cancels the restore.
    pub fn check(&self) -> Result<(), Interruption> {
        let cursor = crate::input::mouse::cursor_location().ok().map(|p| (p.x, p.y));
        let front = crate::input::skylight::front_process_matches(self.pid, self.window_id);
        if let Some(reason) = takeover(self.point, cursor, front) {
            self.taken_over.store(true, Ordering::SeqCst);
            return Err(Interruption::TakenOver(reason));
        }
        let enumeration = crate::windows::visible_windows_any_layer();
        destination_holds(
            &enumeration.windows,
            &enumeration.alphas,
            self.point,
            self.own_pid,
            self.window_id,
            &self.bounds,
        )
        .map_err(Interruption::DestinationChanged)
    }
}

/// Whether the user took over: the parked cursor moved, or WindowServer
/// fronts another process.
fn takeover(parked: (f64, f64), cursor: Option<(f64, f64)>, front: Option<bool>) -> Option<String> {
    if let Some((x, y)) = cursor {
        if (x - parked.0).abs() > CURSOR_SLACK || (y - parked.1).abs() > CURSOR_SLACK {
            return Some(format!(
                "the pointer moved to ({x:.0}, {y:.0}) during the scroll; the user has it"
            ));
        }
    }
    (front == Some(false)).then(|| "another application came to the front during the scroll".into())
}

/// The target is still the topmost window at the point and has not moved.
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
/// previously frontmost application is re-fronted, unless the user took over
/// (see [`Envelope::check`]) or fronted a different application meanwhile.
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
    let own_pid = std::process::id() as i32;
    let deadline = std::time::Instant::now() + RAISE_TIMEOUT;
    if prior_app != Some(pid) {
        set_app_frontmost(pid);
    }
    let bounds = loop {
        let enumeration = crate::windows::visible_windows_any_layer();
        let top = topmost_window_at(&enumeration.windows, &enumeration.alphas, x, y, own_pid);
        let on_top = top.filter(|window| window.window_id == window_id);
        let front = crate::input::skylight::front_process_matches(pid, window_id) != Some(false);
        let expired = std::time::Instant::now() >= deadline;
        if let Some(window) = on_top.filter(|_| front || expired) {
            break window.bounds.clone();
        }
        if expired {
            restore(pid, window_id, prior_app, None);
            return Err(RaisedPointerError::Covered(Covered {
                owner: top.map(|window| (window.app_name.clone(), window.pid)),
            }));
        }
        if let Err(error) = operation::sleep(RAISE_POLL) {
            restore(pid, window_id, prior_app, None);
            return Err(RaisedPointerError::Failed(error.into()));
        }
    };
    let envelope = Envelope {
        pid,
        window_id,
        point: (x, y),
        bounds,
        own_pid,
        taken_over: AtomicBool::new(false),
    };
    let _restore = operation::ReleaseOnDrop::new(|| {
        if !envelope.taken_over.load(Ordering::SeqCst) {
            restore(pid, window_id, prior_app, prior_cursor.map(|p| (p.x, p.y)));
        }
    });
    crate::input::mouse::move_cursor_desktop(x, y)?;
    Ok(body(&envelope)?)
}

fn restore(pid: i32, window_id: u32, prior_app: Option<i32>, cursor: Option<(f64, f64)>) {
    if let Some((x, y)) = cursor {
        let _ = crate::input::mouse::move_cursor_desktop(x, y);
    }
    if let Some(prior) = prior_app.filter(|prior| *prior != pid) {
        if crate::input::skylight::front_process_matches(pid, window_id) != Some(false) {
            set_app_frontmost(prior);
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

/// The frontmost visible window on any layer containing `(x, y)`, skipping
/// transparent windows and this process's own (the agent-cursor overlay
/// ignores mouse events). A panel, menu or overlay above layer 0 covers the
/// point like any window. `alphas` is parallel to `windows`.
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
            window.layer >= 0
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

    /// Between chunks a floating panel (layer 3) opened over the point, then
    /// the target moved: each stops the gesture instead of wheeling on.
    #[test]
    fn a_panel_opening_or_the_window_moving_between_chunks_stops_the_gesture() {
        let target = window(1, 10, 1, 0.0, 400.0);
        let bounds = target.bounds.clone();
        assert!(destination_holds(&[target.clone()], &[1.0], (100.0, 10.0), 99, 1, &bounds).is_ok());

        let mut panel = window(5, 50, 2, 50.0, 100.0);
        panel.layer = 3;
        let covered =
            destination_holds(&[target.clone(), panel], &[1.0, 1.0], (100.0, 10.0), 99, 1, &bounds);
        assert!(covered.unwrap_err().contains("covered by app50"));

        let mut moved = target;
        moved.bounds.x = 40.0;
        let moved = destination_holds(&[moved], &[1.0], (100.0, 10.0), 99, 1, &bounds);
        assert!(moved.unwrap_err().contains("moved or resized"));
    }

    #[test]
    fn a_moved_pointer_or_another_front_app_is_a_takeover() {
        assert_eq!(takeover((100.0, 100.0), Some((101.0, 100.0)), Some(true)), None);
        assert!(takeover((100.0, 100.0), Some((180.0, 90.0)), Some(true))
            .unwrap()
            .contains("pointer moved"));
        assert!(takeover((100.0, 100.0), Some((100.0, 100.0)), Some(false))
            .unwrap()
            .contains("another application"));
        assert_eq!(takeover((100.0, 100.0), None, None), None);
    }
}
