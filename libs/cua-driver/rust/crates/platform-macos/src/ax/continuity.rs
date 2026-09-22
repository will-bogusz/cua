//! `element_id`: accessibility-object continuity across snapshots.
//!
//! `element_index` and `element_token` are bound to one snapshot. An
//! `element_id` is not: two snapshots in one driver process publish the same
//! id for a row exactly when this registry proves the row is the *same
//! accessibility object* — `CFEqual` on a retained `AXUIElementRef` from an
//! earlier walk, the same role, and the same process instance (pid plus
//! process start time, so a reused pid starts over).
//!
//! It is object continuity, NOT record identity. AX lets an application reuse
//! an object for different content (AppKit table views recycle cell views
//! across rows; a freed object's identity can be reissued), so the same id
//! can name an object now showing a different record. The converse is
//! conservative: anything this registry cannot prove — an unreadable process
//! start time, an evicted entry, a different role — gets a fresh id. A fresh
//! id never means the object is new, only that continuity is unproven.

use super::bindings::AXUIElementRef;
use super::cache::RetainedElement;
use core_foundation::base::{CFEqual, CFHash, CFTypeRef};
use std::collections::HashMap;
use std::sync::Mutex;

/// Snapshots of one process an unseen entry survives. Matches the per-pid
/// snapshot history the element-token cache keeps.
const RETAINED_SNAPSHOTS: u64 = 8;
/// Entries one process may hold; beyond it the least recently seen go.
const MAX_ENTRIES_PER_PROCESS: usize = 20_000;

struct Entry {
    element: RetainedElement,
    role: String,
    id: u64,
    last_seen: u64,
}

#[derive(Default)]
struct Process {
    started: Option<(i64, i64)>,
    snapshot: u64,
    buckets: HashMap<usize, Vec<Entry>>,
    len: usize,
}

impl Process {
    fn evict(&mut self) {
        let horizon = self.snapshot.saturating_sub(RETAINED_SNAPSHOTS);
        self.retain_entries(|entry| entry.last_seen > horizon);
        if self.len > MAX_ENTRIES_PER_PROCESS {
            let mut seen: Vec<u64> = self
                .buckets
                .values()
                .flatten()
                .map(|entry| entry.last_seen)
                .collect();
            seen.sort_unstable();
            let cutoff = seen[self.len - MAX_ENTRIES_PER_PROCESS];
            self.retain_entries(|entry| entry.last_seen >= cutoff);
        }
    }

    fn retain_entries(&mut self, keep: impl Fn(&Entry) -> bool) {
        let mut len = 0;
        self.buckets.retain(|_, bucket| {
            bucket.retain(&keep);
            len += bucket.len();
            !bucket.is_empty()
        });
        self.len = len;
    }
}

/// The process-wide registry. One per `ToolState`.
#[derive(Default)]
pub struct ElementIds {
    inner: Mutex<(u64, HashMap<i32, Process>)>,
}

/// One row to identify: a live element pointer from the walk that is
/// publishing it, and its role.
pub struct Sighting<'a> {
    pub element: usize,
    pub role: &'a str,
}

pub fn format_id(id: u64) -> String {
    format!("ax-{id}")
}

