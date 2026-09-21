//! Pure decision logic for scoping an AX walk to one requested CGWindowID.
//!
//! Split out of [`super::tree`] so the decision — which is where issue #2237's
//! silent menu-bar-only "success" came from — is testable without a
//! WindowServer.
//!
//! **Structural invariant:** the walk set is non-empty ONLY for
//! [`WindowScope::Matched`] and [`WindowScope::DesktopSurface`], the two
//! scopes whose content is proven to be the requested window's own. Every
//! failure variant walks nothing, so no unresolved scope can produce a
//! healthy-looking tree of some *other* surface (the app's menu bar) that a
//! caller would then click by `element_index`.

use crate::windows::{WindowBounds, WindowOwner};

/// What the requested `window_id` turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowScope {
    /// An AXWindow or an explicitly attached AXSheet reported the requested CGWindowID.
    Matched,
    /// WindowServer has no record of the requested CGWindowID — closed,
    /// stale, or fabricated.
    NotFound,
    /// The CGWindowID exists but another process owns it. macOS hosts every
    /// sandboxed app's Open/Save panel out-of-process, so this is a routine
    /// shape, not an exotic race.
    OwnerPidMismatch {
        owner_pid: i32,
        owner_app_name: String,
    },
    /// The CGWindowID exists under the requested pid, but no top-level
    /// AXWindow claims it. The screenshot is still the requested window; the
    /// AX surface is not, so callers report a degraded (empty-tree) snapshot
    /// rather than an error.
    AxUnresolved { ax_window_count: usize },
    /// The requested window is a display's desktop surface (WindowServer files
    /// it at `kCGDesktopIconWindowLevel`), whose content AppKit exposes as
    /// children of the application element rather than under any AXWindow —
    /// Finder's `AXScrollArea "desktop"`. The walk covers exactly the
    /// application-level children whose frame lies inside that window's
    /// bounds, plus the menu bar every window-scoped tree carries.
    DesktopSurface { content_children: usize },
}

impl WindowScope {
    /// True when the walk describes the requested window's own content — its
    /// AXWindow, or the application-level surface a desktop window exposes.
    /// Element caches and element-token snapshots may be written only then.
    pub fn is_resolved(&self) -> bool {
        matches!(
            self,
            WindowScope::Matched | WindowScope::DesktopSurface { .. }
        )
    }
}

/// A top-level child of the application AX element, reduced to the two facts
/// the scoping decision needs.
#[derive(Debug, Clone)]
pub struct TopLevelCandidate {
    pub role: String,
    /// AXSubrole for top-level windows. Dialog-like windows must not inherit
    /// the application's menu bar into their window-scoped snapshot.
    pub subrole: Option<String>,
    /// Stable AppKit AX identifier, when exposed. Tahoe's Open/Save panels
    /// report `AXStandardWindow` rather than a dialog subrole, but identify
    /// themselves as `open-panel` / `save-panel`.
    pub identifier: Option<String>,
    /// `_AXUIElementGetWindow` result for an AXWindow or attached AXSheet.
    pub ax_window_id: Option<u32>,
    /// `AXModal` — the application's own report that this window blocks every
    /// other window of its process. `None` when it was not read or not
    /// answered, which is unknown, never "not modal".
    pub modal: Option<bool>,
}

impl TopLevelCandidate {
    pub fn new(role: impl Into<String>, ax_window_id: Option<u32>) -> Self {
        Self {
            role: role.into(),
            subrole: None,
            identifier: None,
            ax_window_id,
            modal: None,
        }
    }

    pub fn with_subrole(mut self, subrole: impl Into<String>) -> Self {
        self.subrole = Some(subrole.into());
        self
    }

    pub fn with_identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    pub fn with_modal(mut self, modal: bool) -> Self {
        self.modal = Some(modal);
        self
    }

    pub fn is_dialog_like(&self) -> bool {
        self.role == "AXSheet"
            || self
                .subrole
                .as_deref()
                .is_some_and(|s| matches!(s, "AXDialog" | "AXSystemDialog" | "AXSheet"))
            || self
                .identifier
                .as_deref()
                .is_some_and(|id| matches!(id, "open-panel" | "save-panel"))
    }

