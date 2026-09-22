//! Window-change detector — Rust port of Swift's
//! `WindowChangeDetector` (`libs/cua-driver/Sources/CuaDriverServer/Tools/WindowChangeDetector.swift`).
//!
//! ## What this does
//!
//! Action tools (click, type_text, hotkey, …) on a backgrounded app can
//! trigger window/foreground side-effects: a "Sign In" button opens a
//! modal sheet, a Safari link spawns a new tab, an autocomplete dropdown
//! pops a helper window. The Rust port mirrors Swift's
//! snapshot → action → detect cycle so tool results can:
//!
//! 1. Surface the side-effect to the agent (one-line suffix on the
//!    tool result, matching Swift verbatim).
//! 2. Arm a **wildcard** focus-steal suppression entry that covers the
//!    full snapshot→detect window. Wildcards (`target_pid = None`)
//!    catch any activation other than the prior frontmost — so even an
//!    app we didn't know about (Safari activating because a UTM Gallery
//!    link routed to it) is suppressed before the first compositor
//!    frame.
//!
//! ## Usage
//!
//! ```ignore
//! // Callers capture frontmost BEFORE the snapshot so the wildcard
//! // suppressor and the snapshot's recorded frontmost agree on the
//! // pid to restore to — avoids a race where another app activates
//! // between the caller's `frontmost_pid()` and the detector's own.
//! let prior_front = apps::frontmost_pid();
//! let snapshot = WindowChangeDetector::snapshot(prior_front);
//! // … perform action …
//! let changes = snapshot.detect();
//! // changes.result_suffix() — append to ToolResult text.
//! ```
//!
//! Dropping the `Snapshot` ends the suppression lease (RAII). `detect()`
//! also drops the lease before returning — the lease's `Drop` is
//! idempotent so explicit-detect + later-drop is safe.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::apps;
use crate::focus_steal::{self, SuppressionLease};
use crate::windows::{self, WindowInfo};

/// One window that appeared between `snapshot()` and `detect()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEvent {
    pub window_id: u32,
    pub pid: i32,
    pub app_name: String,
    pub title: String,
}

/// A window that appeared while an action ran, with what accessibility proves
/// about how it relates to its application's other windows.
///
/// Appearing during the action is timing, not causation: any process may open
/// a window inside the poll. `relation` is `sheet` only when the window is an
/// `AXSheet` whose accessibility parent is the window `attached_to`, and
/// `app-modal` only when its application reports it `AXModal` (so no other
/// window of that process accepts input while it is up). Everything else is
/// `unknown` — including a window accessibility could not resolve, which also
/// carries no `role`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GainedWindow {
    pub window_id: u32,
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subrole: Option<String>,
    pub relation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attached_to: Option<u32>,
}

/// What accessibility says about one window, read by [`read_window_facts`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowFacts {
    pub role: Option<String>,
    pub subrole: Option<String>,
    pub modal: Option<bool>,
    /// CGWindowID of the window whose accessibility children include this
    /// window (a sheet's own window), when it is one.
    pub parent_window: Option<u32>,
}

pub const SHEET_RELATION: &str = crate::ax::window_scope::SHEET_RELATION;
pub const APP_MODAL_RELATION: &str = crate::ax::window_scope::MODAL_RELATION;
pub const UNKNOWN_RELATION: &str = "unknown";

impl GainedWindow {
    /// Classify one appeared window from its accessibility facts (`None`:
    /// accessibility could not resolve it).
    pub fn classify(event: &WindowEvent, facts: Option<WindowFacts>) -> Self {
        let facts = facts.unwrap_or_default();
        let (relation, attached_to) = match (&facts.role, facts.parent_window, facts.modal) {
            (Some(role), Some(parent), _) if role == "AXSheet" => (SHEET_RELATION, Some(parent)),
            (_, _, Some(true)) => (APP_MODAL_RELATION, None),
            _ => (UNKNOWN_RELATION, None),
        };
        Self {
            window_id: event.window_id,
            pid: event.pid,
            app_name: event.app_name.clone(),
            title: event.title.clone(),
            role: facts.role,
            subrole: facts.subrole,
            relation,
            attached_to,
        }
    }
}

/// Windows whose accessibility facts one post-action poll reads; the rest are
/// reported with `relation: unknown`. An action opens a sheet or a dialog, not
/// a flock of windows, and each read is a native round trip.
const MAX_CLASSIFIED_WINDOWS: usize = 8;
/// Per-element messaging timeout for those reads, so a wedged app cannot hold
/// the reply.
const WINDOW_FACT_TIMEOUT_SECONDS: f32 = 0.25;
/// Top-level windows searched for an appeared sheet.
const MAX_SHEET_PARENTS: usize = 32;

