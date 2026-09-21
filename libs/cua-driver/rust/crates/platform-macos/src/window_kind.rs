use std::collections::HashMap;

use crate::windows::{WindowBounds, WindowInfo};

/// The small window-sharing indicator macOS draws inside an application while
/// another process — this driver's own rendering lease, or any other capturer
/// — holds a screen-capture lease on it. It carries the captured
/// application's pid, sits on layer 0 and is on screen, so a WindowServer row
/// alone cannot tell it from an app window; in the accessibility roster it is
/// an `AXDialog` titled "Window", ~66×20 pt, whose single child is
/// `AXButton "WindowSharingSessionButton"`. Listed so a caller can observe
/// that the application is being captured, never part of that application's
/// own UI.
pub const SYSTEM_OVERLAY_KIND: &str = "system_overlay";
/// The per-display window Finder draws the desktop icons on. Not an
/// application window, but a surface a caller may read through
/// `get_window_state`.
pub const DESKTOP_KIND: &str = "desktop";
/// A window the application reports modal (`AXModal`): while it is up, no
/// other window of that process accepts input, so a caller addressing one of
/// them is addressing a window that cannot answer. Read from the
/// accessibility roster, so it appears only when accessibility metadata was
/// requested — a CGWindow row alone cannot say whether a window is modal.
pub const APP_MODAL_KIND: &str = "app-modal";

const INDICATOR_PROVIDER_EXECUTABLE: &str = "/System/Library/Frameworks/AppKit.framework/Versions/C/XPCServices/ThemeWidgetControlViewService.xpc/Contents/MacOS/ThemeWidgetControlViewService";

const APP_WINDOW_LAYER: i32 = 0;
const INDICATOR_MIN_WIDTH_POINTS: f64 = 32.0;
const INDICATOR_MAX_WIDTH_POINTS: f64 = 200.0;
const INDICATOR_MIN_HEIGHT_POINTS: f64 = 12.0;
const INDICATOR_MAX_HEIGHT_POINTS: f64 = 40.0;

const EXECUTABLE_PATH_BUFFER_BYTES: usize = 4096;

pub fn window_sharing_indicator_candidates(
    windows: &[WindowInfo],
    is_indicator_provider_pid: impl Fn(i32) -> bool,
) -> Vec<u32> {
    let mut provider_pids: HashMap<i32, bool> = HashMap::new();
    let mut candidates = Vec::new();

    for host in windows {
        if host.layer != APP_WINDOW_LAYER || !within_indicator_size_class(&host.bounds) {
            continue;
        }
        if provider_pid(&mut provider_pids, &is_indicator_provider_pid, host.pid) {
            continue;
        }
        for hosted in windows {
            if hosted.window_id == host.window_id || hosted.pid == host.pid {
                continue;
            }
            if !encloses(&host.bounds, &hosted.bounds) {
                continue;
            }
            if provider_pid(&mut provider_pids, &is_indicator_provider_pid, hosted.pid) {
                candidates.push(host.window_id);
                break;
            }
        }
    }

    candidates
}

/// The windows a roster reports as `system_overlay`. Candidates are rare (a
/// capture lease draws one indicator), so the ids stay an ordered list.
pub fn system_overlay_window_ids(windows: &[WindowInfo]) -> Vec<u32> {
    window_sharing_indicator_candidates(windows, runs_indicator_provider)
}

fn provider_pid(
    known: &mut HashMap<i32, bool>,
    is_indicator_provider_pid: &impl Fn(i32) -> bool,
    pid: i32,
) -> bool {
    match known.get(&pid) {
        Some(known) => *known,
        None => {
            let is_provider = is_indicator_provider_pid(pid);
            known.insert(pid, is_provider);
            is_provider
        }
    }
}

fn within_indicator_size_class(bounds: &WindowBounds) -> bool {
    (INDICATOR_MIN_WIDTH_POINTS..=INDICATOR_MAX_WIDTH_POINTS).contains(&bounds.width)
        && (INDICATOR_MIN_HEIGHT_POINTS..=INDICATOR_MAX_HEIGHT_POINTS).contains(&bounds.height)
}

fn encloses(outer: &WindowBounds, inner: &WindowBounds) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