    /// A top-level dialog window the application reports modal: while it is
    /// up, no window of that process — including the one being observed — can
    /// accept input, so it belongs to that window's observation. Attached
    /// sheets keep their own related-surface path ([`is_related_sheet`]), so
    /// this is deliberately the top-level case only.
    pub fn is_app_modal_dialog(&self) -> bool {
        self.role == "AXWindow" && self.modal == Some(true) && self.is_dialog_like()
    }
}

/// The scope conclusion plus which candidates to walk at depth 0.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeDecision {
    pub scope: WindowScope,
    /// Indices into the candidate slice. Empty for every non-`Matched` scope.
    pub walk: Vec<usize>,
    /// Same-pid top-level dialog windows the application reports modal, which
    /// are not the requested window. Empty for every non-`Matched` scope: an
    /// unresolved request describes nothing.
    pub modal: Vec<usize>,
}

/// The scope conclusion reachable from CGWindowList alone, before any AX work.
///
/// `None` means the window exists under the requested pid, so only the AX walk
/// can decide between [`WindowScope::Matched`] and
/// [`WindowScope::AxUnresolved`].
pub fn scope_from_owner(owner: &WindowOwner) -> Option<WindowScope> {
    match owner {
        WindowOwner::SamePid => None,
        WindowOwner::Unknown => Some(WindowScope::NotFound),
        WindowOwner::ForeignPid {
            owner_pid,
            owner_app_name,
        } => Some(WindowScope::OwnerPidMismatch {
            owner_pid: *owner_pid,
            owner_app_name: owner_app_name.clone(),
        }),
    }
}

pub const SHEET_RELATION: &str = "sheet";

/// A same-pid modal dialog is not attached to the observed window, but it
/// owns every input the process can accept while it is up.
pub const MODAL_RELATION: &str = "app-modal";

/// An independently mapped attached sheet has its own control scope. Its
/// subtree must not mint tokens for the document window being observed.
pub fn is_related_sheet(role: &str, requested: Option<u32>, mapped: Option<u32>) -> bool {
    role == "AXSheet"
        && matches!((requested, mapped), (Some(parent), Some(sheet)) if parent != sheet)
}

/// Decide what a window-scoped walk of `candidates` should cover.
///
/// `resolve_owner` is only invoked when no candidate claims `requested` — the
/// CGWindowList enumeration it performs is wasted work on the happy path.
pub fn decide_window_scope<F>(
    candidates: &[TopLevelCandidate],
    requested: u32,
    resolve_owner: F,
) -> ScopeDecision
where
    F: FnOnce() -> WindowOwner,
{
    let matched: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            matches!(c.role.as_str(), "AXWindow" | "AXSheet") && c.ax_window_id == Some(requested)
        })
        .map(|(i, _)| i)
        .collect();

    if matched.is_empty() {
        // Nothing claims the id. Ask WindowServer *why*: a fabricated/stale id
        // and a live id owned by another process are different refusals, and a
        // live same-pid id whose AX surface never materialised is not a refusal
        // at all.
        let scope = scope_from_owner(&resolve_owner()).unwrap_or(WindowScope::AxUnresolved {
            ax_window_count: candidates.iter().filter(|c| c.role == "AXWindow").count(),
        });
        return ScopeDecision {
            scope,
            walk: Vec::new(),
            modal: Vec::new(),
        };
    }

    // Matched: walk the requested window plus the non-window top-level children.
    //
    // The menu bar is deliberately among them. MACOS.md documents a
    // two-snapshot menu flow whose first step reads `AXMenuBarItem` rows
    // straight out of this window-scoped tree, and `get_window_state` is the
    // only tool that returns a subtree — dropping AXMenuBar here would remove
    // menu navigation with no replacement path. Keeping other non-window
    // children is also what `browser/consent_ui.rs` relies on to reach a
    // top-level `AXSheet` consent prompt.
    let sheet_scope = matched.iter().any(|&i| candidates[i].role == "AXSheet");
    let dialog_scope = sheet_scope || matched.iter().any(|&i| candidates[i].is_dialog_like());
    let walk = candidates
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            (!sheet_scope || matched.contains(i))
                && (matched.contains(i)
                    || (c.role != "AXWindow" && !(c.role == "AXSheet" && c.ax_window_id.is_some())))
                && !(dialog_scope && c.role == "AXMenuBar")
        })
        .map(|(i, _)| i)
        .collect();
    // A modal dialog of this process blocks the requested window whether or
    // not it is attached to it, so the observation has to show it: an agent
    // that cannot see it reads an unchanging window and calls its own landed
    // actions no-ops.
    let modal = candidates
        .iter()
        .enumerate()
        .filter(|(i, c)| !matched.contains(i) && c.is_app_modal_dialog())
        .map(|(i, _)| i)
        .collect();
    ScopeDecision {
        scope: WindowScope::Matched,
        walk,
        modal,
    }
}

