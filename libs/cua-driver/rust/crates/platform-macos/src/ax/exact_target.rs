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
    try_copy_string_attr, AXUIElementCreateApplication, AXUIElementRef,
};
use crate::windows::{all_windows, resolve_window_owner, WindowInfo, WindowOwner};

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
        current = parent?;
        owned = true;
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    resolved
}

/// Whether `element` belongs to `pid`'s own menu bar.
///
/// A menu bar and its rows are process-scoped: they carry no CGWindowID and
/// their `AXParent` chain terminates at `AXApplication` without passing an
/// `AXWindow`, so [`element_window_id`] returns `None` for every menu row and
/// window ancestry can never be proven. This proves the exact fact that does
/// exist instead — the row ascends to the requested process's `AXMenuBar` —
/// and leaves the routing decision to the core gate.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn element_is_app_menu_descendant(element: AXUIElementRef, pid: i32) -> bool {
    if actual_pid_of_element(element) != Some(pid) {
        return false;
    }
    let mut current: AXUIElementRef = element;
    let mut owned = false;
    let mut in_menu_bar = false;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXMenuBar") => {
                in_menu_bar = true;
                break;
            }
            Some("AXWindow" | "AXSheet" | "AXApplication") | None => break,
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
            None => return false,
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    in_menu_bar
}

/// How the sheet an element lives in relates to the requested window.
///
/// AppKit answers `AXWindow` for a control inside a sheet with the window the
/// sheet is attached to, never the sheet, while `_AXUIElementGetWindow` maps
/// the control to the sheet's own CGWindowID. Window ancestry through
/// [`element_window_id`] therefore always points away from a sheet, in both
/// directions, and neither answer is a reason to refuse the element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SheetAncestry {
    /// The requested window IS the sheet the element lives in. The element is
    /// exactly addressed: the sheet is its own WindowServer window and, while
    /// modal, the key window of its application.
    RequestedWindowIsTheSheet,
    /// The element lives in a sheet attached to the requested window. The
    /// sheet's own CGWindowID when WindowServer gave it one — the identity
    /// `get_window_state` reports for it under `related_windows`, which is
    /// the scope the caller can re-target. `None` when the sheet has no
    /// CGWindowID of its own, in which case it is composited into the
    /// requested window and that window is its only scope.
    AttachedToRequestedWindow(Option<u32>),
}

/// Resolve `element`'s nearest `AXSheet` ancestor against `target_window_id`.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn sheet_ancestry(element: AXUIElementRef, target_window_id: u32) -> Option<SheetAncestry> {
    let mut current: AXUIElementRef = element;
    let mut owned = false;
    let mut ancestry = None;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXSheet") => {
                let sheet_window_id = ax_get_window_id(current);
                if sheet_window_id == Some(target_window_id) {
                    ancestry = Some(SheetAncestry::RequestedWindowIsTheSheet);
                    break;
                }
                let attached_to = copy_element_attr(current, "AXParent").and_then(|parent| {
                    let id = ax_get_window_id(parent);
                    CFRelease(parent as CFTypeRef);
                    id
                });
                if attached_to == Some(target_window_id) {
                    ancestry = Some(SheetAncestry::AttachedToRequestedWindow(sheet_window_id));
                }
                break;
            }
            Some("AXWindow" | "AXApplication") | None => break,
            _ => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        current = parent?;
        owned = true;
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    ancestry
}

/// WindowServer's owner for one CGWindowID, as `get_window_state` reports it.
/// `None` means WindowServer has no record of the id, so no owner may be named.
fn window_owner_pid(pid: i32, window_id: u32) -> Option<i32> {
    match resolve_window_owner(pid, window_id) {
        WindowOwner::SamePid => Some(pid),
        WindowOwner::ForeignPid { owner_pid, .. } => Some(owner_pid),
        WindowOwner::Unknown => None,
    }
}

