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
//!   be mistaken for the click landing,
//! * the accessory windows the application has on screen — an open `NSMenu`
//!   or a popover is drawn in its own window, outside the target window's
//!   subtree, so none of the three signals above moves when one appears.
//!
//! Unchanged across every usable signal is reported as such and escalated to
//! the foreground rung. It is never converted into a retry, a route switch, or
//! an error: a dispatched action may have taken effect invisibly, and repeating
//! it could act twice. Which of the signals above were actually comparable
//! varies per call — an action addressed by pixel watches no element of its
//! own, an unquiescent window has no digest — so the reply names the ones that
//! were compared and the ones that could not be read, and claims nothing about
//! the rest.

use std::time::{Duration, Instant};

use crate::ax::bindings::{
    children_count, copy_bool_attr, copy_string_attr, element_screen_rect,
    focused_element_of_pid, kAXErrorInvalidUIElement, try_copy_string_attr, AXUIElementRef,
};
use crate::ax::RetainedElement;

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
/// The same window for a probe the caller passed no element to — a chord, or
/// a press whose focused element would not read. The budget above is sized
/// for the slowest signal this probe has, a control's own state (~1.3 s on
/// Contacts' add button), and that signal needs an element pointer: what is
/// left reads the application, not the control it was aimed at. So the blind
/// wait is capped at the 500 ms a single sample used to be taken at, and a
/// chord nothing reacted to pays a quarter of the full budget instead of all
/// of it. Lowering it further is a `SETTLE_POLL` question, not this one: the
/// loop returns on the first sample that differs.
///
/// A key-forwarding client's window (Screen Sharing, iPhone Mirroring) shows
/// another device's screen: what an action does there never reaches this
/// Mac's accessibility tree, so a readable element there is no better than
/// none and pays this budget too. Measured on iPhone Mirroring: every key
/// press and toolbar AX press waited out the full 2 s and still reported
/// nothing.
const BLIND_SETTLE_BUDGET: Duration = Duration::from_millis(500);
/// Gap between post-dispatch samples. Each sample already costs one or more
/// native AX reads, so this only keeps a cheap sample set from spinning.
const SETTLE_POLL: Duration = Duration::from_millis(50);
/// How many extra pre-dispatch captures a window the driver just activated
/// gets before its digest is treated as self-updating, and the gap between
/// them.
const ACTIVATION_SETTLE_ATTEMPTS: usize = 3;
const ACTIVATION_SETTLE_GAP: Duration = Duration::from_millis(100);

/// What one read of the watched element answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ElementRead {
    /// Identity + mutable state, comparable against another `State`.
    State(String),
    /// The element reported itself invalid: it was destroyed. Only reachable
    /// because the probe holds its own reference — an unretained pointer to a
    /// destroyed element traps instead of answering.
    Gone,
    /// No comparable answer (attribute missing, app busy, AX timeout).
    #[default]
    Unreadable,
}

impl ElementRead {
    fn state(&self) -> Option<&str> {
        match self {
            ElementRead::State(state) => Some(state),
            _ => None,
        }
    }
}

/// One sample of the signals the probe can compare. `None` means "not
/// readable", which is never treated as a change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals {
    /// Identity + mutable state of the watched element.
    pub element: ElementRead,
    /// Identity of the application's AX focused element.
    pub focus: Option<String>,
    /// Digest of the target window's capped AX subtree.
    pub tree: Option<u64>,
    /// CGWindowIDs of the accessory surfaces the application has on screen.
    ///
    /// An open `NSMenu` is one of them, and so is a popover: both are drawn
    /// in their own window outside the target window's AX subtree, so none
    /// of the three signals above moves when one appears. Compared in one
    /// direction only — a surface gained is a reaction, a surface lost is
    /// not, because the driver's own foreground handling can dismiss one.
    pub menus: Vec<u32>,
}

/// What the probe observed after the dispatch; a reaction names the signal
/// that moved, which is the `kind` its evidence row publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// A usable signal differed after the dispatch; the app reacted.
    Changed(&'static str),
    /// The watched element no longer exists — a dismissal (Escape on a
    /// popover, sheet or menu) destroys the element it dismisses, so this is
    /// the reaction itself, not a failed read. It publishes as the
    /// `element_state` signal: the element's own state is what was watched,
    /// and the reply's prose says it is gone.
    ElementGone,
    /// Every usable signal was identical after the dispatch.
    Unchanged,
    /// Nothing could be compared (element unreadable, window unreadable, and
    /// a self-updating or unreadable subtree).
    Unusable,
}

/// The window poll's own signal: a window opened, or the frontmost
/// application changed. Only this signal carries observed windows.
pub const WINDOW_SIGNAL: &str = "window_change";
/// The watched element's own state. Spelled as the action contract publishes
/// it — an unpublished spelling is dropped from the action record, so the
/// probe only ever names signals the contract knows.
pub const ELEMENT_SIGNAL: &str = "element_state";
/// The application's focused element moved.
pub const FOCUS_SIGNAL: &str = "app_focus";
/// The target window's capped subtree digest differed.
pub const TREE_SIGNAL: &str = "window_tree";
/// The application put a menu or popover on screen. Additive over contract
/// 0.10's four signals; like them it refutes "the target did not react" and
/// claims nothing about the caller's intended postcondition.
pub const MENU_SIGNAL: &str = "menu_opened";

impl Evidence {
    pub fn signal(self) -> &'static str {
        match self {
            Evidence::Changed(signal) => signal,
            Evidence::ElementGone => ELEMENT_SIGNAL,
            Evidence::Unchanged => "none",
            Evidence::Unusable => "unavailable",
        }
    }

    /// The target reacted. Ends the settle wait and publishes evidence.
    pub fn is_reaction(self) -> bool {
        matches!(self, Evidence::Changed(_) | Evidence::ElementGone)
    }
}

