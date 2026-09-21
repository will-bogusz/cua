//! macOS window enumeration via CGWindowList APIs.
//!
//! Uses the C-level CGWindowListCopyWindowInfo API which returns a CFArray
//! of CFDictionary objects describing each window.

use serde::{Deserialize, Serialize};

/// How long to wait for an NSMenu to materialize after `AXShowMenu` returns.
const MENU_APPEARANCE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);
const MENU_APPEARANCE_POLL: std::time::Duration = std::time::Duration::from_millis(40);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: u32,
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    pub bounds: WindowBounds,
    pub layer: i32,
    pub z_index: usize,
    pub is_on_screen: bool,
    /// Active Space on the display WindowServer associates with this window.
    /// This can differ between windows when displays use independent Spaces.
    pub current_space_id: Option<u64>,
    pub on_current_space: Option<bool>,
    pub space_ids: Option<Vec<u64>>,
}

pub(crate) struct WindowEnumeration {
    pub(crate) windows: Vec<WindowInfo>,
    pub(crate) current_space_id: Option<u64>,
}

// ── CGWindow option flags ─────────────────────────────────────────────────────
// Apple-canonical kCG* naming preserved to match the public Apple headers — the
// upper-case-globals lint would rename them to KCG_..., which would silently
// shadow the Apple-namespaced constant references in any future code that
// re-introduces them. Mirrors platform-windows::uia/windows_enum.rs which uses
// the same allow for UIA_* constants.
#[allow(non_upper_case_globals)]
const kCGWindowListOptionAll: u32 = 0;
#[allow(non_upper_case_globals)]
const kCGWindowListExcludeDesktopElements: u32 = 16;
#[allow(non_upper_case_globals)]
const kCGWindowListOptionOnScreenOnly: u32 = 1;
#[allow(non_upper_case_globals)]
const kCGNullWindowID: u32 = 0;
/// `CGWindowLevelKey` for the level Finder draws each display's desktop icons
/// on. The numeric level behind it is resolved at runtime through
/// `CGWindowLevelForKey`, never hard-coded: it has moved between releases.
#[allow(non_upper_case_globals)]
const kCGDesktopIconWindowLevelKey: i32 = 18;

// ── Internal CGWindowInfo parsing ─────────────────────────────────────────────
//
// We use `system_profiler` workaround via `CGWindowListCopyWindowInfo` which
// returns a plist-like structure. The simplest cross-compile-safe approach
// is to dump via `osascript` or use the Objective-C runtime.
//
// For the initial version we use the `core-foundation` crate + direct C linkage.

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(
        option: u32,
        relativeToWindow: u32,
    ) -> core_foundation::array::CFArrayRef;
    fn CGWindowLevelForKey(key: i32) -> i32;
}

/// The CGWindow level of a desktop icon window, as this macOS reports it.
pub fn desktop_icon_window_level() -> i32 {
    static LEVEL: std::sync::LazyLock<i32> =
        std::sync::LazyLock::new(|| unsafe { CGWindowLevelForKey(kCGDesktopIconWindowLevelKey) });
    *LEVEL
}

/// Whether WindowServer files `window` as a desktop surface: the per-display
/// window Finder draws the desktop icons on. It sits at
/// `kCGDesktopIconWindowLevel`, below every application window, and
/// `kCGWindowListExcludeDesktopElements` hides it, so it is neither a layer-0
/// window nor an accessory one.
pub fn is_desktop_surface(window: &WindowInfo) -> bool {
    window.layer == desktop_icon_window_level()
}

/// Enumerate all windows (including off-screen).
pub fn all_windows() -> Vec<WindowInfo> {
    all_windows_with_space_snapshot().windows
}

pub(crate) fn all_windows_with_space_snapshot() -> WindowEnumeration {
    enumerate_windows(kCGWindowListExcludeDesktopElements, LayerFilter::ZeroOnly)
}

/// Enumerate only on-screen windows.
pub fn visible_windows() -> Vec<WindowInfo> {
    visible_windows_with_space_snapshot().windows
}

pub(crate) fn visible_windows_with_space_snapshot() -> WindowEnumeration {
    enumerate_windows(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        LayerFilter::ZeroOnly,
    )
}