/// Classify an addressed element that did not resolve to the requested window.
/// `resolved` is the window its `AXWindow`/`AXParent` chain did reach, when
/// one could be mapped at all.
///
/// A sheet is checked first: it is the requested window's modal state, not a
/// separate scope the caller chose, and the window cannot be used again until
/// the sheet is dismissed.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn classify_foreign_ancestry(
    element: AXUIElementRef,
    pid: i32,
    target_window_id: u32,
    resolved: Option<u32>,
) -> ElementAncestry {
    match sheet_ancestry(element, target_window_id) {
        // The requested window is the sheet this element lives in, or a sheet
        // WindowServer never gave an id to, which is composited into the
        // requested window. Either way the element is in the requested window.
        Some(SheetAncestry::RequestedWindowIsTheSheet)
        | Some(SheetAncestry::AttachedToRequestedWindow(None)) => {
            return ElementAncestry::ProvenDescendant
        }
        Some(SheetAncestry::AttachedToRequestedWindow(Some(sheet_window_id))) => {
            if let Some(owner_pid) = window_owner_pid(pid, sheet_window_id) {
                return ElementAncestry::ProvenAttachedSheet {
                    pid: owner_pid,
                    window_id: sheet_window_id,
                };
            }
        }
        None => {}
    }
    match resolved {
        Some(window_id) => ElementAncestry::OutsideTargetWindow {
            pid: window_owner_pid(pid, window_id),
            window_id,
        },
        None if element_is_app_menu_descendant(element, pid) => ElementAncestry::ProvenAppMenu,
        None => ElementAncestry::Unproven,
    }
}

/// The process's focused AX element, but only when it provably belongs to the
/// requested window. Returns a retained element the caller must release.
///
/// This is the only focused-element reader background window-scoped keyboard
/// paths may use: a PID-global focused element can belong to a sibling window,
/// and sibling state must never address or confirm the requested target.
///
/// An element in a surface the requested window owns
/// ([`focused_owned_child_surface`]) belongs to it: the window is where the
/// application is editing, and its editor has no window ancestry to walk.
///
/// # Safety
///
/// Caller must `CFRelease` the returned element.
pub unsafe fn focused_element_in_window(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    let element = focused_element_of_pid(pid)?;
    if element_window_id(element) == Some(window_id)
        || element_in_owned_child_surface(element, pid, window_id)
    {
        Some(element)
    } else {
        CFRelease(element as CFTypeRef);
        None
    }
}

/// Whether `element` lives in a surface `window_id` owns — the case where the
/// application's own editing surface holds the keyboard and accessibility
/// publishes no window ancestry for it at all.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn element_in_owned_child_surface(
    element: AXUIElementRef,
    pid: i32,
    window_id: u32,
) -> bool {
    if element_window_id(element).is_some() {
        return false;
    }
    let Some(element_window) = ax_get_window_id(element) else {
        return false;
    };
    focused_owned_child_surface(pid, window_id) == Some(element_window)
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

/// What the application published one of its window surfaces as.
///
/// `AXWindows` is not a list of windows. An application may answer it with a
/// control that happens to own its own WindowServer window: measured in
/// Finder, the inline rename editor is published as the `AXTextField` itself —
/// role `AXTextField`, `CFEqual` to the application's `AXFocusedUIElement`,
/// mapped by `_AXUIElementGetWindow`, with no `AXWindow` and no
/// `AXTopLevelUIElement` attribute and an `AXParent` chain that goes straight
/// to `AXApplication`. Such a surface cannot be addressed, activated or made
/// key on its own; it is part of the window it is drawn inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceRole {
    /// Role `AXWindow`, or `AXSheet` — the requested window's own modal state,
    /// which is still its own key window. Both are keyboard destinations in
    /// their own right.
    Independent,
    /// Any other role: a control the application hosts in its own child
    /// window, which no caller can address or activate separately.
    HostedControl,
}

