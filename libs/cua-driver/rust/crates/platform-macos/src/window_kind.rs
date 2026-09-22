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

/// System UI that takes the screen away from the application a caller is
/// driving, drawn by processes other than that application and mostly above
/// layer 0, where `list_windows`' own rows never look. Input keeps flowing to
/// the target while one is up (background routes are pid-addressed), so a
/// caller has to ask.
///
/// `auth`: keychain, admin-rights and Touch ID / password prompts.
/// `permission`: TCC consent dialogs and the accessibility-permission warning
/// (UserNotificationCenter also hosts other alerts: read the window).
/// `lock`: the lock screen, fast user switching, the screen saver.
/// `unknown`: a window drawn at or above the screen-saver level by a process
/// this table does not name — something covers the screen and the driver
/// cannot say what; it is not a verdict that input is blocked.
pub const AUTH_KIND: &str = "auth";
pub const PERMISSION_KIND: &str = "permission";
pub const LOCK_KIND: &str = "lock";
pub const UNKNOWN_SYSTEM_KIND: &str = "unknown";

/// CGWindow owner name (`kCGWindowOwnerName`, lowercased) to system kind.
/// Owner names, not bundle ids: `SecurityAgent` runs as uid 92, where no
/// bundle or accessibility lookup is possible. Observed ground truth for a
/// login-keychain prompt: owner `SecurityAgent`, layer 1000, alpha 1, empty
/// title, with `frontmostApplication` flipping back to the previous app while
/// the panel stayed up — so the owner, not frontmost, is the signal.
const SYSTEM_OWNERS: &[(&str, &str)] = &[
    ("securityagent", AUTH_KIND),
    ("coreautha", AUTH_KIND),
    ("coreauthd", AUTH_KIND),
    ("localauthenticationremoteservice", AUTH_KIND),
    ("usernotificationcenter", PERMISSION_KIND),
    ("universalaccessauthwarn", PERMISSION_KIND),
    ("loginwindow", LOCK_KIND),
    ("screensaverengine", LOCK_KIND),
];

/// One on-screen system window, as `list_windows` publishes it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SystemWindow {
    pub window_id: u32,
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    pub bounds: WindowBounds,
    pub layer: i32,
    pub z_index: usize,
    pub kind: &'static str,
}

/// The system windows among `windows` (front to back, with `alphas`
/// parallel). Invisible and degenerate windows are skipped — the login window
/// keeps zero-alpha placeholders up at all times — and so are `own_pid`'s,
/// which are this driver's overlays.
pub fn system_windows(
    windows: &[WindowInfo],
    alphas: &[f64],
    own_pid: i32,
    screen_saver_level: i32,
) -> Vec<SystemWindow> {
    windows
        .iter()
        .zip(alphas)
        .filter(|(window, &alpha)| {
            window.pid != own_pid
                && alpha > 0.0
                && window.bounds.width >= 1.0
                && window.bounds.height >= 1.0
        })
        .filter_map(|(window, _)| {
            let owner = window.app_name.trim().to_lowercase();
            let kind = SYSTEM_OWNERS
                .iter()
                .find(|(name, _)| *name == owner)
                .map(|(_, kind)| *kind)
                .or_else(|| (window.layer >= screen_saver_level).then_some(UNKNOWN_SYSTEM_KIND))?;
            Some(SystemWindow {
                window_id: window.window_id,
                pid: window.pid,
                app_name: window.app_name.clone(),
                title: window.title.clone(),
                bounds: window.bounds.clone(),
                layer: window.layer,
                z_index: window.z_index,
                kind,
            })
        })
        .collect()
}

/// The system windows on screen now.
pub fn onscreen_system_windows() -> Vec<SystemWindow> {
    let enumeration = crate::windows::onscreen_windows_any_layer();
    system_windows(
        &enumeration.windows,
        &enumeration.alphas,
        std::process::id() as i32,
        crate::windows::screen_saver_window_level(),
    )
}

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

    fn owned(window_id: u32, owner: &str, layer: i32, alpha: f64) -> (WindowInfo, f64) {
        let mut row = window(window_id, 500 + window_id as i32, layer, (0.0, 0.0, 400.0, 300.0));
        row.app_name = owner.into();
        (row, alpha)
    }

    #[test]
    fn system_windows_are_classified_by_owner_and_high_unnamed_layers_are_unknown() {
        const SAVER: i32 = 1000;
        let rows = vec![
            owned(1, "SecurityAgent", 1000, 1.0),
            owned(2, "loginwindow", 0, 0.0), // zero-alpha placeholder
            owned(3, "UserNotificationCenter", 23, 1.0),
            owned(4, "Finder", 0, 1.0),     // ordinary app window
            owned(5, "Dock", 20, 1.0),      // accessory UI below the band
            owned(6, "Mystery", 1500, 1.0), // covers the screen, unnamed
            owned(7, "loginwindow", 2002, 1.0),
            owned(8, "coreautha", 1000, 1.0),
        ];
        let (windows, alphas): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        let found = system_windows(&windows, &alphas, 999, SAVER);
        let kinds: Vec<(u32, &str)> = found.iter().map(|w| (w.window_id, w.kind)).collect();
        assert_eq!(
            kinds,
            [(1, "auth"), (3, "permission"), (6, "unknown"), (7, "lock"), (8, "auth")]
        );
        let published = serde_json::to_value(&found).unwrap();
        serde_json::from_value::<Vec<cua_driver_contract::SystemWindow>>(published)
            .expect("the producer's spelling is the contract's");
    }

    #[test]
    fn the_drivers_own_and_degenerate_windows_are_not_system_windows() {
        let (mut overlay, alpha) = owned(1, "cua-driver", 2000, 1.0);
        overlay.pid = 77;
        let (mut sliver, _) = owned(2, "SecurityAgent", 1000, 1.0);
        sliver.bounds.height = 0.5;
        assert!(system_windows(&[overlay, sliver], &[alpha, 1.0], 77, 1000).is_empty());
    }

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