/// Which of the probe's signals were comparable across this dispatch.
///
/// A signal nobody could read refutes nothing, so "nothing changed" has to
/// carry its own scope: a caller that passed no element pointer never watched
/// an element, and an unquiescent window has no digest to compare. Derived
/// from the samples the verdict was reached on, by the same predicates
/// [`classify`] uses — never from what the calling tool usually watches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Watched {
    /// The watched element answered a comparable state before the dispatch,
    /// and either answered one after it or reported itself destroyed.
    pub element: bool,
    /// The application's focused element answered on both samples.
    pub focus: bool,
    /// The window's subtree digest was comparable: the window proved
    /// quiescent and both walks finished.
    pub tree: bool,
    /// The application's accessory windows were compared for a surface
    /// gained. Read from WindowServer, which answers for any pid, so an
    /// empty set is a real answer rather than a failed read.
    pub accessory: bool,
}

impl Watched {
    /// What a sample pair let [`classify`] compare.
    fn over(before: &Signals, after: &Signals, quiescent: bool) -> Self {
        Self {
            element: before.element.state().is_some()
                && matches!(after.element, ElementRead::State(_) | ElementRead::Gone),
            focus: before.focus.is_some() && after.focus.is_some(),
            tree: quiescent && before.tree.is_some() && after.tree.is_some(),
            accessory: true,
        }
    }

    /// What the pre-dispatch sample alone offers, for a verdict reached
    /// before any post-dispatch sample was taken ([`DeliveryProbe::settled`]).
    fn offered(before: &Signals, quiescent: bool) -> Self {
        Self {
            element: before.element.state().is_some(),
            focus: before.focus.is_some(),
            tree: quiescent && before.tree.is_some(),
            accessory: true,
        }
    }

    /// The signals that were compared, spelled as the action contract
    /// publishes them. `polled` is the caller's own window watch: the probe
    /// cannot see a window opening, so it names that signal only when the
    /// caller asked for it.
    pub fn compared(self, polled: bool) -> Vec<&'static str> {
        [
            (self.element, ELEMENT_SIGNAL),
            (self.focus, FOCUS_SIGNAL),
            (self.tree, TREE_SIGNAL),
            (self.accessory, MENU_SIGNAL),
            (polled, WINDOW_SIGNAL),
        ]
        .into_iter()
        .filter_map(|(compared, signal)| compared.then_some(signal))
        .collect()
    }

    /// The signals the probe tried to read and could not, so a change there
    /// would not have been seen. The caller's window watch is not in here: a
    /// poll it declined is a choice, not a failed read.
    pub fn unread(self) -> Vec<&'static str> {
        [
            (self.element, ELEMENT_SIGNAL),
            (self.focus, FOCUS_SIGNAL),
            (self.tree, TREE_SIGNAL),
        ]
        .into_iter()
        .filter_map(|(compared, signal)| (!compared).then_some(signal))
        .collect()
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
    /// Which signals this verdict is a verdict over. A "nothing reacted"
    /// report names them, so the claim cannot be read wider than it is.
    pub watched: Watched,
}

/// Pre-dispatch capture. Hold it across the action, then `compare()`.
pub struct DeliveryProbe {
    pid: i32,
    window_id: u32,
    /// The watched element, retained for the probe's whole life. The action
    /// being probed may destroy it — Escape dismisses the popover, sheet or
    /// menu whose element it was — and a post-dispatch read of an unretained
    /// pointer to a destroyed element traps inside CoreFoundation instead of
    /// answering. Owning a reference turns that into
    /// `kAXErrorInvalidUIElement`, which is a verdict rather than a crash.
    element: Option<RetainedElement>,
    before: Signals,
    /// The window's subtree digest was identical across two pre-dispatch
    /// samples, so a post-dispatch difference is attributable to the action.
    quiescent: bool,
    /// The window has no accessibility content, so its subtree is never
    /// sampled (see [`window_has_no_ax_content`]).
    tree_skipped: bool,
    /// The target is a key-forwarding client (see [`BLIND_SETTLE_BUDGET`]).
    remote_screen: bool,
    elapsed: Duration,
}

impl DeliveryProbe {
    /// Sample the target before dispatch. Blocking AX work — call from a
    /// blocking thread. `element_ptr` must be live for this call; the probe
    /// retains it for itself, so the caller's own reference may go away
    /// afterwards.
    pub fn capture(pid: i32, window_id: u32, element_ptr: Option<usize>) -> Self {
        let start = Instant::now();
        // SAFETY: the caller's contract is a live pointer at capture time,
        // which is exactly what `retain` needs.
        let element = element_ptr.map(|ptr| unsafe { RetainedElement::retain(ptr) });
        let state = element
            .as_ref()
            .map_or(ElementRead::Unreadable, element_state);
        let focus = focus_state(pid);
        let menus = menu_state(pid);
        // A window whose accessibility tree has no content (a pixel-only
        // surface such as a mirrored phone screen) digests to noise: its
        // subtree is not evidence either way, so it is not sampled.
        let tree_skipped = window_has_no_ax_content(pid, window_id);
        let (first, second) = if tree_skipped {
            (None, None)
        } else {
            (tree_digest(pid, window_id), tree_digest(pid, window_id))
        };
        let quiescent = matches!((first, second), (Some(a), Some(b)) if a == b);
        Self {
            pid,
            window_id,
            element,
            before: Signals {
                element: state,
                focus,
                tree: second,
                menus,
            },
            quiescent,
            tree_skipped,
            remote_screen: crate::input::keyboard::is_key_forwarding_pid(pid),
            elapsed: start.elapsed(),
        }
    }

    /// `capture` for a window this driver has just made key itself. The
    /// activation moves focus and redraws the window over the next few
    /// hundred milliseconds, so a first pair of samples that disagree is the
    /// driver's own change still settling, not a self-updating window; the
    /// digest is re-sampled a bounded number of times before the window is
    /// written off as unquiescent. Measured on the AppKit harness: the
    /// label a menu command sets was missed 2/3 when the capture followed
    /// the activation immediately, and seen 3/3 once it was left to settle.
    pub fn capture_after_activation(pid: i32, window_id: u32) -> Self {
        let start = Instant::now();
        let mut probe = Self::capture(pid, window_id, None);
        for _ in 0..ACTIVATION_SETTLE_ATTEMPTS {
            if probe.quiescent
                || probe.tree_skipped
                || cua_driver_core::operation::sleep(ACTIVATION_SETTLE_GAP).is_err()
            {
                break;
            }
            probe = Self::capture(pid, window_id, None);
        }
        probe.elapsed = start.elapsed();
        probe
    }