/// Slack, in points, when deciding whether an AX frame lies inside a
/// CGWindow's bounds: AppKit reports AX frames as floats derived from view
/// geometry while WindowServer reports integral window bounds, so an edge can
/// land a rounding step outside the window it belongs to.
const FRAME_TOLERANCE_POINTS: f64 = 1.0;

/// Whether `inner` lies inside `outer`, up to [`FRAME_TOLERANCE_POINTS`].
fn lies_within(outer: &WindowBounds, inner: &WindowBounds) -> bool {
    let slack = FRAME_TOLERANCE_POINTS;
    inner.x + slack >= outer.x
        && inner.y + slack >= outer.y
        && inner.x + inner.width <= outer.x + outer.width + slack
        && inner.y + inner.height <= outer.y + outer.height + slack
}

/// Whether a top-level candidate may count as a desktop surface's content.
///
/// The frame rule is only ever applied to children that carry no window
/// identity of their own: an `AXWindow` whose id the SPI could not map is
/// "not the requested window" by the fail-closed rule, a mapped sheet is a
/// sibling window, and the menu bar is process-scoped and admitted separately
/// (below) for the same reason every window-scoped tree carries it.
fn eligible_desktop_content(candidate: &TopLevelCandidate) -> bool {
    candidate.role != "AXWindow"
        && candidate.role != "AXMenuBar"
        && !(candidate.role == "AXSheet" && candidate.ax_window_id.is_some())
}

