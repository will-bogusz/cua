//! Fresh exact-target acquisition for background input decisions.
//!
//! Gathers the facts [`cua_driver_core::background_input`] needs about one
//! requested `(pid, CGWindowID)` immediately before a background mutation:
//! WindowServer ownership, fresh `AXWindows` membership (mapped through
//! `_AXUIElementGetWindow`), minimized/hidden state, competing same-pid AX
//! top-level keyboard destinations, and addressed-element ancestry. All reads are
//! bounded and fail closed — an unreadable fact never unlocks a route.

use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::background_input::{
    BackgroundTargetFacts, ElementAncestry, WindowServerOwnership,
};
use std::ffi::CStr;

use super::bindings::{
    actual_pid_of_element, ax_get_window_id, copy_ax_window_surfaces, copy_bool_attr,
    copy_children, copy_element_attr, copy_only_child, copy_string_attr, focused_element_of_pid,
    AXUIElementCreateApplication, AXUIElementRef,
};
use crate::windows::{all_windows, resolve_window_owner, WindowOwner};

/// Bounded `AXParent` ascent used when an element does not expose `AXWindow`.
const MAX_ANCESTRY_DEPTH: usize = 40;

/// Resolve the CGWindowID of the top-level AX window that owns `element`.
///
/// Prefers the element's `AXWindow` attribute and falls back to a bounded
/// `AXParent` walk. `None` means ancestry could not be proven — callers must
/// treat that as "not the requested window", never as a wildcard.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
pub unsafe fn element_window_id(element: AXUIElementRef) -> Option<u32> {
    if matches!(
        copy_string_attr(element, "AXRole").as_deref(),
        Some("AXWindow" | "AXSheet")
    ) {
        if let Some(id) = ax_get_window_id(element) {
            return Some(id);
        }
    }
    if let Some(window) = copy_element_attr(element, "AXWindow") {
        let window_id = ax_get_window_id(window);
        CFRelease(window as CFTypeRef);
        if window_id.is_some() {
            return window_id;
        }
    }
    // Fallback: ascend AXParent until a window role, then map it.
    let mut current: AXUIElementRef = element;
    let mut owned = false;
    let mut resolved = None;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXWindow") | Some("AXSheet") => {
                resolved = ax_get_window_id(current);
                break;
            }
            Some("AXApplication") | None => break,
            _ => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        match parent {
            Some(parent) => {
                current = parent;
                owned = true;
            }
            None => return None,
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    resolved
}

/// The process's focused AX element, but only when it provably belongs to the
/// requested window. Returns a retained element the caller must release.
///
/// This is the only focused-element reader background window-scoped keyboard
/// paths may use: a PID-global focused element can belong to a sibling window,
/// and sibling state must never address or confirm the requested target.
///
/// # Safety
///
/// Caller must `CFRelease` the returned element.
pub unsafe fn focused_element_in_window(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    let element = focused_element_of_pid(pid)?;
    if element_window_id(element) == Some(window_id) {
        Some(element)
    } else {
        CFRelease(element as CFTypeRef);
        None
    }
}

/// AppKit's sharing status dialog is hosted in the target process, but its sole
/// control belongs to the system ThemeWidget service and cannot be its key window.
/// Require backing-process provenance and exact AX ancestry; labels alone never
/// exempt an application-owned dialog. Missing SPI/attributes keep it eligible.
unsafe fn is_system_sharing_overlay(window: AXUIElementRef, window_id: u32, pid: i32) -> bool {
    if copy_string_attr(window, "AXRole").as_deref() != Some("AXWindow")
        || copy_string_attr(window, "AXSubrole").as_deref() != Some("AXDialog")
        || copy_bool_attr(window, "AXModal") != Some(false)
    {
        return false;
    }
    let Some(child) = copy_only_child(window) else {
        return false;
    };
    let proven = (|| {
        if copy_string_attr(child, "AXRole").as_deref() != Some("AXButton")
            || copy_string_attr(child, "AXTitle").as_deref() != Some("WindowSharingSessionButton")
            || element_window_id(child) != Some(window_id)
        {
            return false;
        }
        let Some(provider_pid) = actual_pid_of_element(child).filter(|provider| *provider != pid)
        else {
            return false;
        };
        let mut executable = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let length = libc::proc_pidpath(
            provider_pid,
            executable.as_mut_ptr().cast(),
            executable.len() as u32,
        );
        length > 0
            && CStr::from_bytes_until_nul(&executable).ok()
                == Some(c"/System/Library/Frameworks/AppKit.framework/Versions/C/XPCServices/ThemeWidgetControlViewService.xpc/Contents/MacOS/ThemeWidgetControlViewService")
    })();
    CFRelease(child as CFTypeRef);
    proven
}

/// One fresh `AXWindows` row: the mapped CGWindowID plus its minimized state.
/// `minimized: None` means the attribute could not be read — unknown, not
/// "not minimized".
struct AxWindowRecord {
    window_id: u32,
    minimized: Option<bool>,
    system_sharing_overlay: bool,
    hosted_panel: Option<(i32, u32)>,
}

