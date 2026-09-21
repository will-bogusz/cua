//! Raw FFI bindings to the macOS Accessibility API (AXUIElement).
//!
//! We call the C-level AX API directly rather than using a crate wrapper,
//! because most available crates are incomplete or unmaintained.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]

use core_foundation::{
    array::CFArrayRef,
    base::{CFRelease, CFRetain, CFTypeID, CFTypeRef},
    string::CFStringRef,
};
use std::os::raw::{c_int, c_void};

// ── AXUIElement opaque type ──────────────────────────────────────────────────

#[repr(C)]
pub struct __AXUIElement(c_void);
pub type AXUIElementRef = *mut __AXUIElement;

// ── AXError ──────────────────────────────────────────────────────────────────

pub type AXError = c_int;
pub const kAXErrorSuccess: AXError = 0;
pub const kAXErrorFailure: AXError = -25200;
pub const kAXErrorInvalidUIElement: AXError = -25202;
pub const kAXErrorAttributeUnsupported: AXError = -25205;
pub const kAXErrorActionUnsupported: AXError = -25206;
pub const kAXErrorNoValue: AXError = -25212;
pub const kAXErrorAPIDisabled: AXError = -25211;

// ── AXValue opaque type ──────────────────────────────────────────────────────

#[repr(C)]
pub struct __AXValue(c_void);
pub type AXValueRef = *mut __AXValue;

pub type AXValueType = c_int;
pub const kAXValueCGPointType: AXValueType = 1;
pub const kAXValueCGSizeType: AXValueType = 2;
pub const kAXValueCGRectType: AXValueType = 3;
pub const kAXValueCFRangeType: AXValueType = 4;
pub const kAXValueIllegalType: AXValueType = 1_000;

// ── Link to AXUIElement functions ────────────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    #[link_name = "AXUIElementCopyAttributeValue"]
    fn AXUIElementCopyAttributeValue_native(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    pub fn AXUIElementCopyAttributeNames(
        element: AXUIElementRef,
        names: *mut CFArrayRef,
    ) -> AXError;
    #[link_name = "AXUIElementGetAttributeValueCount"]
    fn AXUIElementGetAttributeValueCount_native(
        element: AXUIElementRef,
        attribute: CFStringRef,
        count: *mut isize,
    ) -> AXError;
    #[link_name = "AXUIElementCopyActionNames"]
    fn AXUIElementCopyActionNames_native(
        element: AXUIElementRef,
        names: *mut CFArrayRef,
    ) -> AXError;
    pub fn AXUIElementCopyElementAtPosition(
        application: AXUIElementRef,
        x: f32,
        y: f32,
        element: *mut AXUIElementRef,
    ) -> AXError;
    pub fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    #[link_name = "AXUIElementSetAttributeValue"]
    fn AXUIElementSetAttributeValue_native(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    #[link_name = "AXUIElementIsAttributeSettable"]
    fn AXUIElementIsAttributeSettable_native(
        element: AXUIElementRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> AXError;
    pub fn AXUIElementSetMessagingTimeout(
        element: AXUIElementRef,
        timeout_in_seconds: f32,
    ) -> AXError;
    pub fn AXUIElementGetTypeID() -> CFTypeID;
    pub fn AXIsProcessTrusted() -> bool;
    /// `AXIsProcessTrustedWithOptions(options)` — when called with
    /// `{kAXTrustedCheckOptionPrompt: true}` raises the system Accessibility
    /// prompt if the process isn't already trusted.  Returns the post-prompt
    /// trust state (may still be false if the user dismissed the prompt).
    pub fn AXIsProcessTrustedWithOptions(
        options: core_foundation::dictionary::CFDictionaryRef,
    ) -> bool;

    /// Private SPI: maps an AX window element to its CGWindowID.
    /// Stable since macOS 10.9; used by yabai, Hammerspoon, Accessibility Inspector.
    #[link_name = "_AXUIElementGetWindow"]
    fn _AXUIElementGetWindow_native(element: AXUIElementRef, window_id: *mut u32) -> AXError;
}

/// Deadline-aware AX call; pointers follow the native API lifetime contract.
///
/// # Safety
/// All input and output pointers must be valid for this native request.
pub unsafe fn AXUIElementCopyAttributeValue(
    element: AXUIElementRef,
    attribute: CFStringRef,
    value: *mut CFTypeRef,
) -> AXError {
    super::budget::request(element, || {
        AXUIElementCopyAttributeValue_native(element, attribute, value)
    })
}

/// Deadline-aware AX call; pointers follow the native API lifetime contract.
///
/// # Safety
/// All input and output pointers must be valid for this native request.
pub unsafe fn AXUIElementCopyActionNames(
    element: AXUIElementRef,
    names: *mut CFArrayRef,
) -> AXError {
    super::budget::request(element, || {
        AXUIElementCopyActionNames_native(element, names)
    })
}

/// Deadline-aware AX call; pointers follow the native API lifetime contract.
///
/// # Safety
/// All input and output pointers must be valid for this native request.
pub unsafe fn AXUIElementSetAttributeValue(
    element: AXUIElementRef,
    attribute: CFStringRef,
    value: CFTypeRef,
) -> AXError {
    super::budget::request(element, || {
        AXUIElementSetAttributeValue_native(element, attribute, value)
    })
}

/// Deadline-aware AX call; pointers follow the native API lifetime contract.
///
/// # Safety
/// All input and output pointers must be valid for this native request.
pub unsafe fn AXUIElementIsAttributeSettable(
    element: AXUIElementRef,
    attribute: CFStringRef,
    settable: *mut u8,
) -> AXError {
    super::budget::request(element, || {
        AXUIElementIsAttributeSettable_native(element, attribute, settable)
    })
}

/// Deadline-aware AX call; pointers follow the native API lifetime contract.
///
/// # Safety
/// All input and output pointers must be valid for this native request.
pub unsafe fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> AXError {
    super::budget::request(element, || _AXUIElementGetWindow_native(element, window_id))
}

/// Read a hosted element's backing PID, not its presenter's PID.
///
/// The private SPI is optional: unavailable symbols and failed/unknown identity
/// reads return `None`, never a host-PID or debug-description fallback.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`.
pub unsafe fn actual_pid_of_element(element: AXUIElementRef) -> Option<i32> {
    type GetActualPid = unsafe extern "C" fn(AXUIElementRef, *mut i32) -> AXError;
    static GET_ACTUAL_PID: std::sync::LazyLock<Option<GetActualPid>> =
        std::sync::LazyLock::new(|| unsafe {
            let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"_AXUIElementGetActualPid".as_ptr());
            if symbol.is_null() {
                None
            } else {
                Some(std::mem::transmute::<*mut c_void, GetActualPid>(symbol))
            }
        });
    let get_pid = (*GET_ACTUAL_PID)?;
    let mut pid = 0;
    (get_pid(element, &mut pid) == kAXErrorSuccess && pid > 0).then_some(pid)
}

/// Hit-test one process's accessibility tree at a screen point. The returned
/// element is retained and must be released by the caller.
///
/// # Safety
///
/// The caller must release any returned element exactly once with `CFRelease`.
pub unsafe fn element_at_screen_position(pid: i32, x: f64, y: f64) -> Option<AXUIElementRef> {
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return None;
    }
    let mut element = std::ptr::null_mut();
    let error = AXUIElementCopyElementAtPosition(application, x as f32, y as f32, &mut element);
    CFRelease(application as CFTypeRef);
    (error == kAXErrorSuccess && !element.is_null()).then_some(element)
}