/// Decide what a walk of a desktop surface should cover, once
/// [`decide_window_scope`] found no AXWindow claiming the id and WindowServer
/// filed the id as a desktop surface (`is_desktop_surface`) owned by the
/// requested pid.
///
/// AppKit exposes a desktop's content as children of the application element
/// (Finder: `AXScrollArea "desktop"` → `AXGroup` → one `AXImage` per icon),
/// listed under `AXWindows` but never as an `AXWindow`, so no CGWindowID can
/// be mapped from it. Identity is therefore proven from the two facts that do
/// exist: the process owns the window, and the child's frame lies inside the
/// window's bounds. Two displays have two desktop surfaces with disjoint
/// bounds, so each application-level surface belongs to exactly one.
///
/// `frame_of` is read lazily, only for eligible children. `None` (an
/// unreadable frame) never qualifies a child. When nothing qualifies the
/// answer stays [`WindowScope::AxUnresolved`] with an empty walk.
pub fn decide_desktop_surface_scope<F>(
    candidates: &[TopLevelCandidate],
    bounds: &WindowBounds,
    mut frame_of: F,
) -> ScopeDecision
where
    F: FnMut(usize) -> Option<WindowBounds>,
{
    let content: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(index, candidate)| {
            eligible_desktop_content(candidate)
                && frame_of(*index).is_some_and(|frame| lies_within(bounds, &frame))
        })
        .map(|(index, _)| index)
        .collect();
    if content.is_empty() {
        return ScopeDecision {
            scope: WindowScope::AxUnresolved {
                ax_window_count: candidates.iter().filter(|c| c.role == "AXWindow").count(),
            },
            walk: Vec::new(),
            modal: Vec::new(),
        };
    }
    // Content plus the menu bar, in candidate order — the same process-scoped
    // menu-bar rule a matched non-dialog window follows, so the two-snapshot
    // menu flow works from the desktop too (File ▸ New Folder).
    let walk = candidates
        .iter()
        .enumerate()
        .filter(|(index, candidate)| candidate.role == "AXMenuBar" || content.contains(index))
        .map(|(index, _)| index)
        .collect();
    ScopeDecision {
        scope: WindowScope::DesktopSurface {
            content_children: content.len(),
        },
        walk,
        modal: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_sheet_requires_an_independent_mapped_identity() {
        assert!(is_related_sheet("AXSheet", Some(11), Some(22)));
        assert!(!is_related_sheet("AXSheet", Some(22), Some(22)));
        assert!(!is_related_sheet("AXSheet", Some(11), None));
        assert!(!is_related_sheet("AXSheet", None, Some(22)));
        assert!(!is_related_sheet("AXWindow", Some(11), Some(22)));
        assert!(!is_related_sheet("AXButton", Some(11), Some(22)));
    }

    fn panel_service_owner() -> WindowOwner {
        WindowOwner::ForeignPid {
            owner_pid: 900,
            owner_app_name: "Open and Save Panel Service".into(),
        }
    }

    fn never_called() -> WindowOwner {
        panic!("owner lookup must be skipped when a candidate claims the id");
    }

    /// The MACOS.md contract: a resolved normal window keeps the menu bar in
    /// its window-scoped tree, so the documented two-snapshot menu flow
    /// (`AXMenuBarItem` → menu item) still works.
    #[test]
    fn matched_window_walks_window_and_menu_bar() {
        let candidates = [
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", Some(11)),
            TopLevelCandidate::new("AXWindow", Some(22)),
        ];
        let d = decide_window_scope(&candidates, 22, never_called);
        assert_eq!(d.scope, WindowScope::Matched);
        assert_eq!(d.walk, vec![0, 2], "menu bar + requested window only");
    }

    #[test]
    fn matched_dialog_excludes_the_application_menu_bar() {
        let candidates = [
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", Some(11)).with_subrole("AXStandardWindow"),
            TopLevelCandidate::new("AXWindow", Some(22)).with_subrole("AXDialog"),
        ];
        let d = decide_window_scope(&candidates, 22, never_called);
        assert_eq!(d.scope, WindowScope::Matched);
        assert_eq!(d.walk, vec![2], "dialog snapshot must not carry AXMenuBar");
    }

    #[test]
    fn matched_appkit_file_panel_excludes_the_application_menu_bar() {
        let candidates = [
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", Some(11)).with_subrole("AXStandardWindow"),
            TopLevelCandidate::new("AXWindow", Some(22))
                .with_subrole("AXStandardWindow")
                .with_identifier("open-panel"),
        ];
        let d = decide_window_scope(&candidates, 22, never_called);
        assert_eq!(d.scope, WindowScope::Matched);
        assert_eq!(
            d.walk,
            vec![2],
            "AppKit file-panel snapshot must not carry AXMenuBar"
        );
    }

    /// A modal dialog of the same process blocks the observed window, so it
    /// comes back beside it. The observation of a window that cannot accept
    /// input must name what is holding it.
    #[test]
    fn a_same_pid_modal_dialog_accompanies_the_observed_window() {
        let candidates = [
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", Some(22)),
            TopLevelCandidate::new("AXWindow", Some(31))
                .with_subrole("AXDialog")
                .with_modal(true),
        ];
        let d = decide_window_scope(&candidates, 22, never_called);
        assert_eq!(d.scope, WindowScope::Matched);
        assert_eq!(d.walk, vec![0, 1], "the dialog is not part of the scope");
        assert_eq!(d.modal, vec![2]);
    }

    /// Modality is the application's own report, and only a dialog-shaped
    /// top-level window carries it here: an ordinary sibling window, a dialog
    /// the app does not call modal, and an attached sheet (which has its own
    /// related-surface path) are all left alone.
    #[test]
    fn only_a_window_the_app_calls_modal_is_carried_along() {
        let candidates = [
            TopLevelCandidate::new("AXWindow", Some(22)),
            TopLevelCandidate::new("AXWindow", Some(30)),
            TopLevelCandidate::new("AXWindow", Some(31)).with_subrole("AXDialog"),
            TopLevelCandidate::new("AXWindow", Some(32))
                .with_subrole("AXDialog")
                .with_modal(false),
            TopLevelCandidate::new("AXSheet", Some(33)).with_modal(true),
            TopLevelCandidate::new("AXWindow", Some(34))
                .with_subrole("AXStandardWindow")
                .with_identifier("save-panel")
                .with_modal(true),
        ];
        let d = decide_window_scope(&candidates, 22, never_called);
        assert_eq!(
            d.modal,
            vec![5],
            "a modal file panel blocks the process just as a modal alert does"
        );
    }

    /// The dialog itself may be the requested window. It is then the scope,
    /// never also its own blocker.
    #[test]
    fn the_requested_modal_dialog_is_not_reported_beside_itself() {
        let candidates = [
            TopLevelCandidate::new("AXWindow", Some(22)),
            TopLevelCandidate::new("AXWindow", Some(31))
                .with_subrole("AXDialog")
                .with_modal(true),
        ];
        let d = decide_window_scope(&candidates, 31, never_called);
        assert_eq!(d.walk, vec![1]);
        assert!(d.modal.is_empty(), "{:?}", d.modal);
    }

    /// An unresolved request describes nothing — including what is modal.
    #[test]
    fn an_unresolved_request_reports_no_modal_dialog() {
        let candidates = [TopLevelCandidate::new("AXWindow", Some(31))
            .with_subrole("AXDialog")
            .with_modal(true)];
        let d = decide_window_scope(&candidates, 22, panel_service_owner);
        assert!(d.walk.is_empty());
        assert!(d.modal.is_empty(), "{:?}", d.modal);
    }

    /// Issue #2237's headline defect: an id nothing claims used to fall through
    /// to `[AXMenuBar, ...]` and return it as a healthy snapshot.
    #[test]
    fn unresolved_id_is_not_a_menu_bar_success() {
        let candidates = [
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", Some(11)),
        ];
        let d = decide_window_scope(&candidates, 67340, || WindowOwner::SamePid);
        assert_eq!(d.scope, WindowScope::AxUnresolved { ax_window_count: 1 });
        assert!(d.walk.is_empty(), "must not walk the menu bar instead");
    }

    #[test]
    fn foreign_id_reports_the_owner_pid() {
        let candidates = [
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", Some(11)),
        ];
        let d = decide_window_scope(&candidates, 67340, panel_service_owner);
        assert_eq!(
            d.scope,
            WindowScope::OwnerPidMismatch {
                owner_pid: 900,
                owner_app_name: "Open and Save Panel Service".into(),
            }
        );
        assert!(d.walk.is_empty());
    }

    #[test]
    fn stale_id_is_not_found() {
        let candidates = [TopLevelCandidate::new("AXWindow", Some(11))];
        let d = decide_window_scope(&candidates, 67340, || WindowOwner::Unknown);
        assert_eq!(d.scope, WindowScope::NotFound);
        assert!(d.walk.is_empty());
    }

    /// A top-level `AXSheet` (or any non-window child) must not act as a
    /// wildcard that makes an unmatched id look resolved.
    #[test]
    fn top_level_sheet_without_the_id_is_not_a_wildcard() {
        let candidates = [
            TopLevelCandidate::new("AXSheet", None),
            TopLevelCandidate::new("AXMenuBar", None),
        ];
        let d = decide_window_scope(&candidates, 67340, || WindowOwner::SamePid);
        assert_eq!(d.scope, WindowScope::AxUnresolved { ax_window_count: 0 });
        assert!(d.walk.is_empty());
    }

    #[test]
    fn mapped_sheet_is_scoped_without_parent_sibling_or_menu_controls() {
        let candidates = [
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", Some(11)),
            TopLevelCandidate::new("AXSheet", Some(22)),
            TopLevelCandidate::new("AXSheet", Some(33)),
            TopLevelCandidate::new("AXSheet", None),
        ];
        let sheet = decide_window_scope(&candidates, 22, never_called);
        assert_eq!(sheet.scope, WindowScope::Matched);
        assert_eq!(sheet.walk, vec![2]);
        let parent = decide_window_scope(&candidates, 11, never_called);
        assert_eq!(parent.walk, vec![0, 1, 4]);
        let missing = decide_window_scope(&candidates, 44, || WindowOwner::SamePid);
        assert!(!missing.scope.is_resolved());
        assert!(missing.walk.is_empty());
    }

    /// A resolved window still carries its sibling sheets — `consent_ui.rs`
    /// walks Chrome's other window ids expecting the consent `AXSheet` as a
    /// top-level non-window child.
    #[test]
    fn matched_window_keeps_sibling_sheets() {
        let candidates = [
            TopLevelCandidate::new("AXSheet", None),
            TopLevelCandidate::new("AXWindow", Some(11)),
        ];
        let d = decide_window_scope(&candidates, 11, never_called);
        assert_eq!(d.scope, WindowScope::Matched);
        assert_eq!(d.walk, vec![0, 1]);
    }

    #[test]
    fn menu_bar_only_app_has_no_ax_windows() {
        let candidates = [TopLevelCandidate::new("AXMenuBar", None)];
        let d = decide_window_scope(&candidates, 11, || WindowOwner::SamePid);
        assert_eq!(d.scope, WindowScope::AxUnresolved { ax_window_count: 0 });
        assert!(d.walk.is_empty());
    }

    #[test]
    fn empty_candidate_list_walks_nothing() {
        for owner in [
            WindowOwner::SamePid,
            WindowOwner::Unknown,
            panel_service_owner(),
        ] {
            let d = decide_window_scope(&[], 11, || owner.clone());
            assert!(d.walk.is_empty());
            assert!(!d.scope.is_resolved());
        }
    }

    /// The invariant that makes the reported bug unregressable: no failure
    /// variant may ever hand back a walk set.
    #[test]
    fn menu_bar_is_never_walked_alone() {
        let candidates = [
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXSheet", None),
            TopLevelCandidate::new("AXWindow", Some(11)),
            // An AXWindow whose window-id SPI failed — the ViewBridge-remoted
            // panel shape.
            TopLevelCandidate::new("AXWindow", None),
        ];
        for owner in [
            WindowOwner::SamePid,
            WindowOwner::Unknown,
            panel_service_owner(),
        ] {
            let d = decide_window_scope(&candidates, 67340, || owner.clone());
            assert!(
                !d.scope.is_resolved(),
                "id 67340 is claimed by nothing; scope was {:?}",
                d.scope
            );
            assert!(
                d.walk.is_empty(),
                "failure scope {:?} must walk nothing, got {:?}",
                d.scope,
                d.walk
            );
        }
    }

    #[test]
    fn scope_from_owner_defers_only_for_same_pid() {
        assert_eq!(scope_from_owner(&WindowOwner::SamePid), None);
        assert_eq!(
            scope_from_owner(&WindowOwner::Unknown),
            Some(WindowScope::NotFound)
        );
        assert!(matches!(
            scope_from_owner(&panel_service_owner()),
            Some(WindowScope::OwnerPidMismatch { owner_pid: 900, .. })
        ));
    }

    // ── Desktop surface ─────────────────────────────────────────────────────

    fn rect(x: f64, y: f64, width: f64, height: f64) -> WindowBounds {
        WindowBounds {
            x,
            y,
            width,
            height,
        }
    }

    /// Finder's application element as measured on a one-display Mac: the
    /// menu bar and the desktop scroll area (which also appears under
    /// `AXWindows`, deduplicated by the walker before scoping).
    fn finder_candidates() -> Vec<TopLevelCandidate> {
        vec![
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXScrollArea", None),
        ]
    }

    /// Frames as Finder reports them for [`finder_candidates`]: the menu bar
    /// strip and the full-display scroll area.
    fn finder_frames(index: usize) -> Option<WindowBounds> {
        match index {
            0 => Some(rect(0., 0., 1728., 33.)),
            1 => Some(rect(0., 0., 1728., 1117.)),
            _ => None,
        }
    }

    fn display() -> WindowBounds {
        rect(0., 0., 1728., 1117.)
    }

    #[test]
    fn desktop_surface_walks_the_in_frame_content_and_the_menu_bar() {
        let candidates = finder_candidates();
        let d = decide_desktop_surface_scope(&candidates, &display(), finder_frames);
        assert_eq!(
            d.scope,
            WindowScope::DesktopSurface {
                content_children: 1
            }
        );
        assert_eq!(d.walk, vec![0, 1], "menu bar + desktop scroll area");
        assert!(d.scope.is_resolved());
    }

    /// The plain unresolved path stays exactly what it was: only the desktop
    /// step, which the walker takes after a WindowServer desktop fact, can
    /// widen the walk.
    #[test]
    fn without_the_desktop_fact_the_same_candidates_stay_unresolved() {
        let d = decide_window_scope(&finder_candidates(), 9814, || WindowOwner::SamePid);
        assert_eq!(d.scope, WindowScope::AxUnresolved { ax_window_count: 0 });
        assert!(d.walk.is_empty());
        assert!(!d.scope.is_resolved());
    }

    /// Two displays: two desktop surfaces with disjoint bounds and two
    /// application-level scroll areas. Each window reads only its own.
    #[test]
    fn a_second_displays_desktop_content_is_not_this_windows() {
        let candidates = vec![
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXScrollArea", None),
            TopLevelCandidate::new("AXScrollArea", None),
        ];
        let frames = |index: usize| match index {
            0 => Some(rect(0., 0., 1728., 33.)),
            1 => Some(rect(0., 0., 1728., 1117.)),
            2 => Some(rect(1728., 0., 2560., 1440.)),
            _ => None,
        };
        let primary = decide_desktop_surface_scope(&candidates, &display(), frames);
        assert_eq!(primary.walk, vec![0, 1]);
        let secondary =
            decide_desktop_surface_scope(&candidates, &rect(1728., 0., 2560., 1440.), frames);
        assert_eq!(secondary.walk, vec![0, 2]);
    }

    /// A frame that pokes out of the window by more than rounding slack is
    /// another surface's; one exactly on the edge, or within a point, is not.
    #[test]
    fn frame_containment_allows_rounding_but_not_overlap() {
        let candidates = vec![TopLevelCandidate::new("AXGroup", None)];
        for (frame, inside) in [
            (rect(0., 0., 1728., 1117.), true),
            (rect(-0.5, 0., 1728.5, 1117.5), true),
            (rect(0., 0., 1728., 1118.5), false),
            (rect(-2., 0., 1728., 1117.), false),
            (rect(100., 100., 200., 200.), true),
            (rect(1700., 1100., 200., 200.), false),
        ] {
            let d = decide_desktop_surface_scope(&candidates, &display(), |_| Some(frame.clone()));
            assert_eq!(
                d.scope.is_resolved(),
                inside,
                "frame {frame:?} inside={inside}"
            );
        }
    }

    /// Window-identified children never enter by frame: an AXWindow whose id
    /// the SPI could not map is not the requested window (fail closed), and a
    /// mapped sheet is a sibling window. Only unidentified content qualifies.
    #[test]
    fn window_identified_children_are_never_admitted_by_frame() {
        let candidates = vec![
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", None),
            TopLevelCandidate::new("AXWindow", Some(16026)),
            TopLevelCandidate::new("AXSheet", Some(17000)),
        ];
        let d = decide_desktop_surface_scope(&candidates, &display(), |_| Some(display()));
        assert_eq!(d.scope, WindowScope::AxUnresolved { ax_window_count: 2 });
        assert!(
            d.walk.is_empty(),
            "no content → nothing walked, menu bar included"
        );

        // An unmapped sheet or an open menu inside the frame is process-level
        // transient UI, carried the way a matched window carries it.
        let candidates = vec![
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXScrollArea", None),
            TopLevelCandidate::new("AXMenu", None),
            TopLevelCandidate::new("AXSheet", None),
        ];
        let d = decide_desktop_surface_scope(&candidates, &display(), |index| match index {
            1 => Some(display()),
            2 => Some(rect(400., 300., 200., 240.)),
            3 => Some(rect(600., 200., 500., 300.)),
            _ => None,
        });
        assert_eq!(
            d.scope,
            WindowScope::DesktopSurface {
                content_children: 3
            }
        );
        assert_eq!(d.walk, vec![0, 1, 2, 3]);
    }

    /// The menu bar alone is never a desktop's content: the same rule that
    /// keeps #2237's menu-bar-only "success" out of the matched path.
    #[test]
    fn menu_bar_alone_is_not_a_desktop_surface() {
        let candidates = vec![TopLevelCandidate::new("AXMenuBar", None)];
        let d = decide_desktop_surface_scope(&candidates, &display(), |_| {
            Some(rect(0., 0., 1728., 33.))
        });
        assert_eq!(d.scope, WindowScope::AxUnresolved { ax_window_count: 0 });
        assert!(d.walk.is_empty());
    }

    /// An unreadable frame proves nothing, so it admits nothing.
    #[test]
    fn an_unreadable_frame_never_qualifies_content() {
        let candidates = finder_candidates();
        let d = decide_desktop_surface_scope(&candidates, &display(), |_| None);
        assert_eq!(d.scope, WindowScope::AxUnresolved { ax_window_count: 0 });
        assert!(d.walk.is_empty());
    }

    /// Frames are read only for children the role rule lets through.
    #[test]
    fn frames_are_read_only_for_eligible_children() {
        let candidates = vec![
            TopLevelCandidate::new("AXMenuBar", None),
            TopLevelCandidate::new("AXWindow", Some(16026)),
            TopLevelCandidate::new("AXScrollArea", None),
        ];
        let mut read = Vec::new();
        decide_desktop_surface_scope(&candidates, &display(), |index| {
            read.push(index);
            Some(display())
        });
        assert_eq!(read, vec![2]);
    }
}