fn is_file_panel_provider(pid: i32) -> bool {
    let mut executable = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid, executable.as_mut_ptr().cast(), executable.len() as u32) };
    length > 0 && CStr::from_bytes_until_nul(&executable).ok() == Some(c"/System/Library/Frameworks/AppKit.framework/Versions/C/XPCServices/com.apple.appkit.xpc.openAndSavePanelService.xpc/Contents/MacOS/com.apple.appkit.xpc.openAndSavePanelService")
}

/// Classify an observed element for capability reporting, or an action target
/// after exact sheet/element ownership has passed the normal gate.
/// A helper's advertised AXOpen can activate its client even when it returns
/// an error. It is not a qualified background file-selection operation.
pub fn is_file_panel_element(element_ptr: usize) -> bool {
    unsafe { actual_pid_of_element(element_ptr as AXUIElementRef) }
        .is_some_and(is_file_panel_provider)
}

/// AppKit's sheet proxy exposes its remote view as a direct AX child. Prove
/// that exact provider/window pair on every action, never by a cached PID or
/// a matching control label. Other applications' embedded views are not a
/// reason to relax the window ancestry requirement.
unsafe fn hosted_panel_window(sheet: AXUIElementRef) -> Option<(i32, u32)> {
    if copy_string_attr(sheet, "AXRole").as_deref() != Some("AXSheet") {
        return None;
    }
    let mut providers = Vec::new();
    for child in copy_children(sheet) {
        if let (Some(pid), Some(window_id)) =
            (actual_pid_of_element(child), ax_get_window_id(child))
        {
            if is_file_panel_provider(pid) {
                let pair = (pid, window_id);
                if !providers.contains(&pair) {
                    providers.push(pair);
                }
            }
        }
        CFRelease(child as CFTypeRef);
    }
    (providers.len() == 1).then(|| providers[0])
}

fn matches_hosted_panel(
    provider: Option<(i32, u32)>,
    pid: Option<i32>,
    window_id: Option<u32>,
) -> bool {
    matches!((provider, pid, window_id), (Some(expected), Some(pid), Some(window_id)) if expected == (pid, window_id))
}

unsafe fn surface_minimized(window: AXUIElementRef) -> Option<bool> {
    if let Some(value) = copy_bool_attr(window, "AXMinimized") {
        return Some(value);
    }
    if copy_string_attr(window, "AXRole").as_deref() != Some("AXSheet") {
        return None;
    }
    let parent = copy_element_attr(window, "AXWindow")?;
    let value = copy_bool_attr(parent, "AXMinimized");
    CFRelease(parent as CFTypeRef);
    value
}

/// Map the application's fresh `AXWindows` through `_AXUIElementGetWindow`.
/// Windows whose id the SPI cannot resolve are omitted: an unmappable window
/// can never satisfy an exact-target requirement.
unsafe fn ax_window_records(
    app: AXUIElementRef,
    pid: i32,
    target_window_id: u32,
) -> Vec<AxWindowRecord> {
    copy_ax_window_surfaces(app)
        .into_iter()
        .filter_map(|window| {
            let record = ax_get_window_id(window).map(|window_id| {
                let minimized = surface_minimized(window);
                AxWindowRecord {
                    window_id,
                    minimized,
                    system_sharing_overlay: window_id != target_window_id
                        && minimized != Some(true)
                        && is_system_sharing_overlay(window, window_id, pid),
                    hosted_panel: hosted_panel_window(window),
                }
            });
            CFRelease(window as CFTypeRef);
            record
        })
        .collect()
}

/// Count independently AX-mapped, non-minimized sibling top-level windows.
///
/// WindowServer may expose several layer-0 compositor surfaces for one native
/// Electron, Tauri, or WebKit window. A raw same-pid CGWindow row is therefore
/// not enough to prove another process-scoped keyboard destination. Requiring a
/// fresh `AXWindows` mapping preserves the fail-closed two-window guard while
/// ignoring render surfaces that cannot independently become the AX key window.
fn count_competing_keyboard_destinations(
    pid: i32,
    target_window_id: u32,
    window_server_rows: impl IntoIterator<Item = (i32, u32)>,
    ax_records: &[AxWindowRecord],
) -> usize {
    window_server_rows
        .into_iter()
        .filter(|(owner_pid, window_id)| {
            *owner_pid == pid
                && *window_id != target_window_id
                && ax_records.iter().any(|record| {
                    record.window_id == *window_id
                        && record.minimized != Some(true)
                        && !record.system_sharing_overlay
                })
        })
        .count()
}

fn window_owner_pid(pid: i32, window_id: u32) -> Option<i32> {
    match resolve_window_owner(pid, window_id) {
        WindowOwner::SamePid => Some(pid),
        WindowOwner::ForeignPid { owner_pid, .. } => Some(owner_pid),
        WindowOwner::Unknown => None,
    }
}