/// One fresh `AXWindows` row: the mapped CGWindowID plus its minimized state.
/// `minimized: None` means the attribute could not be read — unknown, not
/// "not minimized".
struct AxWindowRecord {
    window_id: u32,
    minimized: Option<bool>,
    /// The application's own report that this window blocks every other
    /// window of its process (`AXModal`). Unreadable is not modal.
    modal: bool,
    system_sharing_overlay: bool,
    hosted_panel: Option<(i32, u32)>,
    role: SurfaceRole,
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

/// What the application published this surface as: the one AX fact that
/// separates a rival keyboard destination from a control the application draws
/// in its own window. An unreadable role proves nothing, so it counts as
/// independent.
unsafe fn surface_role(window: AXUIElementRef) -> SurfaceRole {
    match copy_string_attr(window, "AXRole").as_deref() {
        Some("AXWindow" | "AXSheet") | None => SurfaceRole::Independent,
        Some(_) => SurfaceRole::HostedControl,
    }
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
                    modal: copy_bool_attr(window, "AXModal") == Some(true),
                    system_sharing_overlay: window_id != target_window_id
                        && minimized != Some(true)
                        && is_system_sharing_overlay(window, window_id, pid),
                    hosted_panel: hosted_panel_window(window),
                    role: surface_role(window),
                }
            });
            CFRelease(window as CFTypeRef);
            record
        })
        .collect()
}

/// The frame a WindowServer row is drawn in.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Frame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Frame {
    /// Whether this frame is drawn entirely inside `outer`. A degenerate frame
    /// is inside nothing: a surface with no area has no place of its own.
    fn inside(&self, outer: &Frame) -> bool {
        self.width > 0.0
            && self.height > 0.0
            && self.x >= outer.x
            && self.y >= outer.y
            && self.x + self.width <= outer.x + outer.width
            && self.y + self.height <= outer.y + outer.height
    }
}

/// One WindowServer row, reduced to the facts the ownership rule reads.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfaceRow {
    pid: i32,
    window_id: u32,
    frame: Frame,
}

impl From<&WindowInfo> for SurfaceRow {
    fn from(window: &WindowInfo) -> Self {
        Self {
            pid: window.pid,
            window_id: window.window_id,
            frame: Frame {
                x: window.bounds.x,
                y: window.bounds.y,
                width: window.bounds.width,
                height: window.bounds.height,
            },
        }
    }
}

/// Whether `row` is a surface the target window owns rather than a keyboard
/// destination of its own.
///
/// Two facts, both required. Accessibility says the surface is not independent:
/// either the application never published it at all, or it published it as a
/// control rather than as an `AXWindow`/`AXSheet` (see [`SurfaceRole`]) — so
/// nothing can address, activate or make it key. Geometry says which window
/// owns it: drawn entirely inside the requested window's frame. Without the
/// geometry the rule would hand a second document window's inline editor to
/// whichever window the caller happened to address.
fn is_owned_child_surface(
    row: &SurfaceRow,
    target_window_id: u32,
    target_frame: &Frame,
    ax_records: &[AxWindowRecord],
) -> bool {
    row.window_id != target_window_id
        && ax_records
            .iter()
            .find(|record| record.window_id == row.window_id)
            .is_none_or(|record| record.role == SurfaceRole::HostedControl)
        && row.frame.inside(target_frame)
}

/// The CGWindowID of the target window's own child surface that currently
/// holds the keyboard, if there is one.
///
/// macOS apps edit in place in a separate WindowServer window: Finder's inline
/// rename editor, an autocomplete drop-down, a combo popup. Measured on
/// Finder's rename editor, this is what the application reports while one is
/// up: `AXFocusedWindow` is unreadable, so `focused_window_id_of_pid` answers
/// `None`; the focused element has no `AXWindow` and no `AXTopLevelUIElement`
/// attribute and its `AXParent` is the application; and the child's CGWindowID
/// appears in `AXWindows` as the focused `AXTextField` itself. The keyboard is
/// therefore already inside the requested window, in a surface that cannot be
/// named — and the only thing an activation can do to it is dismiss it.
///
/// Blocking: one CGWindowList enumeration plus bounded AX reads. `None`
/// whenever any fact is missing, including a requested window accessibility
/// does not publish: ownership is proven, never assumed.
pub fn focused_owned_child_surface(pid: i32, target_window_id: u32) -> Option<u32> {
    let windows = all_windows();
    let rows: Vec<SurfaceRow> = windows
        .iter()
        .filter(|window| window.pid == pid)
        .map(SurfaceRow::from)
        .collect();
    let target_frame = rows
        .iter()
        .find(|row| row.window_id == target_window_id)?
        .frame;
    // SAFETY: the application element and every surface it hands back are
    // created and released inside this block, and the focused element is
    // released before its mapped id is used.
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let records = ax_window_records(app, pid, target_window_id);
        CFRelease(app as CFTypeRef);
        // A surface can only be owned by a window accessibility knows about.
        if !records
            .iter()
            .any(|record| record.window_id == target_window_id)
        {
            return None;
        }
        let focused = focused_element_of_pid(pid)?;
        let focused_window = ax_get_window_id(focused);
        CFRelease(focused as CFTypeRef);
        let focused_window = focused_window?;
        rows.iter()
            .find(|row| {
                row.window_id == focused_window
                    && is_owned_child_surface(row, target_window_id, &target_frame, &records)
            })
            .map(|row| row.window_id)
    }
}