/// Classify every appeared window. Blocking: native accessibility reads.
pub fn gained_windows(events: &[WindowEvent]) -> Vec<GainedWindow> {
    events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let facts = (index < MAX_CLASSIFIED_WINDOWS)
                .then(|| read_window_facts(event.pid, event.window_id))
                .flatten();
            GainedWindow::classify(event, facts)
        })
        .collect()
}

/// Find `window_id` among `pid`'s accessibility windows, or among their
/// sheets, and read its role, subrole, modality and (for a sheet) the window
/// it hangs off. `None` when accessibility has no element for it.
pub fn read_window_facts(pid: i32, window_id: u32) -> Option<WindowFacts> {
    use crate::ax::bindings::*;
    use core_foundation::base::{CFRelease, CFTypeRef};
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(app, WINDOW_FACT_TIMEOUT_SECONDS);
        let windows = copy_ax_windows(app);
        CFRelease(app as CFTypeRef);
        let read = |element: AXUIElementRef, parent_window: Option<u32>| WindowFacts {
            role: copy_string_attr(element, "AXRole"),
            subrole: copy_string_attr(element, "AXSubrole"),
            modal: copy_bool_attr(element, "AXModal"),
            parent_window,
        };
        let mut facts = None;
        for &window in &windows {
            AXUIElementSetMessagingTimeout(window, WINDOW_FACT_TIMEOUT_SECONDS);
            if ax_get_window_id(window) == Some(window_id) {
                let parent = copy_element_attr(window, "AXParent").and_then(|parent| {
                    let id = ax_get_window_id(parent);
                    CFRelease(parent as CFTypeRef);
                    id
                });
                facts = Some(read(window, parent));
                break;
            }
        }
        if facts.is_none() {
            'search: for &window in windows.iter().take(MAX_SHEET_PARENTS) {
                let children = copy_children(window);
                let mut found = None;
                for &child in &children {
                    AXUIElementSetMessagingTimeout(child, WINDOW_FACT_TIMEOUT_SECONDS);
                    if copy_string_attr(child, "AXRole").as_deref() == Some("AXSheet")
                        && ax_get_window_id(child) == Some(window_id)
                    {
                        found = Some(read(child, ax_get_window_id(window)));
                        break;
                    }
                }
                release_all(children);
                if found.is_some() {
                    facts = found;
                    break 'search;
                }
            }
        }
        release_all(windows);
        facts
    }
}

/// Categorical diff entry. We mirror Swift which only emits
/// `WindowEvent` rows for *new* windows — closed/changed never appear
/// in the result suffix — but keep them as enum variants for future
/// extensibility and so unit tests can pin down the diff semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowChange {
    Opened(WindowEvent),
    Closed { window_id: u32 },
}

/// State captured immediately before the action fires.
///
/// Holds:
/// - `window_ids` — the set of visible layer-0 window IDs at snapshot
///   time. `detect()` diffs against this.
/// - `front_pid` — the OS frontmost pid at snapshot time. `detect()`
///   reports whether a *different* pid became frontmost. The wildcard
///   suppressor in `focus_steal` will normally restore the original
///   front before `detect()`'s poll loop observes the change, so this
///   field is best-effort.
/// - `_lease` — the wildcard suppression lease. Dropping the snapshot
///   ends suppression. Held inside `Option` so `detect()` can take it
///   and drop early.
pub struct Snapshot {
    window_ids: HashSet<u32>,
    front_pid: Option<i32>,
    _lease: Option<SuppressionLease>,
}

/// Result of `detect()` — what changed during the action window.
#[derive(Debug, Clone)]
pub struct Changes {
    pub new_windows: Vec<WindowEvent>,
    /// `new_windows` classified; filled by the tools' post-action observation.
    pub gained_windows: Vec<GainedWindow>,
    pub foreground_changed: bool,
    /// Whether the post-action window poll ran at all. A caller that declined
    /// it (`detect_window_change: false`) gets `false`, and no reply may then
    /// claim new windows were among the signals it watched.
    pub polled: bool,
}

impl Changes {
    pub fn no_change() -> Self {
        Self {
            new_windows: Vec::new(),
            gained_windows: Vec::new(),
            foreground_changed: false,
            polled: true,
        }
    }