/// How far the menu search climbs before giving up. Menus nest a few levels
/// (submenu → item → menu); anything deeper is not a menu chain.
const MENU_ANCESTRY_DEPTH: usize = 12;

/// The root `AXMenu` open at a screen point, when the walk of `window_id`
/// would not reach it anyway.
///
/// An open `NSMenu` is a window of the application in its own right. AppKit
/// exposes it as a child of the control that opened it only sometimes — a
/// menu popped up detached, or opened from a control in a surface that is
/// not AX-mapped to the observed window, has no edge into that window's
/// subtree at all, so a window-scoped walk cannot see it while the user
/// plainly can. Hit-testing the menu's own window and climbing to the
/// outermost `AXMenu` is the route that does not depend on that edge.
///
/// `None` when the point is over no menu, or over one the walk already
/// renders: a menu whose ancestry reaches the requested window, or the menu
/// bar, is reached by the ordinary descent and must not be walked twice.
///
/// # Safety
///
/// The caller must release any returned element exactly once with `CFRelease`.
pub unsafe fn copy_unreachable_menu_root_at(
    pid: i32,
    window_id: Option<u32>,
    x: f64,
    y: f64,
) -> Option<AXUIElementRef> {
    let mut current = element_at_screen_position(pid, x, y)?;
    let mut menu: Option<AXUIElementRef> = None;
    let mut reachable = false;
    for _ in 0..MENU_ANCESTRY_DEPTH {
        let role = copy_string_attr(current, "AXRole").unwrap_or_default();
        match role.as_str() {
            "AXMenu" => {
                // Keep climbing: a submenu's root is the menu above it, and
                // the outermost one is what the caller can render.
                if let Some(inner) = menu.replace(current) {
                    CFRelease(inner as CFTypeRef);
                }
                CFRetain(current as CFTypeRef);
            }
            "AXMenuBar" => reachable = true,
            "AXWindow" | "AXSheet" => {
                reachable = reachable
                    || (window_id.is_some() && ax_get_window_id(current) == window_id);
            }
            _ => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        CFRelease(current as CFTypeRef);
        match parent {
            Some(parent) => current = parent,
            None => {
                current = std::ptr::null_mut();
                break;
            }
        }
    }
    if !current.is_null() {
        CFRelease(current as CFTypeRef);
    }
    let menu = menu?;
    if reachable {
        CFRelease(menu as CFTypeRef);
        return None;
    }
    Some(menu)
}

// ── AXValue functions ────────────────────────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXValueCreate(the_type: AXValueType, value_ptr: *const c_void) -> AXValueRef;
    pub fn AXValueGetType(value: AXValueRef) -> AXValueType;
    pub fn AXValueGetValue(
        value: AXValueRef,
        the_type: AXValueType,
        value_ptr: *mut c_void,
    ) -> bool;
}

#[repr(C)]
struct CGPointValue {
    x: f64,
    y: f64,
}

#[repr(C)]
struct CGSizeValue {
    width: f64,
    height: f64,
}

#[repr(C)]
struct CFRangeValue {
    location: isize,
    length: isize,
}

// ── Helper functions ──────────────────────────────────────────────────────────

use core_foundation::{array::CFArray, base::TCFType, string::CFString as CFStr};

/// Whether an AX attribute is currently writable on this element.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn is_attribute_settable(element: AXUIElementRef, attr_name: &str) -> bool {
    let attr = CFStr::new(attr_name);
    let mut settable = 0_u8;
    AXUIElementIsAttributeSettable(element, attr.as_concrete_TypeRef(), &mut settable)
        == kAXErrorSuccess
        && settable != 0
}

/// Copy a string attribute from an AX element. Returns `None` on any error.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn copy_string_attr(element: AXUIElementRef, attr_name: &str) -> Option<String> {
    try_copy_string_attr(element, attr_name).unwrap_or_default()
}

/// Copy a string attribute, reporting the AX error instead of collapsing it.
///
/// `Err` carries the code the framework returned, which is the only way to
/// tell a dead element (`kAXErrorInvalidUIElement`) from an attribute this
/// element simply does not publish. `Ok(None)` means the read succeeded and
/// the value was not a `CFString`.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
pub unsafe fn try_copy_string_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Result<Option<String>, AXError> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess {
        return Err(err);
    }
    if value.is_null() {
        return Ok(None);
    }
    let cf_string_type_id = CFStr::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_string_type_id {
        CFRelease(value);
        return Ok(None);
    }
    let s = CFStr::wrap_under_create_rule(value as _);
    Ok(Some(s.to_string()))
}