/// Count independently AX-mapped, non-minimized sibling top-level windows.
///
/// WindowServer may expose several layer-0 compositor surfaces for one native
/// Electron, Tauri, or WebKit window. A raw same-pid CGWindow row is therefore
/// not enough to prove another process-scoped keyboard destination. Requiring a
/// fresh `AXWindows` mapping preserves the fail-closed two-window guard while
/// ignoring render surfaces that cannot independently become the AX key window.
///
/// A mapped row can still be the target's own surface rather than a rival: an
/// application that publishes its inline editor in `AXWindows` as a control
/// (see [`is_owned_child_surface`]) has not gained a second keyboard
/// destination. A sheet has: it keeps its window role and stays counted.
fn count_competing_keyboard_destinations(
    pid: i32,
    target_window_id: u32,
    window_server_rows: &[SurfaceRow],
    ax_records: &[AxWindowRecord],
) -> usize {
    let target_frame = window_server_rows
        .iter()
        .find(|row| row.window_id == target_window_id)
        .map(|row| row.frame);
    window_server_rows
        .iter()
        .filter(|row| {
            row.pid == pid
                && row.window_id != target_window_id
                && ax_records.iter().any(|record| {
                    record.window_id == row.window_id
                        && record.minimized != Some(true)
                        && !record.system_sharing_overlay
                })
                && !target_frame.is_some_and(|target_frame| {
                    is_owned_child_surface(row, target_window_id, &target_frame, ax_records)
                })
        })
        .count()
}

