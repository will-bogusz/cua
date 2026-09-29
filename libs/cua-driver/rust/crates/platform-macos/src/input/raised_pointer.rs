//! Foreground pointer envelope: raise one exact window, prove it is the window
//! under a screen point, run pointer input there, then put everything back.
//!
//! A view that scrolls whatever sits under the real pointer (iPhone
//! Mirroring's phone screen is the measured case, but any such view) receives
//! a wheel only when the real cursor is over it and its window is the topmost
//! window at that point. Activating the application is the route that raises
//! the window: `AXFrontmost` on the application element was measured to front
//! and raise in 13-21 ms, where `NSRunningApplication.activate` and the
//! SkyLight front-process SPI activate without raising and `AXRaise` alone
//! does nothing. So this envelope activates with `AXFrontmost`, waits for
//! WindowServer to front the process, and refuses — before any input — when
//! another window still covers the point. It never arms focus suppression:
//! the activation is the point of the envelope, and a suppressor would undo
//! it mid-gesture.

use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::operation;

use crate::ax::bindings::{
    kAXErrorSuccess, set_bool_attr_true, AXUIElementCreateApplication,
    AXUIElementSetMessagingTimeout,
};
use crate::windows::WindowInfo;

/// How long the envelope waits for the target to be front and uncovered.
const RAISE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);
const RAISE_POLL: std::time::Duration = std::time::Duration::from_millis(10);

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

/// Raise `window_id` of `pid` so it is the topmost window at screen point
/// `(x, y)`, park the real cursor there, and run `body`. Afterwards — also on
/// error, refusal or cancellation — the cursor goes back where it was and the
/// previously frontmost application is re-fronted, unless the user fronted a
/// different application meanwhile.
pub fn with_raised_pointer<T>(
    pid: i32,
    window_id: u32,
    x: f64,
    y: f64,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> Result<T, RaisedPointerError> {
    let prior_app =
        crate::input::skylight::frontmost_app_pid().or_else(crate::apps::frontmost_pid);
    let prior_cursor = crate::input::mouse::cursor_location().ok();
    let _restore = operation::ReleaseOnDrop::new(move || {
        if let Some(cursor) = prior_cursor {
            let _ = crate::input::mouse::move_cursor_desktop(cursor.x, cursor.y);
        }
        if let Some(prior) = prior_app.filter(|prior| *prior != pid) {
            if crate::input::skylight::front_process_matches(pid, window_id) != Some(false) {
                set_app_frontmost(prior);
            }
        }
    });
    if prior_app != Some(pid) {
        set_app_frontmost(pid);
    }
    let own_pid = std::process::id() as i32;
    let deadline = std::time::Instant::now() + RAISE_TIMEOUT;
    loop {
        let enumeration = crate::windows::visible_windows_with_space_snapshot();
        let top = topmost_window_at(&enumeration.windows, &enumeration.alphas, x, y, own_pid);
        let on_top = top.is_some_and(|window| window.window_id == window_id);
        let front = crate::input::skylight::front_process_matches(pid, window_id) != Some(false);
        let expired = std::time::Instant::now() >= deadline;
        if on_top && (front || expired) {
            break;
        }
        if expired {
            return Err(RaisedPointerError::Covered(Covered {
                owner: top.map(|window| (window.app_name.clone(), window.pid)),
            }));
        }
        operation::sleep(RAISE_POLL).map_err(anyhow::Error::from)?;
    }
    crate::input::mouse::move_cursor_desktop(x, y)?;
    Ok(body()?)
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
/// transparent windows and this process's own (the agent-cursor overlay is a
/// layer-0 window that ignores mouse events). `alphas` is parallel to
/// `windows`.
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
    use crate::windows::WindowBounds;

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
}