/// The element's display label under the tree's rule: `AXTitle`, else `AXDescription`, else `AXValue`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn copy_label_attr(element: AXUIElementRef) -> Option<String> {
    ["AXTitle", "AXDescription", "AXValue"]
        .into_iter()
        .find_map(|attr_name| {
            copy_string_attr(element, attr_name)
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty())
        })
}

/// Copy a numeric attribute from an AX element as an `f64`. Returns `None` on
/// any error or if the attribute is not a `CFNumber`. SwiftUI sliders expose a
/// readable numeric `AXValue` even when that value is not settable — this lets
/// the stepping fallback read the control's current position for feedback.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn copy_number_attr(element: AXUIElementRef, attr_name: &str) -> Option<f64> {
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let cf_number_type_id = CFNumber::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_number_type_id {
        CFRelease(value);
        return None;
    }
    let n = CFNumber::wrap_under_create_rule(value as _);
    n.to_f64()
}

/// Copy a boolean attribute from an AX element. Returns `None` on any error
/// or if the attribute is neither a `CFBoolean` nor a `CFNumber` (some apps
/// report AXEnabled/AXSelected as a 0/1 CFNumber instead of a CFBoolean).
///
/// # Safety
///
/// `element` must be a valid Accessibility object reference for the duration
/// of this call.
pub unsafe fn copy_bool_attr(element: AXUIElementRef, attr_name: &str) -> Option<bool> {
    use core_foundation::boolean::CFBoolean;
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFBoolean::type_id() {
        let b = CFBoolean::wrap_under_create_rule(value as _);
        return Some(b.into());
    }
    if type_id == CFNumber::type_id() {
        let n = CFNumber::wrap_under_create_rule(value as _);
        return n.to_f64().map(|f| f != 0.0);
    }
    CFRelease(value);
    None
}

unsafe fn coerce_binary_value(value: CFTypeRef) -> Option<bool> {
    use core_foundation::boolean::CFBoolean;
    use core_foundation::number::CFNumber;
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFBoolean::type_id() {
        return Some(CFBoolean::wrap_under_get_rule(value as _).into());
    }
    if type_id == CFNumber::type_id() {
        return match CFNumber::wrap_under_get_rule(value as _).to_f64()? {
            0.0 => Some(false),
            1.0 => Some(true),
            _ => None,
        };
    }
    None
}

pub unsafe fn copy_binary_attr(element: AXUIElementRef, attr_name: &str) -> Option<bool> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let result = coerce_binary_value(value);
    CFRelease(value);
    result
}

/// A copied AX attribute represented for both existing string-only consumers
/// and the wider structured control-state response.
#[derive(Debug, PartialEq, Eq)]
pub struct StringishAttrValue {
    /// Present when the value has a faithful textual form of its own: a
    /// CFString as-is, or a CFDate as its local ISO-8601 date-time. A number
    /// or a boolean has no text the application itself displays, so it stays
    /// out of the string-only surfaces.
    pub string_value: Option<String>,
    /// CFString as-is, CFDate as local ISO-8601, CFNumber as text, or
    /// CFBoolean as `"1"` / `"0"`.
    pub state_value: String,
}

/// CFDate's epoch — 2001-01-01 00:00:00 UTC — as Unix seconds.
const CF_ABSOLUTE_TIME_UNIX_EPOCH: i64 = 978_307_200;

/// Whole Unix seconds for a `CFAbsoluteTime`, floored so a fractional second
/// never reports the following one. `None` for a value no calendar can hold.
fn unix_seconds_for_absolute_time(absolute: f64) -> Option<i64> {
    let seconds = absolute.floor();
    if !seconds.is_finite() || seconds.abs() > 9.0e15 {
        return None;
    }
    Some(seconds as i64 + CF_ABSOLUTE_TIME_UNIX_EPOCH)
}

/// The instant a date-bearing control holds, rendered as the local wall-clock
/// date-time the control itself displays plus the UTC offset that makes it
/// unambiguous (ISO 8601 / RFC 3339).
///
/// `AXDateTimeArea`, `AXDateField` and `AXTimeField` publish their value only
/// as a `CFDate`: there is no `AXValue` string and commonly no
/// `AXValueDescription`, so without this the control renders with no value at
/// all and an agent cannot read the date it is about to change.
fn local_iso8601(unix_seconds: i64) -> Option<String> {
    let mut components: libc::tm = unsafe { std::mem::zeroed() };
    let seconds = unix_seconds as libc::time_t;
    if unsafe { libc::localtime_r(&seconds, &mut components) }.is_null() {
        return None;
    }
    Some(format_local_iso8601(&components))
}

fn format_local_iso8601(components: &libc::tm) -> String {
    let offset_minutes = components.tm_gmtoff / 60;
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let offset_minutes = offset_minutes.abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        components.tm_year + 1900,
        components.tm_mon + 1,
        components.tm_mday,
        components.tm_hour,
        components.tm_min,
        components.tm_sec,
        offset_minutes / 60,
        offset_minutes % 60
    )
}