pub(crate) fn runs_indicator_provider(pid: i32) -> bool {
    executable_path(pid).as_deref() == Some(INDICATOR_PROVIDER_EXECUTABLE)
}

fn executable_path(pid: i32) -> Option<String> {
    let mut buffer = [0_u8; EXECUTABLE_PATH_BUFFER_BYTES];
    let written = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            EXECUTABLE_PATH_BUFFER_BYTES as u32,
        )
    };
    if written <= 0 {
        return None;
    }
    std::str::from_utf8(&buffer[..written as usize])
        .ok()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURED_APP_PID: i32 = 758;
    const PROVIDER_PID: i32 = 22402;
    const UNRELATED_PID: i32 = 43152;

    fn window(window_id: u32, pid: i32, layer: i32, bounds: (f64, f64, f64, f64)) -> WindowInfo {
        let (x, y, width, height) = bounds;
        WindowInfo {
            window_id,
            pid,
            app_name: "App".into(),
            title: String::new(),
            bounds: WindowBounds {
                x,
                y,
                width,
                height,
            },
            layer,
            z_index: 0,
            is_on_screen: false,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    fn provider_view() -> WindowInfo {
        window(10870, PROVIDER_PID, 0, (240.0, 93.0, 14.0, 14.0))
    }

    fn is_provider(pid: i32) -> bool {
        pid == PROVIDER_PID
    }

    #[test]
    fn indicator_sized_window_enclosing_a_provider_view_is_a_candidate() {
        let windows = vec![
            window(10871, CAPTURED_APP_PID, 0, (191.0, 90.0, 66.0, 20.0)),
            provider_view(),
        ];

        assert_eq!(
            window_sharing_indicator_candidates(&windows, is_provider),
            vec![10871]
        );
    }

    #[test]
    fn full_size_window_enclosing_the_same_provider_view_is_not_a_candidate() {
        let windows = vec![
            window(10661, CAPTURED_APP_PID, 0, (0.0, 34.0, 1696.0, 1083.0)),
            provider_view(),
        ];

        assert!(window_sharing_indicator_candidates(&windows, is_provider).is_empty());
    }

    #[test]
    fn indicator_sized_window_enclosing_only_its_own_pid_is_not_a_candidate() {
        let windows = vec![
            window(10871, CAPTURED_APP_PID, 0, (191.0, 90.0, 66.0, 20.0)),
            window(10870, CAPTURED_APP_PID, 0, (240.0, 93.0, 14.0, 14.0)),
        ];

        assert!(window_sharing_indicator_candidates(&windows, is_provider).is_empty());
    }

    #[test]
    fn indicator_sized_window_enclosing_a_non_provider_view_is_not_a_candidate() {
        let windows = vec![
            window(10871, CAPTURED_APP_PID, 0, (191.0, 90.0, 66.0, 20.0)),
            window(10870, UNRELATED_PID, 0, (240.0, 93.0, 14.0, 14.0)),
        ];

        assert!(window_sharing_indicator_candidates(&windows, is_provider).is_empty());
    }

    #[test]
    fn provider_owned_window_is_never_a_candidate() {
        let windows = vec![
            window(10870, PROVIDER_PID, 0, (191.0, 90.0, 66.0, 20.0)),
            window(10872, UNRELATED_PID, 0, (240.0, 93.0, 14.0, 14.0)),
            window(10873, PROVIDER_PID, 0, (245.0, 95.0, 4.0, 4.0)),
        ];

        assert!(window_sharing_indicator_candidates(&windows, is_provider).is_empty());
    }

    #[test]
    fn accessory_layer_window_enclosing_a_provider_view_is_not_a_candidate() {
        let windows = vec![
            window(10871, CAPTURED_APP_PID, 1, (191.0, 90.0, 66.0, 20.0)),
            provider_view(),
        ];

        assert!(window_sharing_indicator_candidates(&windows, is_provider).is_empty());
    }

    #[test]
    fn indicator_sized_window_beside_a_provider_view_is_not_a_candidate() {
        let windows = vec![
            window(10871, CAPTURED_APP_PID, 0, (191.0, 90.0, 66.0, 20.0)),
            window(10870, PROVIDER_PID, 0, (400.0, 93.0, 14.0, 14.0)),
        ];

        assert!(window_sharing_indicator_candidates(&windows, is_provider).is_empty());
    }
}
