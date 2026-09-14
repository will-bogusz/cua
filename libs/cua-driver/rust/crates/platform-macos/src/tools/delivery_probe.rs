//! Post-dispatch delivery evidence for background element actions.
//!
//! ## Why this exists
//!
//! `AXUIElementPerformAction` returns `kAXErrorSuccess` whenever the *request*
//! was accepted, and a PID-routed `CGEvent` reports success once it is posted.
//! Neither reply says the receiver reacted. Measured on a neutral Chromium
//! window (probe page, target window not frontmost, iTerm frontmost):
//!
//! | control                      | AX press (background) | PID CGEvent (background) | HID (foreground) |
//! |------------------------------|-----------------------|--------------------------|------------------|
//! | `<button>` with `click`      | fires                 | fires                    | fires            |
//! | `div` with `mousedown`       | fires                 | fires                    | fires            |
//! | `div` with `pointerdown`     | **silent no-op**      | **silent no-op**         | fires            |
//!
//! Background delivery never produces trusted pointer events, so a control
//! driven by `pointerdown` — the common shape in Chromium/Electron UIs —
//! ignores both background rungs while the reply still looks like a success.
//! The agent then trusts a hollow success and hunts for a change that never
//! happened.
//!
//! ## What this probe promises
//!
//! A click has no general postcondition, so this probe never claims the
//! *intended* effect. It answers one narrower question honestly: after the
//! dispatch, did anything observable about the target change?
//!
//! * the clicked element's own AX state (value / title / focused / selected /
//!   enabled / frame),
//! * the owning application's AX focused element (a real click focuses what it
//!   hits; a semantic AX press usually does not),
//! * a capped digest of the target window's AX subtree — used only when the
//!   window proved quiescent across two back-to-back pre-dispatch samples, so
//!   a self-updating window (clock, download counter, memory readout) can never
//!   be mistaken for the click landing.
//!
//! Unchanged across every usable signal is reported as such and escalated to
//! the foreground rung. It is never converted into a retry, a route switch, or
//! an error: a dispatched action may have taken effect invisibly, and repeating
//! it could act twice.

use std::time::{Duration, Instant};

use crate::ax::bindings::{
    children_count, copy_bool_attr, copy_string_attr, element_screen_rect, focused_element_of_pid,
    AXUIElementRef,
};
use core_foundation::base::{CFRelease, CFTypeRef};

/// Node cap for the probe's window digest. Large enough to reach the content
/// area of real app windows, small enough that two extra walks stay cheap.
const TREE_MAX_ELEMENTS: usize = 250;
/// Depth cap for the probe's window digest — matches the observation default.
const TREE_MAX_DEPTH: usize = 25;
/// Native-request budget for one digest walk. A window that cannot be read
/// inside this budget yields no digest rather than a half-walk that would
/// differ from its own predecessor for no reason.
const TREE_TIMEOUT: Duration = Duration::from_millis(250);
/// How long the post-dispatch window is sampled before a verdict of "nothing
/// reacted" is believed. An application answers an AX action on its own main
/// loop, so the reaction is not in place when `AXUIElementPerformAction`
/// returns: measured on Contacts, pressing the toolbar's add button grows the
/// button's `AXMenu` child ~1.3 s after the dispatch. A single sample ~500 ms
/// in called that a no-op and sent the caller to a pixel rung the app ignores.
/// Matches the `type_text` delivery drain, which answers the same question
/// about the same kind of latency.
const SETTLE_BUDGET: Duration = Duration::from_secs(2);
/// Gap between post-dispatch samples. Each sample already costs one or more
/// native AX reads, so this only keeps a cheap sample set from spinning.
const SETTLE_POLL: Duration = Duration::from_millis(50);

/// One sample of the signals the probe can compare. `None` means "not
/// readable", which is never treated as a change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals {
    /// Identity + mutable state of the clicked element.
    pub element: Option<String>,
    /// Identity of the application's AX focused element.
    pub focus: Option<String>,
    /// Digest of the target window's capped AX subtree.
    pub tree: Option<u64>,
}

/// What the probe observed after the dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// A usable signal differed after the dispatch; the app reacted.
    Changed(&'static str),
    /// Every usable signal was identical after the dispatch.
    Unchanged,
    /// Nothing could be compared (element gone, window unreadable, and a
    /// self-updating or unreadable subtree).
    Unusable,
}

impl Evidence {
    pub fn signal(self) -> &'static str {
        match self {
            Evidence::Changed(signal) => signal,
            Evidence::Unchanged => "none",
            Evidence::Unusable => "unavailable",
        }
    }
}