/// Convert a borrowed CF value without taking ownership of it.
unsafe fn coerce_stringish_value(value: CFTypeRef) -> Option<StringishAttrValue> {
    use core_foundation::boolean::CFBoolean;
    use core_foundation::number::CFNumber;
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFStr::type_id() {
        let string = CFStr::wrap_under_get_rule(value as _).to_string();
        return Some(StringishAttrValue {
            string_value: Some(string.clone()),
            state_value: string,
        });
    }
    if type_id == CFNumber::type_id() {
        let n = CFNumber::wrap_under_get_rule(value as _);
        let f = n.to_f64()?;
        let state_value = if f == f.trunc() && f.abs() < 1e15 {
            format!("{}", f as i64)
        } else {
            format!("{f}")
        };
        return Some(StringishAttrValue {
            string_value: None,
            state_value,
        });
    }
    if type_id == core_foundation::date::CFDate::type_id() {
        let date = core_foundation::date::CFDate::wrap_under_get_rule(value as _);
        let rendered = unix_seconds_for_absolute_time(date.abs_time()).and_then(local_iso8601)?;
        return Some(StringishAttrValue {
            string_value: Some(rendered.clone()),
            state_value: rendered,
        });
    }
    if type_id == CFBoolean::type_id() {
        let b = CFBoolean::wrap_under_get_rule(value as _);
        return Some(StringishAttrValue {
            string_value: None,
            state_value: if bool::from(b) {
                "1".into()
            } else {
                "0".into()
            },
        });
    }
    None
}

/// Copy an attribute that may be a `CFString`, `CFNumber`, or `CFBoolean`.
///
/// The returned pair lets the tree walker preserve its historical CFString-only
/// markdown while using the same single AX read for structured control state.
/// Numbers render without a trailing `.0` when integral (`8`, not `8.0`), and
/// booleans render as `1`/`0` to match AppKit's two-state controls.
///
/// # Safety
///
/// `element` must be a valid Accessibility object reference for the duration
/// of this call.
pub unsafe fn copy_stringish_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Option<StringishAttrValue> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let result = coerce_stringish_value(value);
    CFRelease(value);
    result
}

/// Get the action names for an AX element.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn copy_action_names(element: AXUIElementRef) -> Vec<String> {
    let mut names: CFArrayRef = std::ptr::null_mut();
    let err = AXUIElementCopyActionNames(element, &mut names);
    if err != kAXErrorSuccess || names.is_null() {
        return vec![];
    }
    // Use CFArray<CFStr> (the typed wrapper) to satisfy FromVoid bound.
    let arr = CFArray::<CFStr>::wrap_under_create_rule(names);
    (0..arr.len())
        .filter_map(|i| {
            let cf = arr.get(i)?;
            Some(cf.to_string())
        })
        .collect()
}

/// Read the on-screen center of an AX element (AXPosition + AXSize → center).
/// Returns `(cx, cy)` in screen coordinates, or `None` if either attribute
/// is unavailable or the element has zero size.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn element_screen_center(element: AXUIElementRef) -> Option<(f64, f64)> {
    // AXPosition → CGPoint
    let pos_attr = CFStr::new("AXPosition");
    let mut pos_ref: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, pos_attr.as_concrete_TypeRef(), &mut pos_ref);
    if err != kAXErrorSuccess || pos_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut pos = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        pos_ref as AXValueRef,
        kAXValueCGPointType,
        &mut pos as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(pos_ref);
    if !ok {
        return None;
    }

    // AXSize → CGSize
    let sz_attr = CFStr::new("AXSize");
    let mut sz_ref: CFTypeRef = std::ptr::null();
    let err2 = AXUIElementCopyAttributeValue(element, sz_attr.as_concrete_TypeRef(), &mut sz_ref);
    if err2 != kAXErrorSuccess || sz_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let mut sz = CGSize { w: 0.0, h: 0.0 };
    let ok2 = AXValueGetValue(
        sz_ref as AXValueRef,
        kAXValueCGSizeType,
        &mut sz as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(sz_ref);
    if !ok2 || sz.w < 1.0 || sz.h < 1.0 {
        return None;
    }

    Some((pos.x + sz.w / 2.0, pos.y + sz.h / 2.0))
}

/// Read the on-screen bounding rect of an AX element.
/// Returns `[x, y, width, height]` in screen coordinates (top-left origin), or `None`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn element_screen_rect(element: AXUIElementRef) -> Option<[f64; 4]> {
    // AXPosition → CGPoint
    let pos_attr = CFStr::new("AXPosition");
    let mut pos_ref: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, pos_attr.as_concrete_TypeRef(), &mut pos_ref);
    if err != kAXErrorSuccess || pos_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut pos = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        pos_ref as AXValueRef,
        kAXValueCGPointType,
        &mut pos as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(pos_ref);
    if !ok {
        return None;
    }

    // AXSize → CGSize
    let sz_attr = CFStr::new("AXSize");
    let mut sz_ref: CFTypeRef = std::ptr::null();
    let err2 = AXUIElementCopyAttributeValue(element, sz_attr.as_concrete_TypeRef(), &mut sz_ref);
    if err2 != kAXErrorSuccess || sz_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let mut sz = CGSize { w: 0.0, h: 0.0 };
    let ok2 = AXValueGetValue(
        sz_ref as AXValueRef,
        kAXValueCGSizeType,
        &mut sz as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(sz_ref);
    if !ok2 || sz.w < 1.0 || sz.h < 1.0 {
        return None;
    }

    Some([pos.x, pos.y, sz.w, sz.h])
}

/// Get the focused UI element of a running application by pid.
/// Returns a retained `AXUIElementRef` that the caller must release, or `None`.
///
/// # Safety
///
/// The caller must release any returned element exactly once with `CFRelease`.
pub unsafe fn focused_element_of_pid(pid: i32) -> Option<AXUIElementRef> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let attr = CFStr::new("AXFocusedUIElement");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(app, attr.as_concrete_TypeRef(), &mut value);
    CFRelease(app as CFTypeRef);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let ax_type_id = AXUIElementGetTypeID();
    if core_foundation::base::CFGetTypeID(value) != ax_type_id {
        CFRelease(value);
        return None;
    }
    // Already retained by CopyAttributeValue — hand the raw pointer to the caller.
    Some(value as AXUIElementRef)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedWindow {
    pub window_id: Option<u32>,
    pub role: Option<String>,
}