/// What `list_windows` reports: every layer-0 window plus each display's
/// desktop surface, from one enumeration so every `z_index` in the listing
/// comes from the same WindowServer order (on screen, the desktop sits behind
/// every application window).
///
/// Kept apart from [`all_windows`]: the desktop is a place to read, not a
/// candidate for main-window selection or keyboard-destination counting.
pub(crate) fn listable_windows_with_space_snapshot(on_screen_only: bool) -> WindowEnumeration {
    let options = if on_screen_only {
        kCGWindowListOptionOnScreenOnly
    } else {
        kCGWindowListOptionAll
    };
    enumerate_windows(options, LayerFilter::ZeroOrDesktopSurface)
}

/// Enumerate windows on every CGWindow layer, including the accessory layers
/// (`layer != 0`) that [`all_windows`] hides and the desktop elements that
/// `kCGWindowListExcludeDesktopElements` would drop.
///
/// Only used to answer "does this CGWindowID exist, and who owns it?" — the
/// question `list_windows` must NOT answer, because surfacing tooltips,
/// popovers, the Dock and every NSMenu window would swamp callers. Keeping the
/// layer filter on enumeration and off identity lookup is what lets
/// `get_window_state` tell "no such window" apart from "exists, but is not a
/// layer-0 window" (issue #2237), and admitting desktop elements here is what
/// lets it recognise Finder's desktop surface as a live window of Finder's.
fn all_windows_any_layer() -> Vec<WindowInfo> {
    enumerate_windows(kCGWindowListOptionAll, LayerFilter::AnyLayer).windows
}

/// Which CGWindow layers an enumeration admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerFilter {
    /// Normal application windows only.
    ZeroOnly,
    /// Normal application windows plus the desktop surfaces — what
    /// `list_windows` reports.
    ZeroOrDesktopSurface,
    /// Every layer, accessory windows included.
    AnyLayer,
}

impl LayerFilter {
    fn admits(self, layer: i32) -> bool {
        match self {
            LayerFilter::ZeroOnly => layer == 0,
            LayerFilter::ZeroOrDesktopSurface => layer == 0 || layer == desktop_icon_window_level(),
            LayerFilter::AnyLayer => true,
        }
    }

    /// Space metadata is attached to listings, not to identity lookups.
    fn reports_spaces(self) -> bool {
        !matches!(self, LayerFilter::AnyLayer)
    }
}