    /// Sample the target until something differs or the settle budget runs
    /// out. Blocking AX work — call from a blocking thread.
    ///
    /// The first sample that differs ends the wait, so a responsive
    /// application pays only its own latency; only a target that never
    /// reacts pays the whole budget.
    pub fn compare(self) -> ProbeOutcome {
        let budget = settle_budget(&self.before, self.remote_screen);
        self.compare_within(budget)
    }

    fn compare_within(&self, budget: Duration) -> ProbeOutcome {
        let start = Instant::now();
        let deadline = start + budget;
        loop {
            let after = self.sample();
            let evidence = classify(&self.before, &after, self.quiescent);
            let expired = Instant::now() >= deadline;
            if evidence.is_reaction() || expired {
                return self.outcome(evidence, start.elapsed(), &after);
            }
            // A cancelled caller stops waiting on a target it no longer wants
            // and keeps the verdict observed so far.
            if cua_driver_core::operation::sleep(SETTLE_POLL).is_err() {
                return self.outcome(evidence, start.elapsed(), &after);
            }
        }
    }

    fn sample(&self) -> Signals {
        Signals {
            element: self
                .element
                .as_ref()
                .map_or(ElementRead::Unreadable, element_state),
            focus: focus_state(self.pid),
            tree: if self.quiescent {
                tree_digest(self.pid, self.window_id)
            } else {
                None
            },
            menus: menu_state(self.pid),
        }
    }

    fn outcome(&self, evidence: Evidence, waited: Duration, after: &Signals) -> ProbeOutcome {
        ProbeOutcome {
            evidence,
            probe: self.elapsed + waited,
            waited,
            watched: Watched::over(&self.before, after, self.quiescent),
        }
    }

    /// Verdict for a reaction that was already proven while the action ran —
    /// a sheet or dialog the window observer caught — so nothing is waited for.
    pub fn settled(&self, evidence: Evidence) -> ProbeOutcome {
        ProbeOutcome {
            evidence,
            probe: self.elapsed,
            waited: Duration::ZERO,
            watched: Watched::offered(&self.before, self.quiescent),
        }
    }
}

/// Whether a window answered an action with a change of its own, read by the
/// probe's subtree digest alone and captured after the caller's own write, so
/// that write is part of the baseline.
///
/// For an action aimed at a control that holds still while it runs — a search
/// field's `AXConfirm` over a value already written — the control's own state
/// and the application's focus can move without the application doing what
/// was asked, so neither is watched: only the rest of the window reads what
/// the application did with the action.
pub struct WindowWatch {
    pid: i32,
    window_id: u32,
    /// The digest two back-to-back samples agreed on, or `None` when the
    /// window never held still (or never answered) inside the capture.
    settled: Option<u64>,
}

/// What a [`WindowWatch`] saw after the action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowAnswer {
    /// The window's subtree differed from the settled digest.
    Changed,
    /// Every readable sample matched the settled digest for `waited`.
    Unchanged { waited: Duration },
    /// Nothing could be compared: the window never settled before the action,
    /// or never answered after it.
    Unwatched,
}

impl WindowWatch {
    /// Settle the window's digest before the action. Blocking AX work — call
    /// from a blocking thread. A window still redrawing the caller's own write
    /// gets the same bounded re-sampling a window the driver just activated
    /// does before it is written off as unquiescent.
    pub fn capture(pid: i32, window_id: u32) -> Self {
        Self {
            pid,
            window_id,
            settled: settle_digest(|| tree_digest(pid, window_id), ACTIVATION_SETTLE_ATTEMPTS, || {
                cua_driver_core::operation::sleep(ACTIVATION_SETTLE_GAP).is_ok()
            }),
        }
    }

    /// Sample the window until it differs or the settle budget runs out.
    /// Blocking AX work — call from a blocking thread.
    pub fn answer(&self) -> WindowAnswer {
        let start = Instant::now();
        answer_within(
            self.settled,
            || tree_digest(self.pid, self.window_id),
            || start.elapsed(),
            SETTLE_BUDGET,
            || cua_driver_core::operation::sleep(SETTLE_POLL).is_ok(),
        )
    }
}

/// The digest two back-to-back samples agree on, re-sampling at most
/// `attempts` more times while they disagree; `wait` returning false (a
/// cancelled caller) ends the capture unsettled.
fn settle_digest(
    mut sample: impl FnMut() -> Option<u64>,
    attempts: usize,
    mut wait: impl FnMut() -> bool,
) -> Option<u64> {
    let mut previous = sample();
    for attempt in 0..=attempts {
        let next = sample();
        if next.is_some() && next == previous {
            return next;
        }
        if attempt == attempts || !wait() {
            break;
        }
        previous = next;
    }
    None
}

/// Compare post-action samples against `settled` until one differs or
/// `budget` has elapsed. An unreadable sample compares nothing, so a watch
/// whose every sample was unreadable saw nothing rather than no change.
fn answer_within(
    settled: Option<u64>,
    mut sample: impl FnMut() -> Option<u64>,
    mut elapsed: impl FnMut() -> Duration,
    budget: Duration,
    mut wait: impl FnMut() -> bool,
) -> WindowAnswer {
    let Some(settled) = settled else {
        return WindowAnswer::Unwatched;
    };
    let mut compared = false;
    loop {
        if let Some(after) = sample() {
            if after != settled {
                return WindowAnswer::Changed;
            }
            compared = true;
        }
        let waited = elapsed();
        if waited >= budget || !wait() {
            return if compared {
                WindowAnswer::Unchanged { waited }
            } else {
                WindowAnswer::Unwatched
            };
        }
    }
}

fn settle_budget(before: &Signals, remote_screen: bool) -> Duration {
    if before.element.state().is_some() && !remote_screen {
        SETTLE_BUDGET
    } else {
        BLIND_SETTLE_BUDGET
    }
}