pub fn focused_window_of_pid(pid: i32) -> Option<FocusedWindow> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let window = copy_element_attr(app, "AXFocusedWindow");
        CFRelease(app as CFTypeRef);
        let window = window?;
        let focused = FocusedWindow {
            window_id: ax_get_window_id(window),
            role: copy_string_attr(window, "AXRole"),
        };
        CFRelease(window as CFTypeRef);
        Some(focused)
    }
}

/// Return the CGWindowID of the application's focused AX window.
///
/// This is a narrow read-only proof used before global keyboard delivery: an
/// already focused exact window must not be re-activated, because doing so can
/// make a focus-proxy renderer drop its current key target.
pub fn focused_window_id_of_pid(pid: i32) -> Option<u32> {
    focused_window_of_pid(pid).and_then(|focused| focused.window_id)
}

/// Ask AppKit to make exactly one of `pid`'s windows key and main: `AXRaise`,
/// then `AXMain` and `AXFocused` on the AXWindow that reports `window_id`.
/// Addresses only that window; it never orders the process's other windows.
///
/// This is the half of the exact-window sequence that moves key status
/// *between sibling windows*. The SkyLight make-key records
/// ([`crate::input::skylight::make_exact_window_key`]) make an inactive
/// application install its remembered key window on activation, but an
/// application that is already frontmost with another of its windows key
/// keeps that window until AppKit itself is asked to raise the target.
///
/// Returns whether any of the three requests was accepted; the caller's
/// postcondition read remains authoritative.
pub fn raise_exact_ax_window(pid: i32, window_id: u32) -> bool {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        let mut target = None;
        for window in copy_ax_windows(app) {
            if target.is_none() && ax_get_window_id(window) == Some(window_id) {
                target = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
        CFRelease(app as CFTypeRef);
        let Some(target) = target else {
            return false;
        };
        let raised = perform_action(target, "AXRaise") == 0;
        let main = set_bool_attr_true(target, "AXMain") == 0;
        let focused = set_bool_attr_true(target, "AXFocused") == 0;
        CFRelease(target as CFTypeRef);
        raised || main || focused
    }
}

/// How many children an element reports, without copying or retaining any of
/// them. One native request and no allocation, so it is cheap enough to
/// sample repeatedly.
///
/// `None` when the count could not be read at all — including an element that
/// does not carry `AXChildren`, which is indistinguishable here from one whose
/// read failed and must therefore not be reported as "no children".
///
/// # Safety
///
/// `element` must be valid.
pub unsafe fn children_count(element: AXUIElementRef) -> Option<usize> {
    let attr = CFStr::new("AXChildren");
    let mut count: isize = 0;
    let err = super::budget::request(element, || {
        AXUIElementGetAttributeValueCount_native(element, attr.as_concrete_TypeRef(), &mut count)
    });
    if err != kAXErrorSuccess || count < 0 {
        return None;
    }
    Some(count as usize)
}

/// Get the children of an AX element.
///
/// # Safety
///
/// `element` must be valid, and the caller must release every returned element.
pub unsafe fn copy_children(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    copy_children_checked(element).0
}

/// [`copy_children`], plus whether the read hid children rather than proving
/// there are none.
///
/// An element with no children answers with an empty array, with
/// `kAXErrorAttributeUnsupported`, or with `kAXErrorNoValue`. Every other
/// outcome leaves an unknown number of descendants unseen.
///
/// # Safety
///
/// `element` must be valid, and the caller must release every returned element.
pub unsafe fn copy_children_checked(element: AXUIElementRef) -> (Vec<AXUIElementRef>, bool) {
    let attr = CFStr::new("AXChildren");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess {
        return (vec![], children_read_hid_descendants(err));
    }
    if value.is_null() {
        return (vec![], true);
    }
    let cf_array_type_id = CFArray::<CFTypeRef>::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_array_type_id {
        CFRelease(value);
        return (vec![], true);
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    let children: Vec<AXUIElementRef> = (0..arr.len())
        .filter_map(|i| {
            let item = *arr.get(i)?;
            if core_foundation::base::CFGetTypeID(item) == ax_type_id {
                // Retain so we own it — caller is responsible for releasing.
                CFRetain(item);
                Some(item as AXUIElementRef)
            } else {
                None
            }
        })
        .collect();
    let dropped = children.len() as isize != arr.len();
    (children, dropped)
}

/// # Safety
///
/// `element` must be valid, and the caller must release every returned element.
pub unsafe fn copy_element_array_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Option<Vec<AXUIElementRef>> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    if core_foundation::base::CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
        CFRelease(value);
        return None;
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    Some(
        (0..arr.len())
            .filter_map(|i| {
                let item = *arr.get(i)?;
                if core_foundation::base::CFGetTypeID(item) == ax_type_id {
                    CFRetain(item);
                    Some(item as AXUIElementRef)
                } else {
                    None
                }
            })
            .collect(),
    )
}

/// # Safety
///
/// Each element must be owned by the caller exactly once.
pub unsafe fn release_all(elements: Vec<AXUIElementRef>) {
    for element in elements {
        CFRelease(element as CFTypeRef);
    }
}

/// Whether an `AXChildren` error code leaves the child list unknown, as
/// opposed to establishing that the element has no children.
fn children_read_hid_descendants(err: AXError) -> bool {
    !matches!(
        err,
        kAXErrorSuccess | kAXErrorAttributeUnsupported | kAXErrorNoValue
    )
}

/// Copy a child only when AXChildren is exactly one valid AX element.
///
/// Failed or malformed reads return `None`, rather than filtering unusable
/// entries. The caller must release the returned element.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`.
pub unsafe fn copy_only_child(element: AXUIElementRef) -> Option<AXUIElementRef> {
    let attr = CFStr::new("AXChildren");
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error != kAXErrorSuccess || value.is_null() {
        return None;
    }
    if core_foundation::base::CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
        CFRelease(value);
        return None;
    }
    let children = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    if children.len() != 1 {
        return None;
    }
    let child = *children.get(0)?;
    if core_foundation::base::CFGetTypeID(child) != AXUIElementGetTypeID() {
        return None;
    }
    CFRetain(child);
    Some(child as AXUIElementRef)
}