/// Gather fresh background-input facts for one `(pid, window_id)` target.
///
/// `element_ptr` is an optional retained `AXUIElementRef` (as `usize`) for an
/// explicitly addressed element; the caller must keep it retained for the
/// duration of this call. Blocking: performs one CGWindowList enumeration and
/// bounded AX reads. Call from a blocking context immediately before deciding.
pub fn gather_background_facts(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
) -> BackgroundTargetFacts {
    let window_server = match resolve_window_owner(pid, window_id) {
        WindowOwner::SamePid => WindowServerOwnership::SamePid,
        WindowOwner::Unknown => WindowServerOwnership::NotFound,
        WindowOwner::ForeignPid { owner_pid, .. } => {
            WindowServerOwnership::ForeignPid { owner_pid }
        }
    };

    // SAFETY: the application element is created and released here; window
    // elements are released inside ax_window_records; the caller guarantees
    // element_ptr stays retained.
    let (records, app_hidden, element) = unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            (
                Vec::new(),
                None,
                element_ptr.map(|_| ElementAncestry::Unproven),
            )
        } else {
            // Electron/Chromium apps may need per-process-lifetime enablement
            // before their AX windows and subtrees are materialized.
            super::enablement::ensure_chromium_ax_enabled(pid, app);
            let records = ax_window_records(app, pid, window_id);
            let app_hidden = copy_bool_attr(app, "AXHidden");
            let element = element_ptr.map(|ptr| {
                let element = ptr as AXUIElementRef;
                let hosted = records
                    .iter()
                    .find(|record| record.window_id == window_id)
                    .and_then(|record| record.hosted_panel);
                if matches_hosted_panel(
                    hosted,
                    actual_pid_of_element(element),
                    ax_get_window_id(element),
                ) {
                    return ElementAncestry::ProvenDescendant;
                }
                match element_window_id(element) {
                    Some(id) if id == window_id => ElementAncestry::ProvenDescendant,
                    Some(other) => ElementAncestry::OutsideTargetWindow {
                        pid: window_owner_pid(pid, other),
                        window_id: other,
                    },
                    None => ElementAncestry::Unproven,
                }
            });
            CFRelease(app as CFTypeRef);
            (records, app_hidden, element)
        }
    };

    let target = records.iter().find(|record| record.window_id == window_id);
    let competing_keyboard_destinations = count_competing_keyboard_destinations(
        pid,
        window_id,
        all_windows()
            .iter()
            .map(|window| (window.pid, window.window_id)),
        &records,
    );

    BackgroundTargetFacts {
        window_server,
        ax_window_present: target.is_some(),
        target_minimized: target.and_then(|record| record.minimized),
        app_hidden,
        competing_keyboard_destinations,
        element: element.unwrap_or(ElementAncestry::NotAddressed),
    }
}

#[cfg(test)]
mod tests {
    use super::{count_competing_keyboard_destinations, matches_hosted_panel, AxWindowRecord};

    #[test]
    fn panel_embedding_requires_both_current_provider_and_window() {
        let panel = Some((71, 42));
        assert!(matches_hosted_panel(panel, Some(71), Some(42)));
        assert!(!matches_hosted_panel(panel, Some(72), Some(42)));
        assert!(!matches_hosted_panel(panel, Some(71), Some(43)));
        assert!(!matches_hosted_panel(None, Some(71), Some(42)));
        assert!(!matches_hosted_panel(panel, None, Some(42)));
        assert!(!matches_hosted_panel(panel, Some(71), None));
    }

    fn ax_window(window_id: u32, minimized: Option<bool>) -> AxWindowRecord {
        AxWindowRecord {
            window_id,
            minimized,
            system_sharing_overlay: false,
            hosted_panel: None,
        }
    }

    #[test]
    fn compositor_surfaces_do_not_create_keyboard_ambiguity() {
        let rows = [(42, 10), (42, 11), (42, 12), (42, 13), (42, 14), (42, 15)];
        let records = [ax_window(10, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records),
            0
        );
    }

    #[test]
    fn independently_mapped_sibling_remains_ambiguous() {
        let rows = [(42, 10), (42, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records),
            1
        );
    }
    #[test]
    fn proven_sharing_overlay_is_ignored_without_hiding_a_real_sibling() {
        let rows = [(42, 10), (42, 11), (42, 12)];
        let mut overlay = ax_window(11, Some(false));
        overlay.system_sharing_overlay = true;
        assert_eq!(
            count_competing_keyboard_destinations(
                42,
                10,
                rows,
                &[ax_window(10, Some(false)), overlay]
            ),
            0
        );
        let mut overlay = ax_window(11, Some(false));
        overlay.system_sharing_overlay = true;
        assert_eq!(
            count_competing_keyboard_destinations(
                42,
                10,
                rows,
                &[ax_window(10, Some(false)), overlay, ax_window(12, None)],
            ),
            1
        );
    }

    #[test]
    fn minimized_mapped_sibling_is_not_a_keyboard_destination() {
        let rows = [(42, 10), (42, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(true))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records),
            0
        );
    }

    #[test]
    fn unmapped_window_server_sibling_is_not_a_keyboard_destination() {
        let rows = [(42, 10), (42, 99), (7, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records),
            0
        );
    }
}