/// Pure verdict over two samples. A signal counts only when both samples read
/// it; an unreadable half is unknown, never a change. The one exception is an
/// element that was readable before and reports itself destroyed after: that
/// is the reaction, not a missing read.
///
/// The accessory-surface set is checked last and one-way: it can only add a
/// reaction, never turn [`Evidence::Unusable`] into [`Evidence::Unchanged`].
/// "No menu appeared" is not evidence that nothing happened, so it must not
/// buy the probe the right to say `suspected_noop`.
pub fn classify(before: &Signals, after: &Signals, quiescent: bool) -> Evidence {
    let mut usable = false;
    match (&before.element, &after.element) {
        (ElementRead::State(b), ElementRead::State(a)) => {
            usable = true;
            if b != a {
                return Evidence::Changed(ELEMENT_SIGNAL);
            }
        }
        (ElementRead::State(_), ElementRead::Gone) => return Evidence::ElementGone,
        _ => {}
    }
    if let (Some(b), Some(a)) = (before.focus.as_ref(), after.focus.as_ref()) {
        usable = true;
        if b != a {
            return Evidence::Changed(FOCUS_SIGNAL);
        }
    }
    if quiescent {
        if let (Some(b), Some(a)) = (before.tree, after.tree) {
            usable = true;
            if b != a {
                return Evidence::Changed(TREE_SIGNAL);
            }
        }
    }
    if !gained_menus(&before.menus, &after.menus).is_empty() {
        return Evidence::Changed(MENU_SIGNAL);
    }
    if usable {
        Evidence::Unchanged
    } else {
        Evidence::Unusable
    }
}

/// Identity + mutable state of one element.
///
/// The child count is part of that state because it is the only signal a
/// menu-bearing control moves: a toolbar `AXMenuButton` whose menu opened
/// keeps its role, title, value, focus, selection, enablement and frame, and
/// gains one `AXMenu` child. Contacts' add button is exactly that control,
/// and without this the probe called an opened menu a no-op.
///
/// The role read reports its AX error so a destroyed element
/// (`kAXErrorInvalidUIElement`) is told apart from one that is merely busy or
/// slow — the first is a reaction, the second is unknown.
fn element_state(element: &RetainedElement) -> ElementRead {
    let element = element.as_ptr() as AXUIElementRef;
    // SAFETY: `RetainedElement` holds a reference to this element, so it is a
    // valid AX element for every read below even if the application already
    // destroyed the control behind it.
    unsafe {
        let role = match try_copy_string_attr(element, "AXRole") {
            Ok(Some(role)) => role,
            Err(error) if error == kAXErrorInvalidUIElement => return ElementRead::Gone,
            Ok(None) | Err(_) => return ElementRead::Unreadable,
        };
        let title = copy_string_attr(element, "AXTitle").unwrap_or_default();
        let value = copy_string_attr(element, "AXValue").unwrap_or_default();
        let focused = copy_bool_attr(element, "AXFocused");
        let selected = copy_bool_attr(element, "AXSelected");
        let enabled = copy_bool_attr(element, "AXEnabled");
        let rect = element_screen_rect(element);
        let children = children_count(element);
        ElementRead::State(format!(
            "{role}|{title}|{value}|{focused:?}|{selected:?}|{enabled:?}|{rect:?}|{children:?}"
        ))
    }
}

/// Identity of the app's focused element. A real click focuses the control it
/// lands on, so this moves on delivery even when the control's own state does
/// not.
///
/// This resolves the focused element afresh on every sample and owns the
/// reference `AXFocusedUIElement` handed over, so no pointer to it survives
/// the dispatch: a chord that destroys the focused element changes this
/// signal, it does not leave a dangling read behind.
fn focus_state(pid: i32) -> Option<String> {
    // SAFETY: `focused_element_of_pid` returns a `+1` reference, which the
    // guard takes over and releases when this sample ends.
    let focused = unsafe { focused_element_of_pid(pid).and_then(|f| RetainedElement::adopt(f)) }?;
    element_state(&focused).state().map(str::to_owned)
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

/// Whether `window_id`'s accessibility tree carries no content: the window
/// has no children, or its only child is a group with no children. `false`
/// when the window or its children cannot be read.
fn window_has_no_ax_content(pid: i32, window_id: u32) -> bool {
    use crate::ax::bindings::{
        ax_get_window_id, copy_ax_windows, copy_children_checked, AXUIElementCreateApplication,
        AXUIElementSetMessagingTimeout,
    };
    use core_foundation::base::{CFRelease, CFTypeRef};

    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        AXUIElementSetMessagingTimeout(app, 0.25);
        let mut window = None;
        for candidate in copy_ax_windows(app) {
            if window.is_none() && ax_get_window_id(candidate) == Some(window_id) {
                window = Some(candidate);
            } else {
                CFRelease(candidate as CFTypeRef);
            }
        }
        CFRelease(app as CFTypeRef);
        let Some(window) = window else {
            return false;
        };
        let (children, hidden) = copy_children_checked(window);
        let shapes: Vec<(Option<String>, Option<usize>)> = children
            .iter()
            .map(|&child| (copy_string_attr(child, "AXRole"), children_count(child)))
            .collect();
        for child in children {
            CFRelease(child as CFTypeRef);
        }
        CFRelease(window as CFTypeRef);
        !hidden && tree_is_contentless(&shapes)
    }
}

/// The content-less shapes, over each child's `(role, child count)`.
fn tree_is_contentless(children: &[(Option<String>, Option<usize>)]) -> bool {
    match children {
        [] => true,
        [(Some(role), Some(0))] => role == "AXGroup",
        _ => false,
    }
}

/// The accessory windows `pid` currently shows. An open `NSMenu` is one of
/// them; so is a popover's own backing window once it is on screen above the
/// application's normal layer. Read from WindowServer rather than AX because
/// neither surface is reliably reachable from the target window's subtree —
/// which is exactly why the other three signals miss it.
fn menu_state(pid: i32) -> Vec<u32> {
    crate::windows::accessory_window_ids(pid)
}