/// Copy an AX element-valued attribute. The returned element is retained and
/// must be released by the caller.
///
/// # Safety
///
/// `element` must be valid, and the caller must release any returned element.
pub unsafe fn copy_element_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Option<AXUIElementRef> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    if core_foundation::base::CFGetTypeID(value) != AXUIElementGetTypeID() {
        CFRelease(value);
        return None;
    }
    Some(value as AXUIElementRef)
}

/// Perform an AX action using a string attribute name.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn perform_action(element: AXUIElementRef, action_name: &str) -> AXError {
    let action = CFStr::new(action_name);
    AXUIElementPerformAction(element, action.as_concrete_TypeRef())
}

/// Set an AX attribute to a CFString value.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_string_attr(element: AXUIElementRef, attr_name: &str, value: &str) -> AXError {
    let attr = CFStr::new(attr_name);
    let cf_value = CFStr::new(value);
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_value.as_CFTypeRef())
}

/// Set an AX attribute to a CFNumber (double) value. Numeric controls — most
/// notably `AXSlider` (NSSlider) and `AXStepper` — expose a numeric `AXValue`
/// reject a `CFString` write — `-25200` (kAXErrorFailure, observed live on a
/// SwiftUI `AXSlider`) or `-25201` (kAXErrorIllegalArgument); only a `CFNumber`
/// is accepted. Text fields, by contrast, take a `CFString`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_number_attr(element: AXUIElementRef, attr_name: &str, value: f64) -> AXError {
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let cf_value = CFNumber::from(value);
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_value.as_CFTypeRef())
}

/// Set an AX CGPoint attribute such as `AXPosition`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_point_attr(element: AXUIElementRef, attr_name: &str, x: f64, y: f64) -> AXError {
    let attr = CFStr::new(attr_name);
    let point = CGPointValue { x, y };
    let value = AXValueCreate(
        kAXValueCGPointType,
        &point as *const CGPointValue as *const c_void,
    );
    if value.is_null() {
        return kAXErrorFailure;
    }
    let result =
        AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value as CFTypeRef);
    CFRelease(value as CFTypeRef);
    result
}

/// Set an AX CGSize attribute such as `AXSize`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_size_attr(
    element: AXUIElementRef,
    attr_name: &str,
    width: f64,
    height: f64,
) -> AXError {
    let attr = CFStr::new(attr_name);
    let size = CGSizeValue { width, height };
    let value = AXValueCreate(
        kAXValueCGSizeType,
        &size as *const CGSizeValue as *const c_void,
    );
    if value.is_null() {
        return kAXErrorFailure;
    }
    let result =
        AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value as CFTypeRef);
    CFRelease(value as CFTypeRef);
    result
}

/// Set an AX CFRange attribute such as `AXSelectedTextRange`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_range_attr(
    element: AXUIElementRef,
    attr_name: &str,
    location: isize,
    length: isize,
) -> AXError {
    let attr = CFStr::new(attr_name);
    let range = CFRangeValue { location, length };
    let value = AXValueCreate(
        kAXValueCFRangeType,
        &range as *const CFRangeValue as *const c_void,
    );
    if value.is_null() {
        return kAXErrorFailure;
    }
    let result =
        AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value as CFTypeRef);
    CFRelease(value as CFTypeRef);
    result
}

/// Read an AX CFRange attribute such as `AXSelectedTextRange` as
/// `(location, length)`. Returns `None` when the attribute is missing or is
/// not a CFRange-typed `AXValue`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn copy_range_attr(element: AXUIElementRef, attr_name: &str) -> Option<(isize, isize)> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let mut range = CFRangeValue {
        location: 0,
        length: 0,
    };
    let ok = AXValueGetValue(
        value as AXValueRef,
        kAXValueCFRangeType,
        &mut range as *mut CFRangeValue as *mut c_void,
    );
    CFRelease(value);
    ok.then_some((range.location, range.length))
}

/// Set an AX attribute to a CFBoolean true value.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_bool_attr_true(element: AXUIElementRef, attr_name: &str) -> AXError {
    use core_foundation::boolean::CFBoolean;
    let attr = CFStr::new(attr_name);
    let cf_true = CFBoolean::true_value();
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_true.as_CFTypeRef())
}

/// Signal to a Chromium/Electron application root that a real assistive client
/// is present so it materializes its full web-content accessibility tree.
///
/// Returns `true` when an attribute write was accepted — meaning the app was
/// flipped from "tree off" to "tree building" and the caller should let the
/// tree settle before walking. Returns `false` when the app does not support
/// either attribute (native Cocoa apps such as Finder / Calculator / TextEdit),
/// in which case no settle delay is warranted.
///
/// `AXManualAccessibility` is the modern opt-in with no screen-reader side
/// effects; `AXEnhancedUserInterface` is the legacy fallback some Electron
/// builds expose instead (the modern attribute returns
/// `kAXErrorAttributeUnsupported` on those builds).
///
/// # Safety
///
/// `app_element` must be a valid, live application `AXUIElementRef`.
pub unsafe fn enable_chromium_accessibility(app_element: AXUIElementRef) -> bool {
    let manual = set_bool_attr_true(app_element, "AXManualAccessibility");
    if manual == kAXErrorSuccess {
        return true;
    }
    if manual != kAXErrorAttributeUnsupported {
        // A transient error (e.g. timeout / app busy) rather than a hard
        // "this app has no such attribute" — don't bother with the legacy
        // fallback, and don't claim enablement happened.
        return false;
    }
    set_bool_attr_true(app_element, "AXEnhancedUserInterface") == kAXErrorSuccess
}