fn enumerate_windows(options: u32, layers: LayerFilter) -> WindowEnumeration {
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, CFTypeRef, TCFType},
        boolean::CFBoolean,
        dictionary::CFDictionary,
        number::CFNumber,
        string::CFString,
    };
    use std::os::raw::c_void;

    let space_query = layers
        .reports_spaces()
        .then(crate::input::skylight::SpaceQuery::new)
        .flatten();
    let current_space_id = space_query
        .as_ref()
        .and_then(|query| query.current_space_id());

    let raw_ref = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if raw_ref.is_null() {
        return WindowEnumeration {
            windows: vec![],
            current_space_id,
        };
    }

    let raw: CFArray<CFTypeRef> = unsafe { CFArray::wrap_under_create_rule(raw_ref as _) };
    let total = raw.len() as usize;
    let mut results = Vec::new();

    for (idx, item) in raw.iter().enumerate() {
        let item = *item;
        // Each item should be a CFDictionary.
        let dict_type = CFDictionary::<*const c_void, *const c_void>::type_id();
        if unsafe { CFGetTypeID(item) } != dict_type {
            continue;
        }

        let dict: CFDictionary<*const c_void, *const c_void> =
            unsafe { CFDictionary::wrap_under_get_rule(item as _) };

        // Helper: get number from dict by key string.
        let get_num = |key: &str| -> i64 {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFNumber::type_id() {
                        CFNumber::wrap_under_get_rule(v as _).to_i64()
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
        };

        let get_str = |key: &str| -> String {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFString::type_id() {
                        Some(CFString::wrap_under_get_rule(v as _).to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        };

        let get_bool = |key: &str| -> bool {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .map(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFBoolean::type_id() {
                        bool::from(CFBoolean::wrap_under_get_rule(v as _))
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
        };

        let window_id = get_num("kCGWindowNumber") as u32;
        let pid = get_num("kCGWindowOwnerPID") as i32;
        let app_name = get_str("kCGWindowOwnerName");
        let title = get_str("kCGWindowName");
        let layer = get_num("kCGWindowLayer") as i32;
        let is_on_screen = get_bool("kCGWindowIsOnscreen");

        if !layers.admits(layer) {
            continue;
        }

        // Parse bounds dict.
        let bounds = {
            let bk = CFString::new("kCGWindowBounds");
            dict.find(bk.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFDictionary::<*const c_void, *const c_void>::type_id() {
                        let bd: CFDictionary<*const c_void, *const c_void> =
                            CFDictionary::wrap_under_get_rule(v as _);
                        let x = get_bounds_num(&bd, "X");
                        let y = get_bounds_num(&bd, "Y");
                        let w = get_bounds_num(&bd, "Width");
                        let h = get_bounds_num(&bd, "Height");
                        Some(WindowBounds {
                            x,
                            y,
                            width: w,
                            height: h,
                        })
                    } else {
                        None
                    }
                })
                .unwrap_or(WindowBounds {
                    x: 0.,
                    y: 0.,
                    width: 0.,
                    height: 0.,
                })
        };

        // z_index: CGWindowList front-to-back → assign reverse index.
        let z_index = z_index_from_front_to_back(total, idx);

        results.push(WindowInfo {
            window_id,
            pid,
            app_name,
            title,
            bounds,
            layer,
            z_index,
            is_on_screen,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        });
    }

    if layers.reports_spaces() {
        let Some(query) = &space_query else {
            return WindowEnumeration {
                windows: results,
                current_space_id,
            };
        };
        for window in &mut results {
            let space_ids = query.window_space_ids(window.window_id);
            let display_space_id = space_ids
                .as_ref()
                .and_then(|_| query.current_space_for_window(window.window_id));
            apply_window_space_metadata(window, space_ids, display_space_id);
        }
    }

    WindowEnumeration {
        windows: results,
        current_space_id,
    }
}

fn apply_window_space_metadata(
    window: &mut WindowInfo,
    space_ids: Option<Vec<u64>>,
    current_space_id: Option<u64>,
) {
    window.on_current_space = window_on_current_space(space_ids.as_deref(), current_space_id);
    window.current_space_id = current_space_id;
    window.space_ids = space_ids;
}

fn window_on_current_space(
    space_ids: Option<&[u64]>,
    current_space_id: Option<u64>,
) -> Option<bool> {
    Some(space_ids?.contains(&current_space_id?))
}

fn z_index_from_front_to_back(total: usize, position: usize) -> usize {
    total.saturating_sub(position)
}

fn get_bounds_num(
    dict: &core_foundation::dictionary::CFDictionary<
        *const std::os::raw::c_void,
        *const std::os::raw::c_void,
    >,
    key: &str,
) -> f64 {
    use core_foundation::{
        base::{CFGetTypeID, TCFType},
        number::CFNumber,
        string::CFString,
    };
    use std::os::raw::c_void;

    let k = CFString::new(key);
    dict.find(k.as_concrete_TypeRef() as *const c_void)
        .and_then(|v| unsafe {
            let v = *v;
            if CFGetTypeID(v) == CFNumber::type_id() {
                CFNumber::wrap_under_get_rule(v as _).to_f64()
            } else {
                None
            }
        })
        .unwrap_or(0.0)
}

/// Look up a window by its CGWindowID across every layer.
///
/// Returns `None` only when WindowServer has no record of the id at all —
/// which is precisely the "closed or fabricated window_id" signal callers need.
pub fn window_info_by_id(window_id: u32) -> Option<WindowInfo> {
    all_windows_any_layer()
        .into_iter()
        .find(|w| w.window_id == window_id)
}

/// The accessory (`layer != 0`) windows `pid` currently shows, front first.
///
/// An open NSMenu is one of them, and its frame is where an accessibility
/// hit-test can find the menu that nothing in the owning window's subtree
/// points at.
pub fn accessory_windows(pid: i32) -> Vec<WindowInfo> {
    let mut windows: Vec<WindowInfo> = all_windows_any_layer()
        .into_iter()
        .filter(|window| window.pid == pid && window.layer != 0 && window.is_on_screen)
        .collect();
    windows.sort_by_key(|window| std::cmp::Reverse(window.z_index));
    windows
}

/// CGWindowIDs of the accessory windows `pid` currently shows.
///
/// Sampling this set across an `AXShowMenu` answers whether a menu actually
/// appeared: the action returns `kAXErrorSuccess` on controls that never
/// open one.
pub fn accessory_window_ids(pid: i32) -> Vec<u32> {
    accessory_windows(pid)
        .into_iter()
        .map(|window| window.window_id)
        .collect()
}

/// Whether `pid` opened an accessory window it did not have in `before`.
///
/// An NSMenu materializes asynchronously after `AXShowMenu` returns, so this
/// polls for a bounded interval before answering no.
pub fn menu_appeared_since(pid: i32, before: &[u32]) -> bool {
    let deadline = std::time::Instant::now() + MENU_APPEARANCE_TIMEOUT;
    loop {
        if accessory_window_ids(pid)
            .into_iter()
            .any(|window_id| !before.contains(&window_id))
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(MENU_APPEARANCE_POLL);
    }
}

/// How long the AX window inventory behind [`capture_content_bounds`] may
/// take. A slow answer degrades to "no window is AX-mapped", which widens the
/// captured rect rather than narrowing it — the safe direction, because a
/// rect that is too small is the mislabel this exists to remove.
const AX_WINDOW_INVENTORY_TIMEOUT_SECONDS: f32 = 0.25;

/// The CGWindowIDs `pid` exposes as top-level `AXWindow`s.
///
/// A surface the application hangs over one of its windows — a popover, a
/// menu, a tooltip, an autocomplete panel — has no `AXWindow` of its own, so
/// membership here is what separates "another window of this application"
/// from "part of this window's presentation".
fn ax_mapped_window_ids(pid: i32) -> Vec<u32> {
    use core_foundation::base::{CFRelease, CFTypeRef};
    // SAFETY: every element below is owned by this function — the application
    // element is created here and each window comes from a `+1` copy — and
    // each one is released exactly once.
    unsafe {
        let app = crate::ax::bindings::AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Vec::new();
        }
        crate::ax::bindings::AXUIElementSetMessagingTimeout(
            app,
            AX_WINDOW_INVENTORY_TIMEOUT_SECONDS,
        );
        let ids = crate::ax::bindings::copy_ax_windows(app)
            .into_iter()
            .filter_map(|window| {
                let id = crate::ax::bindings::ax_get_window_id(window);
                CFRelease(window as CFTypeRef);
                id
            })
            .collect();
        CFRelease(app as CFTypeRef);
        ids
    }
}

fn rects_intersect(a: &WindowBounds, b: &WindowBounds) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

fn rect_union(a: &WindowBounds, b: &WindowBounds) -> WindowBounds {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    WindowBounds {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

/// Whether `candidate` is part of how `target` is presented rather than a
/// window in its own right: same process, on screen, drawn in front of the
/// target, and carrying no `AXWindow` identity.
fn is_attached_surface(target: &WindowInfo, candidate: &WindowInfo, ax_mapped: &[u32]) -> bool {
    candidate.window_id != target.window_id
        && candidate.pid == target.pid
        && candidate.is_on_screen
        && candidate.z_index > target.z_index
        && candidate.bounds.width > 0.0
        && candidate.bounds.height > 0.0
        && !is_desktop_surface(candidate)
        && !ax_mapped.contains(&candidate.window_id)
}

/// The screen rect a window capture covers: `target`'s own frame grown by
/// every attached surface that touches it, transitively — a popover anchored
/// to the window, and the menu that popover's own popup button opened.
///
/// ScreenCaptureKit's desktop-independent window filter renders those
/// surfaces into the window's capture, so the window's frame is not the
/// frame the pixels cover once one is open. Pure over an enumeration so the
/// geometry is testable without a WindowServer.
pub(crate) fn capture_content_union(
    target: &WindowInfo,
    windows: &[WindowInfo],
    ax_mapped: &[u32],
) -> WindowBounds {
    let mut union = target.bounds.clone();
    let mut absorbed: Vec<u32> = vec![target.window_id];
    loop {
        let mut grew = false;
        for candidate in windows {
            if absorbed.contains(&candidate.window_id)
                || !is_attached_surface(target, candidate, ax_mapped)
                || !rects_intersect(&union, &candidate.bounds)
            {
                continue;
            }
            union = rect_union(&union, &candidate.bounds);
            absorbed.push(candidate.window_id);
            grew = true;
        }
        if !grew {
            return union;
        }
    }
}

/// [`capture_content_union`] against the live WindowServer. `None` when the
/// requested window is no longer listed.
///
/// The AX inventory is only read once a same-process window is actually
/// drawn in front of the target, so the ordinary capture — one window, no
/// attached surface — pays nothing and reports the window's own frame.
pub fn capture_content_bounds(window_id: u32) -> Option<WindowBounds> {
    let windows = all_windows_any_layer();
    let target = windows.iter().find(|w| w.window_id == window_id)?;
    let has_candidate = windows.iter().any(|candidate| {
        candidate.window_id != target.window_id
            && candidate.pid == target.pid
            && candidate.is_on_screen
            && candidate.z_index > target.z_index
            && !is_desktop_surface(candidate)
            && rects_intersect(&target.bounds, &candidate.bounds)
    });
    if !has_candidate {
        return Some(target.bounds.clone());
    }
    let ax_mapped = ax_mapped_window_ids(target.pid);
    Some(capture_content_union(target, &windows, &ax_mapped))
}

/// Look up a window's bounds by its CGWindowID.
///
/// Returns `None` if the window is not currently known to WindowServer
/// (e.g. it was closed or the window_id is stale).
pub fn window_bounds_by_id(window_id: u32) -> Option<WindowBounds> {
    window_info_by_id(window_id).map(|w| w.bounds)
}

/// Who owns a requested CGWindowID, as seen by a caller that asked about `pid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowOwner {
    /// The window exists and `pid` owns it.
    SamePid,
    /// The window exists, but a different process owns it. macOS hosts a
    /// sandboxed app's Open/Save panel in
    /// `com.apple.appkit.xpc.openAndSavePanelService`, so the panel's
    /// CGWindowID belongs to that service and not to the app that opened it
    /// (issue #2237).
    ForeignPid {
        owner_pid: i32,
        owner_app_name: String,
    },
    /// WindowServer has no record of the id — closed, stale, or fabricated.
    Unknown,
}

/// Pure form of [`resolve_window_owner`] over an already-enumerated window
/// list, so the ownership decision is testable without a WindowServer.
pub fn resolve_window_owner_in(windows: &[WindowInfo], pid: i32, window_id: u32) -> WindowOwner {
    match windows.iter().find(|w| w.window_id == window_id) {
        None => WindowOwner::Unknown,
        Some(w) if w.pid == pid => WindowOwner::SamePid,
        Some(w) => WindowOwner::ForeignPid {
            owner_pid: w.pid,
            owner_app_name: w.app_name.clone(),
        },
    }
}

/// Resolve whether `pid` really owns `window_id`. Blocking (one CGWindowList
/// enumeration).
pub fn resolve_window_owner(pid: i32, window_id: u32) -> WindowOwner {
    resolve_window_owner_in(&all_windows_any_layer(), pid, window_id)
}

/// Select the best window_id for a pid.
pub fn resolve_main_window_id(pid: i32) -> anyhow::Result<u32> {
    let windows = all_windows();
    let pid_windows: Vec<&WindowInfo> = windows.iter().filter(|w| w.pid == pid).collect();
    if pid_windows.is_empty() {
        anyhow::bail!("pid {pid} has no windows");
    }
    let mut on_screen: Vec<&&WindowInfo> = pid_windows.iter().filter(|w| w.is_on_screen).collect();
    if !on_screen.is_empty() {
        on_screen.sort_by_key(|window| std::cmp::Reverse(window.z_index));
        return Ok(on_screen[0].window_id);
    }
    let largest = pid_windows.iter().max_by(|a, b| {
        let area_a = a.bounds.width * a.bounds.height;
        let area_b = b.bounds.width * b.bounds.height;
        area_a
            .partial_cmp(&area_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(largest.unwrap().window_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cg_front_to_back_order_normalizes_to_higher_is_frontmost() {
        let indices: Vec<_> = (0..3)
            .map(|position| z_index_from_front_to_back(3, position))
            .collect();
        assert_eq!(indices, vec![3, 2, 1]);
        assert!(indices[0] > indices[2]);
    }

    #[test]
    fn space_membership_checks_all_spaces_for_a_window() {
        assert_eq!(window_on_current_space(Some(&[2, 4]), Some(4)), Some(true));
        assert_eq!(window_on_current_space(Some(&[2, 4]), Some(3)), Some(false));
    }

    #[test]
    fn space_membership_stays_unknown_without_either_side() {
        assert_eq!(window_on_current_space(None, Some(4)), None);
        assert_eq!(window_on_current_space(Some(&[4]), None), None);
    }

    #[test]
    fn per_window_current_space_is_the_one_used_for_membership() {
        let mut secondary_display_window = window(42, 800, "TextEdit");
        apply_window_space_metadata(&mut secondary_display_window, Some(vec![2, 4]), Some(4));

        assert_eq!(secondary_display_window.current_space_id, Some(4));
        assert_eq!(secondary_display_window.space_ids, Some(vec![2, 4]));
        assert_eq!(secondary_display_window.on_current_space, Some(true));
        assert!(secondary_display_window
            .space_ids
            .as_deref()
            .is_some_and(|spaces| spaces.contains(
                &secondary_display_window
                    .current_space_id
                    .expect("display Space must be present")
            )));
    }

    fn window(window_id: u32, pid: i32, app_name: &str) -> WindowInfo {
        WindowInfo {
            window_id,
            pid,
            app_name: app_name.into(),
            title: String::new(),
            bounds: WindowBounds {
                x: 0.,
                y: 580.,
                width: 500.,
                height: 500.,
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
    fn owner_resolves_same_pid() {
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 42),
            WindowOwner::SamePid
        );
    }

    /// Issue #2237: TextEdit's Open panel is a layer-0 CGWindow owned by
    /// `com.apple.appkit.xpc.openAndSavePanelService`, not by TextEdit. The
    /// caller must be told the real owner pid, not handed TextEdit's menu bar.
    #[test]
    fn owner_detects_out_of_process_panel_host() {
        let windows = vec![
            window(41, 800, "TextEdit"),
            window(42, 900, "Open and Save Panel Service"),
        ];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 42),
            WindowOwner::ForeignPid {
                owner_pid: 900,
                owner_app_name: "Open and Save Panel Service".into(),
            }
        );
    }

    #[test]
    fn owner_is_unknown_for_fabricated_id() {
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 0xFFFF_FFF0),
            WindowOwner::Unknown
        );
    }

    #[test]
    fn owner_is_unknown_for_zero_id() {
        // kCGNullWindowID is never a real window number.
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 0),
            WindowOwner::Unknown
        );
    }

    #[test]
    fn owner_is_unknown_after_the_window_closes() {
        // Stale id: it was enumerated once, then the panel was dismissed.
        let before = vec![window(42, 900, "Open and Save Panel Service")];
        assert_eq!(
            resolve_window_owner_in(&before, 900, 42),
            WindowOwner::SamePid
        );
        assert_eq!(resolve_window_owner_in(&[], 900, 42), WindowOwner::Unknown);
    }

    /// Finder draws each display's desktop icons on a window WindowServer
    /// files below every application window. A listing must offer it (it is
    /// a place a caller can read) without admitting the accessory layers,
    /// and an identity lookup must recognise every layer.
    #[test]
    fn listing_admits_layer_zero_and_desktop_surfaces_only() {
        let desktop = desktop_icon_window_level();
        assert!(desktop < 0, "desktop icons sit below the application layer");
        for (filter, layer, admitted) in [
            (LayerFilter::ZeroOrDesktopSurface, 0, true),
            (LayerFilter::ZeroOrDesktopSurface, desktop, true),
            (LayerFilter::ZeroOrDesktopSurface, desktop - 1, false),
            (LayerFilter::ZeroOrDesktopSurface, 25, false),
            (LayerFilter::ZeroOnly, desktop, false),
            (LayerFilter::AnyLayer, desktop, true),
            (LayerFilter::AnyLayer, 25, true),
        ] {
            assert_eq!(filter.admits(layer), admitted, "{filter:?} layer {layer}");
        }
        let mut surface = window(9814, 800, "Finder");
        surface.layer = desktop;
        assert!(is_desktop_surface(&surface));
        assert!(!is_desktop_surface(&window(42, 800, "Finder")));
    }

    fn surface(
        window_id: u32,
        pid: i32,
        z_index: usize,
        bounds: (f64, f64, f64, f64),
    ) -> WindowInfo {
        let mut info = window(window_id, pid, "Harness");
        info.z_index = z_index;
        info.bounds = WindowBounds {
            x: bounds.0,
            y: bounds.1,
            width: bounds.2,
            height: bounds.3,
        };
        info
    }

    fn rect(bounds: &WindowBounds) -> (f64, f64, f64, f64) {
        (bounds.x, bounds.y, bounds.width, bounds.height)
    }

    /// One window, nothing hanging over it: the capture covers the window and
    /// nothing else, so the 1:1 grid every pixel action rests on stays true.
    #[test]
    fn a_lone_window_is_captured_at_its_own_frame() {
        let target = surface(10, 800, 1, (253., 34., 935., 598.));
        let others = vec![surface(11, 900, 5, (0., 0., 1512., 900.))];
        let windows = vec![target.clone(), others[0].clone()];
        assert_eq!(
            rect(&capture_content_union(&target, &windows, &[10])),
            (253., 34., 935., 598.)
        );
    }

    /// The measured shape: a 935x598 window with a 326x465 popover hanging
    /// off its right edge and below its bottom. The capture the window server
    /// renders is the 1171x776 union, and reporting the window's own frame is
    /// what labelled a 0.77x image "1 px = 1 window point".
    #[test]
    fn a_popover_overflowing_the_window_grows_the_captured_rect() {
        let target = surface(29973, 800, 3, (253., 34., 935., 598.));
        let popover = surface(29979, 800, 4, (1098., 345., 326., 465.));
        let windows = vec![target.clone(), popover];
        let union = capture_content_union(&target, &windows, &[29973]);
        assert_eq!(rect(&union), (253., 34., 1171., 776.));
    }

    /// A menu opened from a control inside that popover touches the popover,
    /// not the window. The union has to close transitively or the menu's
    /// pixels are in the image while the reported frame denies them.
    #[test]
    fn a_menu_hanging_off_the_popover_is_reached_transitively() {
        let target = surface(29973, 800, 3, (0., 0., 900., 600.));
        let popover = surface(29979, 800, 4, (850., 500., 300., 300.));
        let menu = surface(30010, 800, 5, (1100., 700., 200., 400.));
        let windows = vec![target.clone(), menu, popover];
        let union = capture_content_union(&target, &windows, &[29973]);
        assert_eq!(rect(&union), (0., 0., 1300., 1100.));
    }

    /// Another document window of the same application is a window in its own
    /// right — it carries an `AXWindow` identity — and the desktop-independent
    /// filter does not draw it into this window's capture. Absorbing it would
    /// claim a frame the pixels never covered.
    #[test]
    fn another_window_of_the_same_app_is_not_part_of_this_capture() {
        let target = surface(10, 800, 3, (0., 0., 900., 600.));
        let sibling = surface(11, 800, 4, (400., 300., 900., 600.));
        let windows = vec![target.clone(), sibling];
        assert_eq!(
            rect(&capture_content_union(&target, &windows, &[10, 11])),
            (0., 0., 900., 600.)
        );
    }

    /// Behind the target is behind the capture: a surface the window server
    /// draws under the window contributes no pixels to it.
    #[test]
    fn a_surface_behind_the_target_is_not_part_of_this_capture() {
        let target = surface(10, 800, 5, (0., 0., 900., 600.));
        let behind = surface(11, 800, 2, (800., 500., 400., 400.));
        let windows = vec![target.clone(), behind];
        assert_eq!(
            rect(&capture_content_union(&target, &windows, &[10])),
            (0., 0., 900., 600.)
        );
    }
}