    pub fn not_polled() -> Self {
        Self {
            polled: false,
            ..Self::no_change()
        }
    }

    /// True when we found evidence that the action triggered a cross-app
    /// side-effect that required (or would have required) a foreground
    /// restore. Matches Swift's `Changes.needsRestore`.
    /// Publish the windows that appeared as `gained_windows`: an array
    /// (possibly empty) when the poll ran, nothing when it did not — so an
    /// absent key is "not watched", never "none appeared".
    pub fn publish_gained_windows(&self, structured: &mut serde_json::Value) {
        if self.polled && structured.is_object() {
            structured["gained_windows"] =
                serde_json::to_value(&self.gained_windows).unwrap_or_default();
        }
    }

    pub fn needs_restore(&self) -> bool {
        self.foreground_changed || !self.new_windows.is_empty()
    }

    /// One-liner summary to append to a tool result, or empty string
    /// when nothing interesting happened.
    ///
    /// Format mirrors Swift `WindowChangeDetector.Changes.resultSuffix`
    /// **verbatim** so MCP callers that key off the suffix wording
    /// don't need a per-binary special case.
    pub fn result_suffix(&self) -> String {
        if !self.needs_restore() {
            return String::new();
        }

        if !self.new_windows.is_empty() {
            // Group by app name (stable order), join titles per app.
            let mut by_app: std::collections::BTreeMap<&str, Vec<&str>> =
                std::collections::BTreeMap::new();
            for w in &self.new_windows {
                by_app.entry(&w.app_name).or_default().push(&w.title);
            }
            let summaries: Vec<String> = by_app
                .into_iter()
                .map(|(app, titles)| {
                    let titles: Vec<String> = titles
                        .into_iter()
                        .filter(|t| !t.is_empty())
                        .map(|t| format!("\"{t}\""))
                        .collect();
                    if titles.is_empty() {
                        app.to_string()
                    } else {
                        format!("{app} ({})", titles.join(", "))
                    }
                })
                .collect();
            format!(
                "\n\n🪟 Action opened new window(s): {}.",
                summaries.join("; ")
            )
        } else {
            "\n\n🔀 Action caused a different app to become frontmost.".to_string()
        }
    }
}

/// Default poll deadline — new windows triggered by a click typically
/// appear within ~200ms on macOS; 1.0s gives the wildcard suppressor
/// time to fire and settle.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Default inter-poll interval. Matches Swift's 50ms.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Public API. Mirrors Swift `enum WindowChangeDetector` — no state of
/// its own; all state lives inside the returned `Snapshot`.
pub struct WindowChangeDetector;

impl WindowChangeDetector {
    /// Capture the current window set + frontmost pid and arm the
    /// wildcard focus-steal suppressor. Call immediately before
    /// dispatching the action.
    ///
    /// `prior_front` is the frontmost pid the **caller** already
    /// observed — typically captured one line earlier via
    /// `apps::frontmost_pid()` for the surrounding `focus_guard`
    /// lease. We use the caller's value (not a fresh re-read) so the
    /// wildcard suppressor's `restore_to` matches what the focus-guard
    /// lease saw; a race where another app became frontmost between
    /// the caller's read and this method would otherwise leave the
    /// two leases targeting different pids.
    ///
    /// Returns `Snapshot`. Drop ends suppression (via the held
    /// `SuppressionLease`); call `Snapshot::detect()` to consume the
    /// snapshot and get a `Changes` summary.
    ///
    /// Safe to call from any thread — `CGWindowListCopyWindowInfo` is
    /// documented as thread-safe.
    pub fn snapshot(prior_front: Option<i32>) -> Snapshot {
        Self::capture(prior_front, true, None)
    }

    /// Capture the same before-state without arming reactive focus suppression.
    /// Foreground delivery owns its temporary activation and restoration, so a
    /// wildcard lease would race the target while the action is settling.
    pub fn snapshot_without_suppression(prior_front: Option<i32>) -> Snapshot {
        Self::capture(prior_front, false, None)
    }

    /// Capture the before-state and suppress cross-app activations while
    /// allowing one intentional target activation.
    ///
    /// The raw background pixel-click path needs this middle ground:
    /// focus-without-raise makes `allowed_pid` AppKit-active so its event queue
    /// accepts the click, but a link or hand-off that activates a different app
    /// must still restore the user's original foreground.
    pub fn snapshot_allowing_activation(prior_front: Option<i32>, allowed_pid: i32) -> Snapshot {
        Self::capture(prior_front, true, Some(allowed_pid))
    }