/// Get the CGWindowID of an AX window element via the private `_AXUIElementGetWindow` SPI.
/// Returns `None` if the element is not a composited window.
///
/// # Safety
///
/// `element` must be a valid, live window `AXUIElementRef`.
pub unsafe fn ax_get_window_id(element: AXUIElementRef) -> Option<u32> {
    let mut wid: u32 = 0;
    let err = _AXUIElementGetWindow(element, &mut wid);
    if err == kAXErrorSuccess && wid != 0 {
        Some(wid)
    } else {
        None
    }
}

/// Read the `AXWindows` attribute of an application element.
/// Unlike `AXChildren`, this returns the window list regardless of whether
/// the app is frontmost. Returns a Vec of retained AXUIElementRefs.
///
/// # Safety
///
/// `element` must be valid, and the caller must release every returned element.
pub unsafe fn copy_ax_windows(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    copy_ax_windows_checked(element).unwrap_or_default()
}

/// Fresh AXWindows plus their explicitly attached AXSheet children.
///
/// AppKit file panels expose a same-process sheet below the document window,
/// not in AXWindows. Its mapped CGWindowID is the visible panel, while children
/// can be supplied by an XPC service. Do not substitute those provider windows
/// or arbitrary descendants for the sheet. Every returned element is retained.
/// The bounded walk follows only window -> sheet -> sheet edges.
///
/// # Safety
/// `element` must be valid; release every returned element exactly once.
pub unsafe fn copy_ax_window_surfaces(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    use core_foundation::base::CFEqual;
    let mut surfaces = copy_ax_windows(element);
    let mut depths = vec![0usize; surfaces.len()];
    let mut index = 0;
    while index < surfaces.len() && index < 64 {
        let parent = surfaces[index];
        let depth = depths[index];
        index += 1;
        if depth >= 4 {
            continue;
        }
        for child in copy_children(parent) {
            let is_sheet = copy_string_attr(child, "AXRole").as_deref() == Some("AXSheet");
            if surfaces.len() < 64
                && is_sheet
                && ax_get_window_id(child).is_some()
                && ax_get_window_id(child) != ax_get_window_id(parent)
                && !surfaces
                    .iter()
                    .any(|&other| CFEqual(other as CFTypeRef, child as CFTypeRef) != 0)
            {
                surfaces.push(child);
                depths.push(depth + 1);
            } else {
                CFRelease(child as CFTypeRef);
            }
        }
    }
    surfaces
}

/// Read the application window list without conflating failure with an empty list.
///
/// # Safety
/// `element` must be valid. The caller must release every returned element.
pub unsafe fn copy_ax_windows_checked(
    element: AXUIElementRef,
) -> Result<Vec<AXUIElementRef>, AXError> {
    let attr = CFStr::new("AXWindows");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess {
        if !value.is_null() {
            CFRelease(value);
        }
        return Err(err);
    }
    take_ax_window_array(value)
}

