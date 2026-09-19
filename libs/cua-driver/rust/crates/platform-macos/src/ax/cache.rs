use super::bindings::AXUIElementRef;
use super::tree::AXNode;
use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};
use cua_driver_core::element_cache::{ElementCacheCore, SnapshotPayload};

/// A reference to an AX element, released exactly once on drop.
///
/// Reading an element pointer whose last reference is gone is not a
/// recoverable AX error: `AXUIElementCopyAttributeValue` validates its
/// argument by calling `CFGetTypeID` on the allocation, so a freed element
/// traps (`EXC_BREAKPOINT` / `SIGTRAP`) inside CoreFoundation and takes the
/// driver process with it. Holding a reference of our own turns the same
/// situation into `kAXErrorInvalidUIElement`, which every AX reader here
/// already handles — a destroyed-but-retained element answers "I am invalid"
/// instead of corrupting the caller.
///
/// Use this guard whenever an element pointer has to outlive the thing that
/// produced it: an app's focused element sampled at one instant, a pointer
/// borrowed out of an element-cache snapshot, or anything watched across an
/// action that may destroy it.
///
/// The pointer is stored as a `usize` so the guard is `Send`: CF reference
/// counting is thread-safe and this crate already moves element pointers into
/// `spawn_blocking` as integers. A null guard (`as_ptr() == 0`) retains and
/// releases nothing.
pub struct RetainedElement(usize);

impl RetainedElement {
    pub fn as_ptr(&self) -> usize {
        self.0
    }

    /// Add a reference to an element owned by somebody else, so this guard
    /// keeps it alive independently of that owner.
    ///
    /// # Safety
    ///
    /// `ptr` must be a live `AXUIElementRef` (or `0`) for the duration of this
    /// call.
    pub unsafe fn retain(ptr: usize) -> Self {
        if ptr != 0 {
            unsafe { CFRetain(ptr as AXUIElementRef as CFTypeRef) };
        }
        Self(ptr)
    }

    /// Take ownership of a `+1` reference — what the `AXUIElementCopy*` family
    /// and [`focused_element_of_pid`](super::bindings::focused_element_of_pid)
    /// return. A null element yields no guard.
    ///
    /// # Safety
    ///
    /// `element` must be a reference the caller owns and will not release
    /// itself.
    pub unsafe fn adopt(element: AXUIElementRef) -> Option<Self> {
        (!element.is_null()).then(|| Self(element as usize))
    }
}

impl Clone for RetainedElement {
    fn clone(&self) -> Self {
        unsafe { Self::retain(self.0) }
    }
}

impl Drop for RetainedElement {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { CFRelease(self.0 as AXUIElementRef as CFTypeRef) };
        }
    }
}

pub struct CachedSnapshot {
    pub elements: Vec<usize>,
}

impl CachedSnapshot {
    pub fn from_nodes(nodes: &[AXNode]) -> Self {
        Self {
            elements: nodes
                .iter()
                .filter(|node| node.element_index.is_some())
                .map(|node| node.element_ptr)
                .collect(),
        }
    }
}

impl SnapshotPayload for CachedSnapshot {
    type Element = RetainedElement;
    fn len(&self) -> usize {
        self.elements.len()
    }
    fn retain(&self, index: usize) -> Option<RetainedElement> {
        self.elements
            .get(index)
            .map(|ptr| unsafe { RetainedElement::retain(*ptr) })
    }
}

impl Drop for CachedSnapshot {
    fn drop(&mut self) {
        for ptr in &self.elements {
            if *ptr != 0 {
                unsafe { CFRelease(*ptr as AXUIElementRef as CFTypeRef) };
            }
        }
    }
}