    fn capture(
        prior_front: Option<i32>,
        suppress_focus: bool,
        allowed_pid: Option<i32>,
    ) -> Snapshot {
        let window_ids: HashSet<u32> = windows::visible_windows()
            .into_iter()
            .filter(|w| w.layer == 0)
            .map(|w| w.window_id)
            .collect();

        // Arm wildcard suppression — covers snapshot → detect window.
        // restore_to = caller-captured frontmost; target = wildcard
        // (any other pid). If there's no frontmost (rare — screensaver,
        // login window), we skip the lease; foreground-change tracking
        // still runs.
        let lease = prior_front
            .filter(|_| suppress_focus)
            .map(|restore_to| match allowed_pid {
                Some(pid) => focus_steal::begin_suppression_allowing(
                    pid,
                    restore_to,
                    "WindowChangeDetector.snapshot_allowing_activation",
                ),
                None => focus_steal::begin_suppression(
                    None, // wildcard
                    restore_to,
                    "WindowChangeDetector.snapshot",
                ),
            });

        Snapshot {
            window_ids,
            front_pid: prior_front,
            _lease: lease,
        }
    }
}

impl Snapshot {
    /// Frontmost pid at snapshot time, if any.
    pub fn front_pid(&self) -> Option<i32> {
        self.front_pid
    }

    /// Poll for up to `DEFAULT_TIMEOUT` for new windows or a
    /// foreground-app change. Returns as soon as a change is detected
    /// or the timeout elapses.
    ///
    /// Consumes the snapshot — the wildcard suppression lease is
    /// dropped when this returns (covers the full action + detection
    /// window).
    pub fn detect(self) -> Changes {
        self.detect_with(DEFAULT_TIMEOUT, DEFAULT_POLL_INTERVAL)
    }

    /// Async wrapper around `detect()` — runs the synchronous poll
    /// loop on a `spawn_blocking` thread so it doesn't stall the
    /// tokio runtime. Most action-tool call sites should prefer this
    /// over the blocking `detect()`.
    pub async fn detect_async(self) -> Changes {
        // Move the Snapshot (and its embedded lease) onto the blocking
        // thread; the lease's Drop runs there when detect_with returns.
        tokio::task::spawn_blocking(move || self.detect())
            .await
            .unwrap_or_else(|_| Changes::not_polled())
    }

    /// Same as `detect()` but with configurable timing — exposed for
    /// tests / callers that want a tighter or looser poll window.
    pub fn detect_with(self, timeout: Duration, poll_interval: Duration) -> Changes {
        let deadline = Instant::now() + timeout;
        loop {
            std::thread::sleep(poll_interval);

            let current: Vec<WindowInfo> = windows::visible_windows()
                .into_iter()
                .filter(|w| w.layer == 0)
                .collect();
            let current_ids: HashSet<u32> = current.iter().map(|w| w.window_id).collect();

            let new_windows: Vec<WindowEvent> = current
                .iter()
                .filter(|w| !self.window_ids.contains(&w.window_id))
                .map(|w| WindowEvent {
                    window_id: w.window_id,
                    pid: w.pid,
                    app_name: w.app_name.clone(),
                    title: w.title.clone(),
                })
                .collect();
            // Diff the other direction too — keeps unit tests honest
            // even though Swift's result_suffix only uses opened windows.
            let _closed: Vec<u32> = self
                .window_ids
                .iter()
                .copied()
                .filter(|id| !current_ids.contains(id))
                .collect();

            let current_front = apps::frontmost_pid();
            let foreground_changed = match (self.front_pid, current_front) {
                (Some(orig), Some(cur)) => orig != cur,
                _ => false,
            };

            if !new_windows.is_empty() || foreground_changed {
                return Changes {
                    new_windows,
                    gained_windows: Vec::new(),
                    foreground_changed,
                    polled: true,
                };
            }
            if Instant::now() >= deadline {
                return Changes::no_change();
            }
        }
    }

    // ── Internal helpers — also used by unit tests via the `pub(super)`
    // path so the diff logic can be exercised without driving the live
    // window enumerator. ────────────────────────────────────────────