/// A verdict plus what it cost to reach it.
#[derive(Debug, Clone, Copy)]
pub struct ProbeOutcome {
    pub evidence: Evidence,
    /// Total AX cost of the probe, before and after the dispatch.
    pub probe: Duration,
    /// How long the post-dispatch window was sampled before this verdict.
    /// This is the number a "nothing reacted" report has to publish: it is
    /// the whole claim's scope.
    pub waited: Duration,
}

/// Pre-dispatch capture. Hold it across the action, then `compare()`.
pub struct DeliveryProbe {
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
    before: Signals,
    /// The window's subtree digest was identical across two pre-dispatch
    /// samples, so a post-dispatch difference is attributable to the action.
    quiescent: bool,
    elapsed: Duration,
}

impl DeliveryProbe {
    /// Sample the target before dispatch. Blocking AX work — call from a
    /// blocking thread. `element_ptr` must stay retained by the caller.
    pub fn capture(pid: i32, window_id: u32, element_ptr: Option<usize>) -> Self {
        let start = Instant::now();
        let element = element_ptr.and_then(element_state);
        let focus = focus_state(pid);
        let first = tree_digest(pid, window_id);
        let second = tree_digest(pid, window_id);
        let quiescent = matches!((first, second), (Some(a), Some(b)) if a == b);
        Self {
            pid,
            window_id,
            element_ptr,
            before: Signals {
                element,
                focus,
                tree: second,
            },
            quiescent,
            elapsed: start.elapsed(),
        }
    }

    /// Sample the target until something differs or the settle budget runs
    /// out. Blocking AX work — call from a blocking thread.
    ///
    /// The first sample that differs ends the wait, so a responsive
    /// application pays only its own latency; only a target that never
    /// reacts pays the whole budget.
    pub fn compare(self) -> ProbeOutcome {
        self.compare_within(SETTLE_BUDGET)
    }

    fn compare_within(self, budget: Duration) -> ProbeOutcome {
        let start = Instant::now();
        let deadline = start + budget;
        loop {
            let after = self.sample();
            let evidence = classify(&self.before, &after, self.quiescent);
            let expired = Instant::now() >= deadline;
            if matches!(evidence, Evidence::Changed(_)) || expired {
                return self.outcome(evidence, start.elapsed());
            }
            // A cancelled caller stops waiting on a target it no longer wants
            // and keeps the verdict observed so far.
            if cua_driver_core::operation::sleep(SETTLE_POLL).is_err() {
                return self.outcome(evidence, start.elapsed());
            }
        }
    }

    fn sample(&self) -> Signals {
        Signals {
            element: self.element_ptr.and_then(element_state),
            focus: focus_state(self.pid),
            tree: if self.quiescent {
                tree_digest(self.pid, self.window_id)
            } else {
                None
            },
        }
    }

    fn outcome(&self, evidence: Evidence, waited: Duration) -> ProbeOutcome {
        ProbeOutcome {
            evidence,
            probe: self.elapsed + waited,
            waited,
        }
    }

    /// Verdict for a reaction that was already proven while the action ran —
    /// a sheet or dialog the window observer caught — so nothing is waited for.
    pub fn settled(&self, evidence: Evidence) -> ProbeOutcome {
        ProbeOutcome {
            evidence,
            probe: self.elapsed,
            waited: Duration::ZERO,
        }
    }
}

/// Pure verdict over two samples. A signal counts only when both samples read
/// it; an unreadable half is unknown, never a change.
pub fn classify(before: &Signals, after: &Signals, quiescent: bool) -> Evidence {
    let mut usable = false;
    for (b, a, signal) in [
        (&before.element, &after.element, "element_state"),
        (&before.focus, &after.focus, "app_focus"),
    ] {
        if let (Some(b), Some(a)) = (b.as_ref(), a.as_ref()) {
            usable = true;
            if b != a {
                return Evidence::Changed(signal);
            }
        }
    }
    if quiescent {
        if let (Some(b), Some(a)) = (before.tree, after.tree) {
            usable = true;
            if b != a {
                return Evidence::Changed("window_tree");
            }
        }
    }
    if usable {
        Evidence::Unchanged
    } else {
        Evidence::Unusable
    }
}