// Consumes the copied attribute value on every path. Do not treat a partially
// readable window array as a complete inventory.
unsafe fn take_ax_window_array(value: CFTypeRef) -> Result<Vec<AXUIElementRef>, AXError> {
    if value.is_null() {
        return Err(kAXErrorNoValue);
    }
    let cf_array_type_id = CFArray::<CFTypeRef>::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_array_type_id {
        CFRelease(value);
        return Err(kAXErrorFailure);
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    if arr
        .iter()
        .any(|item| item.is_null() || core_foundation::base::CFGetTypeID(*item) != ax_type_id)
    {
        return Err(kAXErrorFailure);
    }
    Ok(arr
        .iter()
        .map(|item| {
            CFRetain(*item);
            *item as AXUIElementRef
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::{boolean::CFBoolean, number::CFNumber};

    #[test]
    fn binary_value_accepts_booleans_and_exact_zero_or_one() {
        let true_value = CFBoolean::true_value();
        let false_value = CFBoolean::false_value();
        let zero = CFNumber::from(0.0);
        let one = CFNumber::from(1.0);
        let fractional = CFNumber::from(0.5);
        let other = CFNumber::from(2.0);
        let string = CFStr::new("1");

        assert_eq!(
            unsafe { coerce_binary_value(true_value.as_CFTypeRef()) },
            Some(true)
        );
        assert_eq!(
            unsafe { coerce_binary_value(false_value.as_CFTypeRef()) },
            Some(false)
        );
        assert_eq!(
            unsafe { coerce_binary_value(zero.as_CFTypeRef()) },
            Some(false)
        );
        assert_eq!(
            unsafe { coerce_binary_value(one.as_CFTypeRef()) },
            Some(true)
        );
        assert_eq!(
            unsafe { coerce_binary_value(fractional.as_CFTypeRef()) },
            None
        );
        assert_eq!(unsafe { coerce_binary_value(other.as_CFTypeRef()) }, None);
        assert_eq!(unsafe { coerce_binary_value(string.as_CFTypeRef()) }, None);
    }

    #[test]
    fn binary_value_rejects_near_binary_and_non_finite_numbers() {
        for value in [
            1e-20,
            -1e-20,
            f64::from_bits(1),
            -f64::from_bits(1),
            f64::from_bits(1.0_f64.to_bits() - 1),
            f64::from_bits(1.0_f64.to_bits() + 1),
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let number = CFNumber::from(value);
            assert_eq!(
                unsafe { coerce_binary_value(number.as_CFTypeRef()) },
                None,
                "unexpected binary state for {value:?}"
            );
        }
    }

    #[test]
    fn checked_window_array_distinguishes_empty_from_unavailable_or_malformed() {
        unsafe {
            let empty = CFArray::<CFTypeRef>::from_copyable(&[]);
            assert!(take_ax_window_array(CFRetain(empty.as_CFTypeRef()))
                .unwrap()
                .is_empty());
            assert_eq!(
                take_ax_window_array(std::ptr::null()).unwrap_err(),
                kAXErrorNoValue
            );
            let text = CFStr::new("not a window");
            assert_eq!(
                take_ax_window_array(CFRetain(text.as_CFTypeRef())).unwrap_err(),
                kAXErrorFailure
            );
            let app = AXUIElementCreateApplication(std::process::id() as i32);
            assert!(!app.is_null());
            let valid = CFArray::<CFTypeRef>::from_copyable(&[app as CFTypeRef]);
            let copied = take_ax_window_array(CFRetain(valid.as_CFTypeRef())).unwrap();
            assert_eq!(copied, vec![app]);
            for element in copied {
                CFRelease(element as CFTypeRef);
            }
            let mixed =
                CFArray::<CFTypeRef>::from_copyable(&[app as CFTypeRef, text.as_CFTypeRef()]);
            assert_eq!(
                take_ax_window_array(CFRetain(mixed.as_CFTypeRef())).unwrap_err(),
                kAXErrorFailure
            );
            CFRelease(app as CFTypeRef);
        }
    }

    #[test]
    fn stringish_value_coerces_cfstring_cfnumber_and_cfboolean() {
        let string = CFStr::new("Search");
        let integer = CFNumber::from(8.0);
        let decimal = CFNumber::from(2.5);
        let true_value = CFBoolean::true_value();
        let false_value = CFBoolean::false_value();

        let string_result = unsafe { coerce_stringish_value(string.as_CFTypeRef()) }.unwrap();
        assert_eq!(string_result.string_value.as_deref(), Some("Search"));
        assert_eq!(string_result.state_value, "Search");

        let integer_result = unsafe { coerce_stringish_value(integer.as_CFTypeRef()) }.unwrap();
        assert_eq!(integer_result.string_value, None);
        assert_eq!(integer_result.state_value, "8");

        let decimal_result = unsafe { coerce_stringish_value(decimal.as_CFTypeRef()) }.unwrap();
        assert_eq!(decimal_result.string_value, None);
        assert_eq!(decimal_result.state_value, "2.5");

        let true_result = unsafe { coerce_stringish_value(true_value.as_CFTypeRef()) }.unwrap();
        assert_eq!(true_result.string_value, None);
        assert_eq!(true_result.state_value, "1");

        let false_result = unsafe { coerce_stringish_value(false_value.as_CFTypeRef()) }.unwrap();
        assert_eq!(false_result.string_value, None);
        assert_eq!(false_result.state_value, "0");
    }

    /// A date-bearing control publishes its value only as a `CFDate`. The
    /// coerced form is the local wall-clock date-time the control displays,
    /// with its offset — and it counts as the value's own text, so the
    /// string-only tree row shows it too.
    #[test]
    fn stringish_value_coerces_a_cfdate_to_a_local_iso8601_date_time() {
        // 2026-09-25 00:00:00 UTC.
        let date = core_foundation::date::CFDate::new(1_790_640_000.0 - 978_307_200.0 + 0.0);
        let coerced = unsafe { coerce_stringish_value(date.as_CFTypeRef()) }.unwrap();
        assert_eq!(coerced.string_value.as_deref(), Some(&*coerced.state_value));
        let rendered = coerced.state_value;
        assert_eq!(
            rendered,
            local_iso8601(1_790_640_000).unwrap(),
            "the coerced value is the same instant the formatter renders"
        );
        assert!(
            rendered.len() == "2026-09-25T00:00:00+00:00".len()
                && rendered.as_bytes()[10] == b'T'
                && (rendered.contains('+') || rendered[1..].contains('-')),
            "not an offset-qualified ISO-8601 date-time: {rendered}"
        );
    }

    /// The wall-clock fields and the offset are rendered exactly, zero-padded,
    /// with the sign of a negative offset preserved: this string is what an
    /// agent reads a date picker's value from.
    #[test]
    fn a_local_date_time_renders_zero_padded_with_its_offset() {
        let mut components: libc::tm = unsafe { std::mem::zeroed() };
        components.tm_year = 126; // 2026
        components.tm_mon = 8; // September
        components.tm_mday = 5;
        components.tm_hour = 7;
        components.tm_min = 3;
        components.tm_sec = 9;
        components.tm_gmtoff = -7 * 3600;
        assert_eq!(
            format_local_iso8601(&components),
            "2026-09-05T07:03:09-07:00"
        );

        components.tm_gmtoff = 5 * 3600 + 30 * 60;
        assert_eq!(
            format_local_iso8601(&components),
            "2026-09-05T07:03:09+05:30"
        );
    }

    /// A `CFAbsoluteTime` is seconds since 2001, and a fractional second
    /// belongs to the second it is inside — never the next one.
    #[test]
    fn absolute_time_converts_on_the_cfdate_epoch_and_floors() {
        assert_eq!(unix_seconds_for_absolute_time(0.0), Some(978_307_200));
        assert_eq!(unix_seconds_for_absolute_time(0.99), Some(978_307_200));
        assert_eq!(unix_seconds_for_absolute_time(-1.0), Some(978_307_199));
        assert_eq!(unix_seconds_for_absolute_time(-0.25), Some(978_307_199));
        assert_eq!(unix_seconds_for_absolute_time(f64::NAN), None);
        assert_eq!(unix_seconds_for_absolute_time(f64::INFINITY), None);
    }

    /// An element that reports no children is not the same as an element
    /// whose children could not be read: only the second leaves a window
    /// partially enumerated.
    #[test]
    fn an_unsupported_child_list_is_not_a_hidden_one() {
        for reports_none in [
            kAXErrorSuccess,
            kAXErrorAttributeUnsupported,
            kAXErrorNoValue,
        ] {
            assert!(
                !children_read_hid_descendants(reports_none),
                "{reports_none}"
            );
        }
        for hides in [
            kAXErrorFailure,
            kAXErrorInvalidUIElement,
            kAXErrorAPIDisabled,
        ] {
            assert!(children_read_hid_descendants(hides), "{hides}");
        }
    }
}
