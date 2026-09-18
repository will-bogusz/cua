//! RAII ownership for one `AXUIElementRef`.
//!
//! Reading an element pointer whose last reference is gone is not a
//! recoverable AX error: `AXUIElementCopyAttributeValue` validates its
//! argument by calling `CFGetTypeID` on the allocation, so a freed element
//! traps (`EXC_BREAKPOINT` / `SIGTRAP`) inside CoreFoundation and takes the
//! driver process with it. Holding a reference of our own turns the same
//! situation into `kAXErrorInvalidUIElement`, which every AX reader here
//! already handles — a destroyed-but-retained element answers "I am invalid"
//! instead of corrupting the caller.
//!
//! Use this guard whenever an element pointer has to outlive the thing that
//! produced it: an app's focused element sampled at one instant, a pointer
//! borrowed out of an element-cache snapshot, or anything watched across an
//! action that may destroy it.

use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};

use super::bindings::AXUIElementRef;

/// A reference to an AX element, released exactly once on drop.
///
/// The pointer is stored as a `usize` so the guard is `Send`: CF reference
/// counting is thread-safe and this crate already moves element pointers into
/// `spawn_blocking` as integers.
pub struct OwnedElement(usize);

impl OwnedElement {
    /// Take ownership of a `+1` reference — what the `AXUIElementCopy*` family
    /// and [`focused_element_of_pid`](super::bindings::focused_element_of_pid)
    /// return.
    ///
    /// # Safety
    ///
    /// `element` must be a reference the caller owns and will not release
    /// itself.
    pub unsafe fn adopt(element: AXUIElementRef) -> Option<Self> {
        (!element.is_null()).then(|| Self(element as usize))
    }

    /// Add a reference to an element owned by somebody else, so this guard
    /// keeps it alive independently of that owner.
    ///
    /// # Safety
    ///
    /// `element_ptr` must be a live `AXUIElementRef` for the duration of this
    /// call.
    pub unsafe fn retain(element_ptr: usize) -> Option<Self> {
        if element_ptr == 0 {
            return None;
        }
        // SAFETY: the caller guarantees the pointer is live right now, which is
        // all `CFRetain` needs; from here the reference is ours.
        CFRetain(element_ptr as AXUIElementRef as CFTypeRef);
        Some(Self(element_ptr))
    }

    /// The raw pointer, valid for as long as this guard is held.
    pub fn as_ptr(&self) -> usize {
        self.0
    }

    /// The element, valid for as long as this guard is held.
    pub fn as_element(&self) -> AXUIElementRef {
        self.0 as AXUIElementRef
    }
}

impl Drop for OwnedElement {
    fn drop(&mut self) {
        // SAFETY: the guard holds exactly one reference, taken in `adopt` or
        // `retain` and never handed out, so this balances it exactly once.
        unsafe { CFRelease(self.0 as AXUIElementRef as CFTypeRef) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::{CFGetRetainCount, TCFType};
    use core_foundation::string::CFString;

    fn retain_count(object: CFTypeRef) -> isize {
        unsafe { CFGetRetainCount(object) }
    }

    #[test]
    fn retain_holds_a_reference_of_its_own_until_dropped() {
        let object = CFString::new("element stand-in");
        let raw = object.as_CFTypeRef();
        let base = retain_count(raw);

        let guard = unsafe { OwnedElement::retain(raw as usize) }.expect("retained");
        assert_eq!(guard.as_ptr(), raw as usize);
        assert_eq!(
            retain_count(raw),
            base + 1,
            "capture must not depend on somebody else's reference"
        );

        drop(guard);
        assert_eq!(retain_count(raw), base, "the guard must release on drop");
    }

    #[test]
    fn adopt_takes_over_an_existing_reference() {
        let object = CFString::new("element stand-in");
        let raw = object.as_CFTypeRef();
        let base = retain_count(raw);

        unsafe { CFRetain(raw) };
        let guard = unsafe { OwnedElement::adopt(raw as AXUIElementRef) }.expect("adopted");
        assert_eq!(retain_count(raw), base + 1);

        drop(guard);
        assert_eq!(
            retain_count(raw),
            base,
            "adopt owns the copy it was handed — no second retain, one release"
        );
    }

    #[test]
    fn nothing_to_own_yields_no_guard() {
        assert!(unsafe { OwnedElement::retain(0) }.is_none());
        assert!(unsafe { OwnedElement::adopt(std::ptr::null_mut()) }.is_none());
    }
}