/// Whether `element`'s accessibility reference is no longer valid: the
/// application destroyed the object, so every attribute read answers
/// `kAXErrorInvalidUIElement` and no ancestry fact can exist for it.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn element_reference_invalid(element: AXUIElementRef) -> bool {
    matches!(
        try_copy_string_attr(element, "AXRole"),
        Err(super::bindings::kAXErrorInvalidUIElement)
    )
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
                if element_reference_invalid(element) {
                    return ElementAncestry::Gone;
                }
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
                    // The dialog blocking the requested window: its rows are
                    // in the blocked window's observation, and a semantic
                    // action on one is how the block is lifted.
                    Some(id)
                        if records
                            .iter()
                            .any(|record| record.window_id == id && record.modal) =>
                    {
                        ElementAncestry::ProvenAppModal { pid, window_id: id }
                    }
                    // The application's own editing surface: there is no
                    // window ancestry to walk, so ownership is proven through
                    // the surface the keyboard is in.
                    None if ax_get_window_id(element).is_some_and(|surface| {
                        focused_owned_child_surface(pid, window_id) == Some(surface)
                    }) =>
                    {
                        ElementAncestry::ProvenDescendant
                    }
                    resolved => classify_foreign_ancestry(element, pid, window_id, resolved),
                }
            });
            CFRelease(app as CFTypeRef);
            (records, app_hidden, element)
        }
    };

    let target = records.iter().find(|record| record.window_id == window_id);
    let rows: Vec<SurfaceRow> = all_windows().iter().map(SurfaceRow::from).collect();
    let competing_keyboard_destinations =
        count_competing_keyboard_destinations(pid, window_id, &rows, &records);

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
    use super::{
        count_competing_keyboard_destinations, matches_hosted_panel, AxWindowRecord, Frame,
        SurfaceRole, SurfaceRow,
    };

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
            modal: false,
            system_sharing_overlay: false,
            hosted_panel: None,
            role: SurfaceRole::Independent,
        }
    }

    /// A surface the application published as a control rather than a window —
    /// Finder's inline rename editor.
    fn hosted_control(window_id: u32) -> AxWindowRecord {
        AxWindowRecord {
            role: SurfaceRole::HostedControl,
            ..ax_window(window_id, None)
        }
    }

    fn frame(x: f64, y: f64, width: f64, height: f64) -> Frame {
        Frame {
            x,
            y,
            width,
            height,
        }
    }

    /// A row that overlaps no other row: these cases turn on AX mapping alone.
    fn row(pid: i32, window_id: u32) -> SurfaceRow {
        SurfaceRow {
            pid,
            window_id,
            frame: frame(f64::from(window_id) * 1000.0, 0.0, 100.0, 100.0),
        }
    }

    fn placed(pid: i32, window_id: u32, frame: Frame) -> SurfaceRow {
        SurfaceRow {
            pid,
            window_id,
            frame,
        }
    }

    #[test]
    fn compositor_surfaces_do_not_create_keyboard_ambiguity() {
        let rows = [
            row(42, 10),
            row(42, 11),
            row(42, 12),
            row(42, 13),
            row(42, 14),
            row(42, 15),
        ];
        let records = [ax_window(10, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            0
        );
    }

    #[test]
    fn independently_mapped_sibling_remains_ambiguous() {
        let rows = [row(42, 10), row(42, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            1
        );
    }

    #[test]
    fn proven_sharing_overlay_is_ignored_without_hiding_a_real_sibling() {
        let rows = [row(42, 10), row(42, 11), row(42, 12)];
        let mut overlay = ax_window(11, Some(false));
        overlay.system_sharing_overlay = true;
        assert_eq!(
            count_competing_keyboard_destinations(
                42,
                10,
                &rows,
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
                &rows,
                &[ax_window(10, Some(false)), overlay, ax_window(12, None)],
            ),
            1
        );
    }

    #[test]
    fn minimized_mapped_sibling_is_not_a_keyboard_destination() {
        let rows = [row(42, 10), row(42, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(true))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            0
        );
    }

    #[test]
    fn unmapped_window_server_sibling_is_not_a_keyboard_destination() {
        let rows = [row(42, 10), row(42, 99), row(7, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            0
        );
    }

    /// An inline editor the application published in `AXWindows` as its own
    /// text field, drawn inside the window it edits, is that window's surface.
    #[test]
    fn published_control_inside_the_target_is_not_a_second_destination() {
        let rows = [
            placed(42, 10, frame(400.0, 200.0, 920.0, 920.0)),
            placed(42, 11, frame(476.0, 308.0, 86.0, 23.0)),
        ];
        let records = [ax_window(10, Some(false)), hosted_control(11)];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            0
        );
    }

    /// A sheet is drawn inside its host too, and is still its own key window.
    #[test]
    fn sheet_drawn_inside_its_host_stays_a_keyboard_destination() {
        let rows = [
            placed(42, 10, frame(400.0, 200.0, 920.0, 920.0)),
            placed(42, 11, frame(500.0, 220.0, 500.0, 200.0)),
        ];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            1
        );
    }

    /// Ownership is proven, not guessed: a control surface the target does not
    /// enclose belongs to some other window of the same application.
    #[test]
    fn published_control_outside_the_target_still_counts() {
        let rows = [
            placed(42, 10, frame(400.0, 200.0, 920.0, 920.0)),
            placed(42, 11, frame(1400.0, 308.0, 86.0, 23.0)),
        ];
        let records = [ax_window(10, Some(false)), hosted_control(11)];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            1
        );
    }

    /// A surface with no area has no place of its own, so enclosure proves
    /// nothing about it.
    #[test]
    fn zero_sized_published_control_is_not_owned() {
        let rows = [
            placed(42, 10, frame(400.0, 200.0, 920.0, 920.0)),
            placed(42, 11, frame(476.0, 308.0, 0.0, 0.0)),
        ];
        let records = [ax_window(10, Some(false)), hosted_control(11)];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            1
        );
    }

    /// The requested window is not in the enumeration (closed under us), so no
    /// frame can own anything: every mapped sibling keeps counting.
    #[test]
    fn missing_target_row_owns_nothing() {
        let rows = [placed(42, 11, frame(476.0, 308.0, 86.0, 23.0))];
        let records = [ax_window(10, Some(false)), hosted_control(11)];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, &rows, &records),
            1
        );
    }
}