pub type ElementCache = ElementCacheCore<CachedSnapshot>;

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::{CFGetRetainCount, TCFType};
    use core_foundation::string::CFString;
    use cua_driver_core::element_token::{token_for, ResolvedElement};

    fn resolve(cache: &ElementCache, snapshot: u32, index: usize) -> Option<RetainedElement> {
        match cache
            .resolve_element_args(
                1,
                None,
                Some(&token_for(snapshot, index)),
                None,
                Some(2),
                "click",
            )
            .ok()?
        {
            ResolvedElement::Element { element, .. } => Some(element),
            _ => None,
        }
    }

    fn payload(ptr: usize) -> CachedSnapshot {
        unsafe { CFRetain(ptr as CFTypeRef) };
        CachedSnapshot {
            elements: vec![ptr],
        }
    }

    #[test]
    fn retained_element_survives_concurrent_snapshot_replace() {
        let value = CFString::new("cua-driver-uaf-test-element-placeholder");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = unsafe { CFGetRetainCount(ptr as CFTypeRef) };
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(ptr));
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base + 1);
        let guard = resolve(&cache, snapshot, 0).unwrap();
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base + 2);
        cache.publish(1, 2, CachedSnapshot::from_nodes(&[]));
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base + 1);
        assert!(resolve(&cache, snapshot, 0).is_none());
        drop(guard);
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base);
    }

    #[test]
    fn admitted_element_survives_cache_destruction_until_native_work_finishes() {
        let value = CFString::new("cua-driver-invariant-admitted-native-work");
        let ptr = value.as_concrete_TypeRef() as usize;
        let base = unsafe { CFGetRetainCount(ptr as CFTypeRef) };
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(ptr));
        let guard = resolve(&cache, snapshot, 0).unwrap();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            finish_rx.recv().unwrap();
            assert_eq!(guard.as_ptr(), ptr);
            drop(guard);
        });
        drop(cache);
        let retained = unsafe { CFGetRetainCount(ptr as CFTypeRef) };
        finish_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(retained, base + 1);
        assert_eq!(unsafe { CFGetRetainCount(ptr as CFTypeRef) }, base);
    }

    #[test]
    fn missing_index_returns_none() {
        let cache = ElementCache::new();
        assert!(resolve(&cache, 0, 0).is_none());
        let snapshot = cache.publish(1, 2, CachedSnapshot::from_nodes(&[]));
        assert!(resolve(&cache, snapshot, 0).is_none());
        assert!(resolve(&cache, snapshot, 5).is_none());
    }

    #[test]
    fn abandoned_preparation_releases_native_payload_without_replacing_snapshot() {
        let original = CFString::new("cua-driver-original-published-native-work");
        let replacement = CFString::new("cua-driver-abandoned-prepared-native-work");
        let original_ptr = original.as_concrete_TypeRef() as usize;
        let replacement_ptr = replacement.as_concrete_TypeRef() as usize;
        let base = unsafe { CFGetRetainCount(replacement_ptr as CFTypeRef) };
        let cache = ElementCache::new();
        let snapshot = cache.publish(1, 2, payload(original_ptr));
        let prepared = payload(replacement_ptr);
        assert_eq!(resolve(&cache, snapshot, 0).unwrap().as_ptr(), original_ptr);
        drop(prepared);
        assert_eq!(
            unsafe { CFGetRetainCount(replacement_ptr as CFTypeRef) },
            base
        );
        assert_eq!(resolve(&cache, snapshot, 0).unwrap().as_ptr(), original_ptr);
    }

    #[test]
    fn retain_holds_a_reference_of_its_own_until_dropped() {
        let object = CFString::new("element stand-in");
        let raw = object.as_CFTypeRef();
        let base = unsafe { CFGetRetainCount(raw) };

        let guard = unsafe { RetainedElement::retain(raw as usize) };
        assert_eq!(guard.as_ptr(), raw as usize);
        assert_eq!(
            unsafe { CFGetRetainCount(raw) },
            base + 1,
            "capture must not depend on somebody else's reference"
        );

        drop(guard);
        assert_eq!(
            unsafe { CFGetRetainCount(raw) },
            base,
            "the guard must release on drop"
        );
    }

    #[test]
    fn adopt_takes_over_an_existing_reference() {
        let object = CFString::new("element stand-in");
        let raw = object.as_CFTypeRef();
        let base = unsafe { CFGetRetainCount(raw) };

        unsafe { CFRetain(raw) };
        let guard = unsafe { RetainedElement::adopt(raw as AXUIElementRef) }.expect("adopted");
        assert_eq!(unsafe { CFGetRetainCount(raw) }, base + 1);

        drop(guard);
        assert_eq!(
            unsafe { CFGetRetainCount(raw) },
            base,
            "adopt owns the copy it was handed — no second retain, one release"
        );
    }

    #[test]
    fn adopting_a_null_element_yields_no_guard() {
        assert!(unsafe { RetainedElement::adopt(std::ptr::null_mut()) }.is_none());
    }
}