/// Identity + mutable state of one element. `None` when the element no longer
/// answers AX reads at all (replaced/detached node).
///
/// The child count is part of that state because it is the only signal a
/// menu-bearing control moves: a toolbar `AXMenuButton` whose menu opened
/// keeps its role, title, value, focus, selection, enablement and frame, and
/// gains one `AXMenu` child. Contacts' add button is exactly that control,
/// and without this the probe called an opened menu a no-op.
fn element_state(element_ptr: usize) -> Option<String> {
    let element = element_ptr as AXUIElementRef;
    unsafe {
        let role = copy_string_attr(element, "AXRole")?;
        let title = copy_string_attr(element, "AXTitle").unwrap_or_default();
        let value = copy_string_attr(element, "AXValue").unwrap_or_default();
        let focused = copy_bool_attr(element, "AXFocused");
        let selected = copy_bool_attr(element, "AXSelected");
        let enabled = copy_bool_attr(element, "AXEnabled");
        let rect = element_screen_rect(element);
        let children = children_count(element);
        Some(format!(
            "{role}|{title}|{value}|{focused:?}|{selected:?}|{enabled:?}|{rect:?}|{children:?}"
        ))
    }
}

/// Identity of the app's focused element. A real click focuses the control it
/// lands on, so this moves on delivery even when the control's own state does
/// not.
fn focus_state(pid: i32) -> Option<String> {
    unsafe {
        let focused = focused_element_of_pid(pid)?;
        let state = element_state(focused as usize);
        CFRelease(focused as CFTypeRef);
        state
    }
}

/// Digest of the target window's capped AX subtree. `None` when the walk could
/// not finish inside its budget — a truncated-by-deadline walk is not
/// comparable with the next one.
fn tree_digest(pid: i32, window_id: u32) -> Option<u64> {
    let walk = crate::ax::tree::walk_tree_with_timeout(
        pid,
        Some(window_id),
        None,
        TREE_MAX_ELEMENTS,
        TREE_MAX_DEPTH,
        TREE_TIMEOUT,
    );
    if walk.timed_out || walk.stop_reason.is_some() || walk.tree_markdown.is_empty() {
        return None;
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&walk.tree_markdown, &mut hash);
    Some(std::hash::Hasher::finish(&hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(element: Option<&str>, focus: Option<&str>, tree: Option<u64>) -> Signals {
        Signals {
            element: element.map(str::to_owned),
            focus: focus.map(str::to_owned),
            tree,
        }
    }

    #[test]
    fn element_state_change_is_delivery_evidence() {
        let before = signals(Some("AXCheckBox||0|Some(false)"), Some("f"), Some(1));
        let after = signals(Some("AXCheckBox||1|Some(false)"), Some("f"), Some(1));
        assert_eq!(
            classify(&before, &after, true),
            Evidence::Changed("element_state")
        );
    }

    #[test]
    fn focus_move_is_delivery_evidence_when_element_state_holds() {
        let before = signals(Some("AXButton|New Item||"), Some("AXWebArea|doc"), Some(7));
        let after = signals(
            Some("AXButton|New Item||"),
            Some("AXButton|New Item"),
            Some(7),
        );
        assert_eq!(
            classify(&before, &after, true),
            Evidence::Changed("app_focus")
        );
    }

    #[test]
    fn quiescent_window_reports_subtree_change() {
        let before = signals(Some("AXButton|b||"), Some("f"), Some(11));
        let after = signals(Some("AXButton|b||"), Some("f"), Some(12));
        assert_eq!(
            classify(&before, &after, true),
            Evidence::Changed("window_tree")
        );
    }

    #[test]
    fn self_updating_window_never_counts_as_delivery() {
        let before = signals(Some("AXButton|b||"), Some("f"), Some(11));
        let after = signals(Some("AXButton|b||"), Some("f"), Some(12));
        assert_eq!(classify(&before, &after, false), Evidence::Unchanged);
    }

    #[test]
    fn every_usable_signal_identical_is_unchanged() {
        let before = signals(Some("AXButton|b||"), Some("f"), Some(11));
        let after = signals(Some("AXButton|b||"), Some("f"), Some(11));
        assert_eq!(classify(&before, &after, true), Evidence::Unchanged);
    }

    #[test]
    fn one_sided_reads_are_unknown_not_change() {
        let before = signals(Some("AXButton|b||"), None, None);
        let after = signals(None, Some("f"), Some(4));
        assert_eq!(classify(&before, &after, true), Evidence::Unusable);
    }

    #[test]
    fn detached_element_with_readable_focus_still_compares_focus() {
        let before = signals(None, Some("AXWebArea|doc"), None);
        let after = signals(None, Some("AXWebArea|doc"), None);
        assert_eq!(classify(&before, &after, true), Evidence::Unchanged);
    }
}