    /// Pure-function diff: given the snapshot's window-id set + a
    /// list of currently-visible windows, return the (opened, closed)
    /// classification.
    ///
    /// `#[allow(dead_code)]`: today only the `#[cfg(test)]` block below
    /// constructs this — production callers `wait_for_window_change` /
    /// `wait_for_window_close` keep the (opened, closed) split inline.
    /// Kept `pub(crate)` because the doc comment near the top of this
    /// `impl` block calls it out as the entry point for unit-testing the
    /// diff logic without driving the live window enumerator.
    #[allow(dead_code)]
    pub(crate) fn diff(
        snapshot_ids: &HashSet<u32>,
        current: &[WindowInfo],
    ) -> (Vec<WindowEvent>, Vec<u32>) {
        let current_ids: HashSet<u32> = current.iter().map(|w| w.window_id).collect();
        let opened: Vec<WindowEvent> = current
            .iter()
            .filter(|w| !snapshot_ids.contains(&w.window_id))
            .map(|w| WindowEvent {
                window_id: w.window_id,
                pid: w.pid,
                app_name: w.app_name.clone(),
                title: w.title.clone(),
            })
            .collect();
        let closed: Vec<u32> = snapshot_ids
            .iter()
            .copied()
            .filter(|id| !current_ids.contains(id))
            .collect();
        (opened, closed)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::WindowBounds;

    fn appeared(window_id: u32) -> WindowEvent {
        WindowEvent {
            window_id,
            pid: 42,
            app_name: "TextEdit".into(),
            title: String::new(),
        }
    }

    #[test]
    fn only_accessibility_facts_give_an_appeared_window_a_relation() {
        let sheet = GainedWindow::classify(
            &appeared(7),
            Some(WindowFacts {
                role: Some("AXSheet".into()),
                modal: Some(true),
                parent_window: Some(3),
                ..WindowFacts::default()
            }),
        );
        assert_eq!((sheet.relation, sheet.attached_to), ("sheet", Some(3)));

        let orphan_sheet = GainedWindow::classify(
            &appeared(7),
            Some(WindowFacts {
                role: Some("AXSheet".into()),
                ..WindowFacts::default()
            }),
        );
        assert_eq!((orphan_sheet.relation, orphan_sheet.attached_to), ("unknown", None));

        let dialog = GainedWindow::classify(
            &appeared(8),
            Some(WindowFacts {
                role: Some("AXWindow".into()),
                subrole: Some("AXDialog".into()),
                modal: Some(true),
                // A top-level window's parent is the application, never a window.
                parent_window: None,
            }),
        );
        assert_eq!((dialog.relation, dialog.attached_to), ("app-modal", None));

        let document = GainedWindow::classify(
            &appeared(9),
            Some(WindowFacts {
                role: Some("AXWindow".into()),
                modal: Some(false),
                ..WindowFacts::default()
            }),
        );
        assert_eq!(document.relation, "unknown");

        let unresolved = GainedWindow::classify(&appeared(10), None);
        assert_eq!((unresolved.relation, unresolved.role), ("unknown", None));
    }

    #[test]
    fn gained_windows_are_published_only_when_the_poll_ran() {
        let mut watched = Changes::no_change();
        watched.gained_windows = vec![GainedWindow::classify(&appeared(7), None)];
        let mut structured = serde_json::json!({ "path": "ax" });
        watched.publish_gained_windows(&mut structured);
        let published: Vec<cua_driver_contract::GainedWindow> =
            serde_json::from_value(structured["gained_windows"].clone())
                .expect("the producer's spelling is the contract's");
        assert_eq!(published[0].window_id, 7);

        let mut empty = serde_json::json!({});
        Changes::no_change().publish_gained_windows(&mut empty);
        assert_eq!(empty["gained_windows"], serde_json::json!([]));

        let mut declined = serde_json::json!({});
        Changes::not_polled().publish_gained_windows(&mut declined);
        assert!(declined.get("gained_windows").is_none());
    }

    fn win(id: u32, pid: i32, app: &str, title: &str) -> WindowInfo {
        WindowInfo {
            window_id: id,
            pid,
            app_name: app.into(),
            title: title.into(),
            bounds: WindowBounds {
                x: 0.,
                y: 0.,
                width: 100.,
                height: 100.,
            },
            layer: 0,
            z_index: 0,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn diff_finds_opened_window() {
        let snap: HashSet<u32> = [1, 2].into_iter().collect();
        let cur = vec![
            win(1, 100, "Safari", "Home"),
            win(2, 100, "Safari", "Tab2"),
            win(3, 101, "Mail", "Inbox"),
        ];
        let (opened, closed) = Snapshot::diff(&snap, &cur);
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].window_id, 3);
        assert_eq!(opened[0].app_name, "Mail");
        assert_eq!(opened[0].title, "Inbox");
        assert!(closed.is_empty());
    }

    #[test]
    fn diff_finds_closed_window() {
        let snap: HashSet<u32> = [1, 2, 3].into_iter().collect();
        let cur = vec![win(1, 100, "Safari", "Home")];
        let (opened, closed) = Snapshot::diff(&snap, &cur);
        assert!(opened.is_empty());
        assert_eq!(closed.len(), 2);
        let closed_set: HashSet<u32> = closed.into_iter().collect();
        assert!(closed_set.contains(&2));
        assert!(closed_set.contains(&3));
    }

    #[test]
    fn diff_no_change() {
        let snap: HashSet<u32> = [1, 2].into_iter().collect();
        let cur = vec![win(1, 100, "Safari", "A"), win(2, 100, "Safari", "B")];
        let (opened, closed) = Snapshot::diff(&snap, &cur);
        assert!(opened.is_empty());
        assert!(closed.is_empty());
    }

    #[test]
    fn changes_result_suffix_no_change_is_empty() {
        let c = Changes::no_change();
        assert_eq!(c.result_suffix(), "");
        assert!(!c.needs_restore());
    }

    #[test]
    fn changes_result_suffix_single_new_window_with_title() {
        let c = Changes {
            gained_windows: Vec::new(),
            polled: true,
            new_windows: vec![WindowEvent {
                window_id: 99,
                pid: 100,
                app_name: "Chrome".into(),
                title: "New Tab".into(),
            }],
            foreground_changed: false,
        };
        assert!(c.needs_restore());
        assert_eq!(
            c.result_suffix(),
            "\n\n🪟 Action opened new window(s): Chrome (\"New Tab\")."
        );
    }

    #[test]
    fn changes_result_suffix_groups_windows_by_app() {
        let c = Changes {
            gained_windows: Vec::new(),
            polled: true,
            new_windows: vec![
                WindowEvent {
                    window_id: 1,
                    pid: 100,
                    app_name: "Chrome".into(),
                    title: "Tab A".into(),
                },
                WindowEvent {
                    window_id: 2,
                    pid: 100,
                    app_name: "Chrome".into(),
                    title: "Tab B".into(),
                },
                WindowEvent {
                    window_id: 3,
                    pid: 101,
                    app_name: "Mail".into(),
                    title: "".into(),
                },
            ],
            foreground_changed: true,
        };
        let suffix = c.result_suffix();
        // BTreeMap sort order is alphabetical by app name → Chrome before Mail.
        assert_eq!(
            suffix,
            "\n\n🪟 Action opened new window(s): Chrome (\"Tab A\", \"Tab B\"); Mail."
        );
    }

    #[test]
    fn changes_result_suffix_foreground_change_only() {
        let c = Changes {
            gained_windows: Vec::new(),
            polled: true,
            new_windows: vec![],
            foreground_changed: true,
        };
        assert!(c.needs_restore());
        assert_eq!(
            c.result_suffix(),
            "\n\n🔀 Action caused a different app to become frontmost."
        );
    }

    #[test]
    fn changes_result_suffix_empty_title_is_dropped() {
        let c = Changes {
            gained_windows: Vec::new(),
            polled: true,
            new_windows: vec![WindowEvent {
                window_id: 1,
                pid: 100,
                app_name: "Finder".into(),
                title: "".into(),
            }],
            foreground_changed: false,
        };
        // No title → just the app name, no parentheses.
        assert_eq!(
            c.result_suffix(),
            "\n\n🪟 Action opened new window(s): Finder."
        );
    }

    /// Regression: `snapshot(prior_front)` must store the caller's
    /// captured front pid verbatim (rather than re-reading it inside
    /// the function and racing with concurrent activations).
    #[test]
    fn snapshot_stores_caller_prior_front() {
        // Use an obviously bogus pid so we'd notice if the impl silently
        // fell back to the live frontmost on this test runner.
        let bogus_prior = Some(424242_i32);
        let snap = WindowChangeDetector::snapshot(bogus_prior);
        assert_eq!(snap.front_pid(), bogus_prior);

        // None must round-trip too — and must skip the lease without
        // panicking (no frontmost to restore to).
        let snap_none = WindowChangeDetector::snapshot(None);
        assert_eq!(snap_none.front_pid(), None);
    }
}