/// Accessory windows present after the dispatch that were not present before.
fn gained_menus(before: &[u32], after: &[u32]) -> Vec<u32> {
    after
        .iter()
        .copied()
        .filter(|window_id| !before.contains(window_id))
        .collect()
}

pub struct NoopReport<'a> {
    /// Whether the caller watched for windows opening during the action. The
    /// probe cannot see that signal itself, so the reply names it only when
    /// the caller polled for it.
    pub polled: bool,
    pub escalation: Option<serde_json::Value>,
    pub advice: &'a str,
}

/// Fold the delivery probe's verdict into an action's reply, staying inside
/// the closed public `ActionResult` vocabulary
/// (`confirmed | partial | unverifiable | suspected_noop | refused`).
///
/// The probe answers "did the target react", never "did the action do what
/// the caller wanted". So:
/// * `Changed` → keep `unverifiable` (delivery is not the intended
///   postcondition) and publish the reaction as evidence named after the
///   signal that moved.
/// * `Unchanged` → `suspected_noop`, said loudly, escalated to the rung that
///   can still deliver. Never retried here and never silently re-routed: a
///   dispatched action can take effect invisibly, and acting twice is worse
///   than reporting an unproven one.
/// * `Unusable` → leave the existing contract alone; the probe had nothing
///   comparable to offer, and says so in the text.
pub fn apply_evidence(
    msg: &mut String,
    structured: &mut serde_json::Value,
    outcome: ProbeOutcome,
    noop: NoopReport<'_>,
) {
    let probe_ms = outcome.probe.as_millis();
    let waited_ms = outcome.waited.as_millis();
    let compared = outcome.watched.compared(noop.polled);
    let unread = outcome.watched.unread();
    structured["delivery_probe"] = serde_json::json!({
        "signal": outcome.evidence.signal(),
        "probe_ms": probe_ms,
        "waited_ms": waited_ms,
        "watched": compared,
        "unread": unread,
    });
    match outcome.evidence {
        Evidence::Changed(_) | Evidence::ElementGone => {
            let signal = outcome.evidence.signal();
            structured["evidence"] = serde_json::json!([{ "kind": signal }]);
            msg.push_str(&format!(
                "\n🔎 Delivered: {} after the dispatch, so the app reacted. \
                 That is delivery, not the intended result — check the postcondition you \
                 wanted.",
                reaction_phrase(outcome.evidence)
            ));
        }
        Evidence::Unchanged => {
            structured["effect"] = serde_json::json!("suspected_noop");
            if let Some(escalation) = noop.escalation {
                structured["escalation"] = escalation;
            }
            // The scope of the claim, in the contract's own signal spellings:
            // what was compared, and what a change would have gone unseen in.
            // A per-tool list of what that tool usually watches named signals
            // this call never read.
            let unread = if unread.is_empty() {
                String::new()
            } else {
                format!(
                    "; {} could not be read, so a change there would not have been seen",
                    unread.join(", ")
                )
            };
            msg.push_str(&format!(
                "\n⚠️ Unverified: the target was watched for {waited_ms} ms after the \
                 dispatch and {} read the same{unread}. Re-observe before repeating — the \
                 dispatch may still have landed, so a second call could act twice.{}",
                compared.join(", "),
                noop.advice
            ));
        }
        Evidence::Unusable => {
            msg.push_str(
                "\n❔ Delivery unverified: the target exposed no stable state to compare \
                 (neither the element nor the app's focus answered, and the window is \
                 unreadable or changes on its own). Confirm the postcondition yourself.",
            );
        }
    }
}