impl ElementIds {
    /// Ids for one snapshot's rows of `pid`, parallel to `rows`.
    ///
    /// # Safety
    /// Every `Sighting::element` must be a live `AXUIElementRef` for the
    /// duration of the call.
    pub unsafe fn assign(&self, pid: i32, rows: &[Sighting<'_>]) -> Vec<String> {
        self.assign_with_start(pid, process_start(pid), rows)
    }

    unsafe fn assign_with_start(
        &self,
        pid: i32,
        started: Option<(i64, i64)>,
        rows: &[Sighting<'_>],
    ) -> Vec<String> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (next, processes) = &mut *guard;
        let process = processes.entry(pid).or_default();
        // A different or unknown process instance proves nothing about any
        // earlier object: start over.
        if started.is_none() || process.started != started {
            *process = Process {
                started,
                ..Process::default()
            };
        }
        process.snapshot += 1;
        let snapshot = process.snapshot;
        let ids = rows
            .iter()
            .map(|row| {
                let element = row.element as AXUIElementRef as CFTypeRef;
                let hash = CFHash(element) as usize;
                let bucket = process.buckets.entry(hash).or_default();
                if let Some(entry) = bucket.iter_mut().find(|entry| {
                    entry.role == row.role
                        && CFEqual(entry.element.as_ptr() as CFTypeRef, element) != 0
                }) {
                    entry.last_seen = snapshot;
                    return entry.id;
                }
                *next += 1;
                bucket.push(Entry {
                    element: RetainedElement::retain(row.element),
                    role: row.role.to_owned(),
                    id: *next,
                    last_seen: snapshot,
                });
                process.len += 1;
                *next
            })
            .map(format_id)
            .collect();
        process.evict();
        ids
    }
}

/// When `pid`'s current process started, or `None` when it cannot be read.
fn process_start(pid: i32) -> Option<(i64, i64)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then_some((info.pbi_start_tvsec as i64, info.pbi_start_tvusec as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    /// CFStrings stand in for AX elements: the registry only needs
    /// CFHash/CFEqual/retain, which every CF type answers, and distinct
    /// allocations with equal contents are exactly "equal objects".
    fn object(text: &str) -> CFString {
        CFString::new(text)
    }

    fn ids(registry: &ElementIds, started: (i64, i64), rows: &[(&CFString, &str)]) -> Vec<String> {
        let sightings: Vec<Sighting<'_>> = rows
            .iter()
            .map(|(object, role)| Sighting {
                element: object.as_concrete_TypeRef() as usize,
                role,
            })
            .collect();
        unsafe { registry.assign_with_start(7, Some(started), &sightings) }
    }

    #[test]
    fn an_equal_object_keeps_its_id_and_a_different_one_gets_a_new_id() {
        let registry = ElementIds::default();
        let (field, button) = (object("field"), object("button"));
        let first = ids(&registry, (1, 0), &[(&field, "AXTextField"), (&button, "AXButton")]);
        // A separate allocation equal to the first: the same AX object seen
        // through a fresh proxy in the next walk.
        let field_again = object("field");
        let other = object("other");
        let second = ids(&registry, (1, 0), &[(&other, "AXButton"), (&field_again, "AXTextField")]);
        assert_eq!(second[1], first[0]);
        assert_ne!(second[0], first[1]);
        assert_ne!(first[0], first[1]);
    }

    #[test]
    fn a_role_change_or_a_new_process_instance_breaks_continuity() {
        let registry = ElementIds::default();
        let row = object("row");
        let first = ids(&registry, (1, 0), &[(&row, "AXRow")]);
        assert_ne!(ids(&registry, (1, 0), &[(&row, "AXCell")]), first);
        let before_restart = ids(&registry, (1, 0), &[(&row, "AXRow")]);
        assert_ne!(ids(&registry, (2, 0), &[(&row, "AXRow")]), before_restart, "pid reused");
    }

    #[test]
    fn an_unreadable_process_start_never_reuses_an_id() {
        let registry = ElementIds::default();
        let row = object("row");
        let sighting = [Sighting {
            element: row.as_concrete_TypeRef() as usize,
            role: "AXRow",
        }];
        let first = unsafe { registry.assign_with_start(7, None, &sighting) };
        let second = unsafe { registry.assign_with_start(7, None, &sighting) };
        assert_ne!(first, second);
    }

    #[test]
    fn an_object_unseen_for_the_retained_snapshots_gets_a_fresh_id() {
        let registry = ElementIds::default();
        let (kept, idle) = (object("kept"), object("idle"));
        let first = ids(&registry, (1, 0), &[(&kept, "AXButton"), (&idle, "AXButton")]);
        for _ in 0..RETAINED_SNAPSHOTS {
            ids(&registry, (1, 0), &[(&kept, "AXButton")]);
        }
        let later = ids(&registry, (1, 0), &[(&kept, "AXButton"), (&idle, "AXButton")]);
        assert_eq!(later[0], first[0]);
        assert_ne!(later[1], first[1]);
    }
}