/// How a reaction reads in the reply. Every signal but two is a state that
/// differs; a destroyed element is an absence, and "element_state changed"
/// would leave the agent looking for a control that no longer exists, while
/// "menu_opened changed" would say nothing about what is now on screen.
fn reaction_phrase(evidence: Evidence) -> String {
    match evidence {
        Evidence::ElementGone => "the element the probe watched is gone".to_owned(),
        Evidence::Changed(MENU_SIGNAL) => {
            "the application opened a menu or popover".to_owned()
        }
        other => format!("{} changed", other.signal()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::bindings::AXUIElementCreateApplication;
    use core_foundation::base::{CFRelease, CFTypeRef};
    use core_foundation::base::CFGetRetainCount;

    fn state(digest: &str) -> ElementRead {
        ElementRead::State(digest.to_owned())
    }

    fn signals(element: ElementRead, focus: Option<&str>, tree: Option<u64>) -> Signals {
        Signals {
            element,
            focus: focus.map(str::to_owned),
            tree,
            menus: Vec::new(),
        }
    }

    fn with_menus(mut signals: Signals, menus: &[u32]) -> Signals {
        signals.menus = menus.to_vec();
        signals
    }

    fn probe_over_a_pid_that_answers_nothing() -> DeliveryProbe {
        DeliveryProbe {
            pid: 0,
            window_id: 0,
            element: None,
            before: Signals::default(),
            quiescent: false,
            tree_skipped: false,
            remote_screen: false,
            elapsed: Duration::ZERO,
        }
    }

    /// A mirrored phone screen publishes one empty group and nothing else;
    /// its digest changes with nothing the caller did, so "Delivered:
    /// window_tree changed" there was noise. Any real control keeps it.
    #[test]
    fn only_an_empty_window_or_one_empty_group_is_contentless() {
        let group = |children| (Some("AXGroup".to_owned()), children);
        assert!(tree_is_contentless(&[]));
        assert!(tree_is_contentless(&[group(Some(0))]));
        assert!(!tree_is_contentless(&[group(Some(3))]));
        assert!(!tree_is_contentless(&[group(None)]), "unreadable children prove nothing");
        assert!(!tree_is_contentless(&[group(Some(0)), group(Some(0))]));
        assert!(!tree_is_contentless(&[(Some("AXButton".to_owned()), Some(0))]));
    }

    #[test]
    fn a_readable_element_state_is_what_buys_the_full_settle_budget() {
        assert_eq!(
            settle_budget(&signals(state("AXButton|New Item||"), None, None), false),
            SETTLE_BUDGET
        );
        assert_eq!(
            settle_budget(&signals(state("AXButton|New Item||"), Some("f"), Some(3)), false),
            SETTLE_BUDGET
        );
        assert_eq!(
            settle_budget(&signals(ElementRead::Unreadable, Some("f"), Some(3)), false),
            BLIND_SETTLE_BUDGET
        );
        assert_eq!(
            settle_budget(&signals(ElementRead::Gone, Some("f"), Some(3)), false),
            BLIND_SETTLE_BUDGET
        );
        assert_eq!(settle_budget(&Signals::default(), false), BLIND_SETTLE_BUDGET);
    }

    #[test]
    fn a_key_forwarding_window_never_buys_the_full_settle_budget() {
        assert_eq!(
            settle_budget(&signals(state("AXButton|Home Screen||"), Some("f"), Some(3)), true),
            BLIND_SETTLE_BUDGET
        );
    }

    #[test]
    fn a_capture_without_element_state_stops_at_the_blind_budget() {
        let outcome = probe_over_a_pid_that_answers_nothing().compare();
        assert_eq!(outcome.evidence, Evidence::Unusable);
        assert!(
            outcome.waited >= BLIND_SETTLE_BUDGET,
            "the cap is a full sampling window, not an early exit: {:?}",
            outcome.waited
        );
        assert!(
            outcome.waited < SETTLE_BUDGET,
            "a probe with no element to compare must not wait the full budget: {:?}",
            outcome.waited
        );
    }

    #[test]
    fn element_state_change_is_delivery_evidence() {
        let before = signals(state("AXCheckBox||0|Some(false)"), Some("f"), Some(1));
        let after = signals(state("AXCheckBox||1|Some(false)"), Some("f"), Some(1));
        assert_eq!(
            classify(&before, &after, true),
            Evidence::Changed("element_state")
        );
    }

    #[test]
    fn a_destroyed_element_is_the_reaction_not_a_missing_read() {
        // Escape on a popover: the element it dismissed answers
        // kAXErrorInvalidUIElement afterwards. Nothing else has to have moved
        // for the dismissal to have landed.
        let before = signals(state("AXButton|Search||"), Some("f"), Some(9));
        let after = signals(ElementRead::Gone, Some("f"), Some(9));
        let evidence = classify(&before, &after, true);
        assert_eq!(evidence, Evidence::ElementGone);
        assert!(evidence.is_reaction(), "a dismissal did land");
        assert_eq!(
            reaction_phrase(evidence),
            "the element the probe watched is gone",
            "prose must not send the agent looking for a destroyed control"
        );
    }

    #[test]
    fn a_destroyed_element_publishes_a_signal_the_action_contract_keeps() {
        // `ActionExecutionRecord::from_legacy` drops an evidence row whose
        // signal spelling the contract does not publish, so a reaction named
        // outside this vocabulary would vanish from the record.
        for evidence in [Evidence::ElementGone, Evidence::Changed(ELEMENT_SIGNAL)] {
            assert_eq!(
                cua_driver_contract::ActionEvidenceSignal::from_wire(evidence.signal()),
                Some(cua_driver_contract::ActionEvidenceSignal::ElementState)
            );
        }
    }

    #[test]
    fn a_destroyed_element_reports_a_delivered_press_not_a_noop() {
        let mut msg = "✅ Pressed escape on pid 4242.".to_owned();
        let mut structured = serde_json::json!({ "path": "cgevent", "effect": "unverifiable" });
        apply_evidence(
            &mut msg,
            &mut structured,
            ProbeOutcome {
                evidence: Evidence::ElementGone,
                probe: Duration::from_millis(80),
                waited: Duration::from_millis(20),
                watched: Watched {
                    element: true,
                    focus: true,
                    tree: true,
                    accessory: true,
                },
            },
            NoopReport {
                polled: false,
                escalation: Some(serde_json::json!({ "target": "foreground" })),
                advice: "",
            },
        );
        assert_eq!(structured["evidence"][0]["kind"], "element_state");
        assert_eq!(structured["delivery_probe"]["signal"], "element_state");
        assert_eq!(
            structured["effect"], "unverifiable",
            "a reaction is delivery, never the intended postcondition"
        );
        assert!(structured["escalation"].is_null(), "nothing to escalate");
        assert!(
            msg.contains("the element the probe watched is gone"),
            "{msg}"
        );
    }

    fn unchanged(watched: Watched, waited_ms: u64) -> ProbeOutcome {
        ProbeOutcome {
            evidence: Evidence::Unchanged,
            probe: Duration::from_millis(waited_ms + 70),
            waited: Duration::from_millis(waited_ms),
            watched,
        }
    }

    fn noop(polled: bool) -> NoopReport<'static> {
        NoopReport {
            polled,
            escalation: None,
            advice: "",
        }
    }

    /// The list is the probe's own record, not the calling tool's habit. The
    /// per-tool literal it replaces named signals this call never read and
    /// omitted the accessory surfaces it always compares.
    #[test]
    fn the_no_change_sentence_names_only_the_signals_that_were_compared() {
        let mut msg = String::new();
        let mut structured = serde_json::json!({ "path": "ax", "effect": "unverifiable" });
        apply_evidence(
            &mut msg,
            &mut structured,
            unchanged(
                Watched {
                    element: false,
                    focus: true,
                    tree: false,
                    accessory: true,
                },
                2000,
            ),
            noop(false),
        );
        assert!(
            msg.contains("app_focus, menu_opened read the same"),
            "the compared set is what the sentence may claim: {msg}"
        );
        assert!(
            msg.contains(
                "element_state, window_tree could not be read, so a change there would not \
                 have been seen"
            ),
            "an unread signal refutes nothing and has to say so: {msg}"
        );
        assert_eq!(
            structured["delivery_probe"]["watched"],
            serde_json::json!(["app_focus", "menu_opened"])
        );
        assert_eq!(
            structured["delivery_probe"]["unread"],
            serde_json::json!(["element_state", "window_tree"])
        );

        // A caller that declined the window poll never watched for windows
        // opening, and a caller that asked for it did. The accessory surfaces
        // are read from WindowServer, so they are compared either way.
        let mut polled_msg = String::new();
        apply_evidence(
            &mut polled_msg,
            &mut structured,
            unchanged(
                Watched {
                    element: false,
                    focus: false,
                    tree: false,
                    accessory: true,
                },
                2000,
            ),
            noop(true),
        );
        assert!(
            polled_msg.contains("menu_opened, window_change read the same"),
            "{polled_msg}"
        );
        assert!(!msg.contains(WINDOW_SIGNAL), "a declined poll is not a signal: {msg}");
    }

    /// The login-item regression: `press("cmd+shift+g")` passed no element,
    /// waited the blind budget and the reply still said a "focused element"
    /// was watched. The probe's own record cannot say that.
    #[test]
    fn a_blind_chord_never_claims_an_element_was_watched() {
        let outcome = probe_over_a_pid_that_answers_nothing().compare();
        assert!(
            !outcome.watched.element,
            "no element pointer was ever passed: {:?}",
            outcome.watched
        );
        let mut msg = String::new();
        let mut structured = serde_json::json!({ "path": "key_events" });
        apply_evidence(&mut msg, &mut structured, outcome, noop(false));
        assert!(!msg.contains(ELEMENT_SIGNAL), "{msg}");

        // The same chord against an application that answers: its focused
        // element, the window digest and the accessory surfaces all compare,
        // the element the caller never named does not.
        let mut msg = String::new();
        apply_evidence(
            &mut msg,
            &mut structured,
            unchanged(
                Watched {
                    element: false,
                    focus: true,
                    tree: true,
                    accessory: true,
                },
                633,
            ),
            noop(false),
        );
        assert_eq!(
            msg,
            "\n⚠️ Unverified: the target was watched for 633 ms after the dispatch and \
             app_focus, window_tree, menu_opened read the same; element_state could not be \
             read, so a change there would not have been seen. Re-observe before repeating \
             — the dispatch may still have landed, so a second call could act twice."
        );
    }

    /// The sentence is the wire for this fact: OMP recovers the scope of the
    /// doubt by matching `watched for (\d+) ms after the dispatch` over the
    /// driver's prose (`render.ts`), so the phrase and the number's place in
    /// it are a contract, not wording.
    #[test]
    fn the_sentence_keeps_the_phrase_the_session_matches_on() {
        for watched in [
            Watched {
                element: true,
                focus: true,
                tree: true,
                accessory: true,
            },
            Watched {
                element: false,
                focus: true,
                tree: false,
                accessory: true,
            },
        ] {
            let mut msg = String::new();
            let mut structured = serde_json::json!({ "path": "ax" });
            apply_evidence(
                &mut msg,
                &mut structured,
                unchanged(watched, 1450),
                noop(false),
            );
            let waited = msg
                .split_once("watched for ")
                .and_then(|(_, rest)| rest.split_once(" ms after the dispatch"))
                .map(|(waited, _)| waited.to_owned());
            assert_eq!(
                waited.as_deref(),
                Some("1450"),
                "the phrase render.ts reads the wait out of: {msg}"
            );
        }
    }

    #[test]
    fn an_element_that_merely_stopped_answering_is_not_a_reaction() {
        let before = signals(state("AXButton|Search||"), Some("f"), Some(9));
        let after = signals(ElementRead::Unreadable, Some("f"), Some(9));
        assert_eq!(classify(&before, &after, true), Evidence::Unchanged);
    }

    #[test]
    fn an_element_unreadable_before_the_dispatch_claims_nothing_after() {
        let before = signals(ElementRead::Unreadable, None, None);
        let after = signals(ElementRead::Gone, None, None);
        assert_eq!(classify(&before, &after, true), Evidence::Unusable);
    }

    #[test]
    fn focus_move_is_delivery_evidence_when_element_state_holds() {
        let before = signals(state("AXButton|New Item||"), Some("AXWebArea|doc"), Some(7));
        let after = signals(
            state("AXButton|New Item||"),
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
        let before = signals(state("AXButton|b||"), Some("f"), Some(11));
        let after = signals(state("AXButton|b||"), Some("f"), Some(12));
        assert_eq!(
            classify(&before, &after, true),
            Evidence::Changed("window_tree")
        );
    }

    #[test]
    fn self_updating_window_never_counts_as_delivery() {
        let before = signals(state("AXButton|b||"), Some("f"), Some(11));
        let after = signals(state("AXButton|b||"), Some("f"), Some(12));
        assert_eq!(classify(&before, &after, false), Evidence::Unchanged);
    }

    #[test]
    fn every_usable_signal_identical_is_unchanged() {
        let before = signals(state("AXButton|b||"), Some("f"), Some(11));
        let after = signals(state("AXButton|b||"), Some("f"), Some(11));
        assert_eq!(classify(&before, &after, true), Evidence::Unchanged);
    }

    #[test]
    fn one_sided_reads_are_unknown_not_change() {
        let before = signals(state("AXButton|b||"), None, None);
        let after = signals(ElementRead::Unreadable, Some("f"), Some(4));
        assert_eq!(classify(&before, &after, true), Evidence::Unusable);
    }

    #[test]
    fn detached_element_with_readable_focus_still_compares_focus() {
        let before = signals(ElementRead::Unreadable, Some("AXWebArea|doc"), None);
        let after = signals(ElementRead::Unreadable, Some("AXWebArea|doc"), None);
        assert_eq!(classify(&before, &after, true), Evidence::Unchanged);
    }

    /// The shape this signal exists for: a popup button whose menu opened
    /// keeps its role, title, value, focus, selection, enablement and frame,
    /// the app's focused element does not move, and the menu is drawn in its
    /// own accessory window outside the target window's subtree. Every signal
    /// contract 0.10 published reads identical, and the press was reported as
    /// a no-op while the menu stood open on screen.
    #[test]
    fn an_opened_menu_is_delivery_even_when_every_other_signal_holds() {
        let steady = signals(state("AXPopUpButton|Alarm|None|"), Some("f"), Some(11));
        let before = with_menus(steady.clone(), &[900]);
        let after = with_menus(steady, &[900, 4211]);
        let evidence = classify(&before, &after, true);
        assert_eq!(evidence, Evidence::Changed(MENU_SIGNAL));
        assert!(evidence.is_reaction());
        assert_eq!(
            reaction_phrase(evidence),
            "the application opened a menu or popover"
        );
    }

    #[test]
    fn an_opened_menu_publishes_a_signal_the_action_contract_keeps() {
        assert_eq!(
            cua_driver_contract::ActionEvidenceSignal::from_wire(MENU_SIGNAL),
            Some(cua_driver_contract::ActionEvidenceSignal::MenuOpened)
        );
    }

    /// One direction only. A menu that was already open and then closed is
    /// routinely the driver's own doing — a foreground restore dismisses it —
    /// so a lost accessory window must not be sold as the app reacting.
    #[test]
    fn a_dismissed_menu_is_not_a_reaction_on_its_own() {
        let steady = signals(state("AXPopUpButton|Alarm|None|"), Some("f"), Some(11));
        let before = with_menus(steady.clone(), &[900, 4211]);
        let after = with_menus(steady, &[900]);
        assert_eq!(classify(&before, &after, true), Evidence::Unchanged);
    }

    /// Absence of a menu is not evidence that nothing happened. A probe with
    /// nothing comparable must stay `Unusable` — which reports "delivery
    /// unverified", not `suspected_noop` — even though the accessory-window
    /// set was always readable.
    #[test]
    fn a_readable_but_empty_menu_set_never_buys_a_noop_verdict() {
        let before = signals(ElementRead::Unreadable, None, None);
        let after = signals(ElementRead::Unreadable, None, None);
        assert_eq!(classify(&before, &after, true), Evidence::Unusable);
    }

    #[test]
    fn capture_owns_the_element_it_watches() {
        // The crash this guards: `capture` used to keep a bare pointer, so the
        // element only lived as long as whatever the caller happened to hold.
        // `press_key` released its focused-element reference before
        // `compare()` ran, and reading a freed AXUIElementRef traps inside
        // CoreFoundation instead of returning an error.
        //
        // A never-running pid answers every AX read immediately, so this
        // exercises the ownership without needing a live window.
        let element = unsafe { AXUIElementCreateApplication(-1) };
        assert!(!element.is_null(), "AXUIElementCreateApplication");
        let count = || unsafe { CFGetRetainCount(element as CFTypeRef) };
        let base = count();

        let probe = DeliveryProbe::capture(-1, 0, Some(element as usize));
        assert_eq!(
            count(),
            base + 1,
            "the probe must hold the watched element itself"
        );

        drop(probe);
        assert_eq!(count(), base, "and give the reference back when dropped");
        unsafe { CFRelease(element as CFTypeRef) };
    }

    #[test]
    fn an_invalid_element_reads_as_gone_and_any_other_error_does_not() {
        // What a retained-but-destroyed element answers: AX reports
        // kAXErrorInvalidUIElement instead of trapping, and only that error
        // means the element is gone. An app that cannot be reached at all
        // (kAXErrorCannotComplete) says nothing about its elements.
        let invalid = unsafe { AXUIElementCreateApplication(-1) }; // no such process
        let guard = unsafe { RetainedElement::adopt(invalid) }.expect("element");
        assert_eq!(element_state(&guard), ElementRead::Gone);

        let unreachable = unsafe { AXUIElementCreateApplication(999_999) }; // above pid_max
        let guard = unsafe { RetainedElement::adopt(unreachable) }.expect("element");
        assert_eq!(element_state(&guard), ElementRead::Unreadable);
    }

    /// Samples handed out in order; once exhausted, the last one repeats.
    fn samples(values: &[Option<u64>]) -> impl FnMut() -> Option<u64> + '_ {
        let mut next = 0;
        move || {
            let value = values[next.min(values.len() - 1)];
            next += 1;
            value
        }
    }

    /// A clock that advances one poll per read.
    fn ticking(step_ms: u64) -> impl FnMut() -> Duration {
        let mut now = 0;
        move || {
            now += step_ms;
            Duration::from_millis(now)
        }
    }

    #[test]
    fn a_window_still_redrawing_the_write_settles_before_the_watch_starts() {
        // The first pair disagrees (the write still drawing), the second agrees.
        assert_eq!(
            settle_digest(samples(&[Some(1), Some(2), Some(2)]), 3, || true),
            Some(2)
        );
        // A window that never holds still is not watched.
        assert_eq!(
            settle_digest(samples(&[Some(1), Some(2), Some(3), Some(4), Some(5)]), 3, || true),
            None
        );
        // Two unreadable samples agree on nothing.
        assert_eq!(settle_digest(samples(&[None]), 3, || true), None);
    }

    #[test]
    fn a_window_that_changed_after_the_action_answered_it() {
        let answer = answer_within(
            Some(7),
            samples(&[Some(7), Some(8)]),
            ticking(50),
            SETTLE_BUDGET,
            || true,
        );
        assert_eq!(answer, WindowAnswer::Changed);
    }

    #[test]
    fn a_window_that_held_still_for_the_budget_did_not_answer() {
        let answer = answer_within(Some(7), samples(&[Some(7)]), ticking(500), SETTLE_BUDGET, || true);
        assert_eq!(
            answer,
            WindowAnswer::Unchanged {
                waited: SETTLE_BUDGET
            }
        );
    }

    /// Neither an unsettled capture nor a window that never answered after
    /// the action is a window that held still.
    #[test]
    fn a_watch_that_compared_nothing_saw_nothing() {
        assert_eq!(
            answer_within(None, samples(&[Some(1)]), ticking(500), SETTLE_BUDGET, || true),
            WindowAnswer::Unwatched
        );
        assert_eq!(
            answer_within(Some(7), samples(&[None]), ticking(500), SETTLE_BUDGET, || true),
            WindowAnswer::Unwatched
        );
        // An unreadable sample never reads as the change either.
        assert_eq!(
            answer_within(Some(7), samples(&[None, Some(7)]), ticking(500), SETTLE_BUDGET, || true),
            WindowAnswer::Unchanged {
                waited: SETTLE_BUDGET
            }
        );
    }
}
