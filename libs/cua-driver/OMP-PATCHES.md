# Fork patches carried on `will/omp`

This branch is upstream `trycua/cua` at `e7e141ae` (identical to the
`cua-driver-rs-v0.28.0` contract, version, and release workflow) plus the
linear patch stack below, one commit per row, in stack order. The previous
fork head (13 commits on 0.24.0) remains reachable as tag `will/omp-pre-0.28`.

Rules for this stack:

- Generated files are never merged by hand: `contract/manifest.json` comes
  from `cargo run -p cua-driver-contract --bin cua-contract-gen -- all`, and
  `rust/include/cua_driver_abi.h` from
  `cargo run -p cua-driver-bindgen --bin cua-driver-abi-header`.
- The `will/omp` rows below add nothing to the public contract: every field
  they add to a tool result rides through upstream's 0.28.0 result boundary
  (`conforming_tool_result`) unchanged — the window snapshot schema is open,
  and typed refusals (`cancelled`, `capture_timeout`, `foreground_unavailable`)
  already carry a `code`, which the boundary keeps. That stopped being true
  of the bench stack at wave 5: `CONTRACT_VERSION` moved to 0.9.0 (6ee6d7632)
  and then 0.10.0 (cf52d17ea), and the typed inputs gained
  `detect_window_change`. The rule for those rows is instead that every
  change is additive over 0.8 (b76016c77: the kind name `window_change` is
  kept; `signal`, `committed`, `menu_path`, the `element` / `snapshot`
  targets and the `menu_command` route are new optional vocabulary; and each
  typed input has a `new` constructor so a struct literal is never required).
- Nothing here is OMP-specific behaviour; `cancel_operation`, `_call_id`, and
  `notifications/cancelled` are generic MCP.

| # | Patch (commit title) | Why upstream lacks it | Related upstream issue / PR | Upstream PR? |
|---|---|---|---|---|
| P1 | `feat(cua-driver-contract): report display identity and screen origin` | `get_screen_size` / `get_desktop_state` publish only width/height/scale, so two equal-sized displays are indistinguishable and desktop consent grants cannot be bound to a source. Fields are optional; Linux already computes the identity internally (`platform-linux/wayland/hyprland.rs`) but does not publish it. | none found | issue #3797 filed; PR after the display/primary relationship answered there (09-14) |
| P2 | `feat(cua-driver-core): cooperative operation cancellation and owned native cleanup` | Upstream has no operation-scoped cancellation: a caller that disappears cannot stop a long native input sequence, and runtime shutdown cannot drain native resources it does not own. Maintainers deferred a "general cancelled-blocking-worker drain redesign" (#3473). | #3473 (deferral), #2002 (idle CPU / stuck capture after session) | RFC #3796 (with P3/P8/P9); maintainers asked for an RFC before implementation, first slice scoped to core ownership + transports |
| P3 | `feat(cua-driver): honor notifications/cancelled and cancel by call id over the C ABI` | MCP `notifications/cancelled` is spec-level and ignored upstream; there is no way to stop one in-flight call. Adds transport-scoped call ids, the `cancel_operation` fallback tool, `cua_driver_invoke_call_v1` / `cua_driver_cancel_call_v1` (additive C ABI), and concurrent dispatch on the direct stdio path so a cancel can be read mid-call. Shutdown only interrupts pacing; a call that still completes under it keeps its own result (upstream's drain contract). | none found (spec gap) | RFC #3796 (with P2) |
| P4 | `feat(cua-driver-core): attach document path and dirty bit to window snapshots` | `get_window_state` has no notion of the document a window edits or whether it has unsaved changes; absence must stay absent rather than default to "clean". | none found | #3795 (with P10) |
| P5 | `fix(cua-driver-core): admit semantic AX on app menu bars and attached sheets; name the owning window in refusals` | An app's own `AXMenuBar` and an `AXSheet` attached to the requested window have no ancestry to the requested CGWindowID by construction, so window-scoped semantic actions on them are refused as "outside the target window". | #3351; alternative design to draft PR #3353 (which walks a bounded ancestry chain to the parent window for every route) | #3793 (with P11) |
| P6 | `fix(cua-driver-core): publish observed post-dispatch change as evidence without laundering it into confirmed` | Legacy structured output can carry a `window_change` observation after dispatch; the typed action record either dropped it or would let it promote the effect to `confirmed`. | PR #3373 (post-action rediscovery; typed `window_change`) — if it lands first, rebase onto its typed field | conditional on #3373 — unfiled |
| P7 | `fix(macos): post each mouse event through exactly one transport` | Upstream posts window-scoped mouse events through SkyLight `SLEventPostToPid` *and* `CGEvent::post_to_pid`; a receiver accepting both sees down/down/up/up. Upstream's `MousePostMode` (#2907) only moved foreground clicks to public-only; the background path still dual-posts. | #2874 (maintainer asked for exactly this regression) | yes — strongest candidate — unfiled |
| P8 | `feat(macos): cancellation-aware input and tool workers` | Depends on P2: input primitives prepare their release before the press and release on cancel/unwind; pacing wakes on cancellation; blocking tool workers carry the operation. | with P2 | RFC #3796 (with P2) |
| P17 | `fix(macos): capture exact window pixels via SCScreenshotManager; never fall back to the inset legacy API` | On macOS 26 `SCScreenshotManager::capture_image` returns an inset frame; every pixel coordinate the agent reads is then off. Uses `capture_screenshot` (macos_26_0 API, `screencapturekit` 8.0.1) with shadow/cursor excluded and validates geometry before bytes escape; a modern failure stays a failure. | none found (SCK dependency bump landed upstream as #3680) | yes — unfiled |
| P18 | `fix(macos): exact launch identity, list_windows AX metadata, lossless AX values, file-item actions` | Several exact-targeting gaps: launch by name could match the wrong bundle; `list_windows` lacked AX metadata; empty/whitespace `AXValue` was replaced by the placeholder hint and raw newlines could fabricate tree rows; hosted Open/Save panels and sheets could not be addressed as part of the exact window; file items hid their open/confirm actions. | PR #3456 (accessory-app running state) overlaps the launch-identity part | #3787 (lossless-value part); launch-identity part unfiled (dedupe against #3456) |
| P9 | `feat(macos): session-owned rendering lease with bounded capture start (capture_timeout)` | Covered Chromium renderers stop painting without a display stream; upstream has no session-owned lease and `SCStream.start_capture` can hang forever (`cua-sck-window-capture` stuck in `SyncCompletion`, #2002). Adds a bounded start with a typed `capture_timeout` and drains the lease through P2's owned runtime resources. | #2002 | RFC #3796 (with P2) |
| P16 | `fix(macos): thread-scoped AX walk budget with deadline-aware bindings` | Upstream bounds each native AX request with a per-element messaging timeout, so a wedged app can extend an observation element by element. One thread-scoped deadline across the walk, deadline-aware binding wrappers, partial-state reporting (`ax_walk_timed_out`, `stop_reason`), and a refusal instead of substituting another window's controls. | #1537, draft PR #1755 (wall-clock guard this supersedes) | yes — unfiled |
| P10 | `feat(macos): read AXDocument/AXEdited for the requested window` | macOS half of P4: `AXDocument` and `AXEdited` (window, then close button). | none found | #3795 (with P4) |
| P11 | `fix(macos): prove sheet and menu-bar ancestry in exact_target` | macOS half of P5: proves `ProvenAppMenu` / `ProvenAttachedSheet` ancestry and names the owning window in refusals. | #3351 / #3353 | #3793 (with P5) |
| P12 | `fix(macos): resolve menu activation from WindowServer, not cached NSWorkspace state` | `invoke_menu` polls `NSWorkspace.frontmostApplication`, an AppKit cached value that never refreshes on the blocking menu path; upstream still reads it. Kept alongside upstream's main-queue affinity for the embedded self-pid case (`2dbc1c2c`). | none found | #3781 |
| P13 | `fix(macos): poll for the Chromium AX tree instead of a fixed 0.5 s sleep` | After `AXManualAccessibility` upstream sleeps 500 ms, too short on a loaded machine and wasted otherwise. Side effect to disclose: VS Code shows its screen-reader toast on first assertion. | #1756 | #3783 (APPROVED) |
| P14 | `fix(macos): verify bring_to_front against the process it activated` | Upstream verifies by global z-order and false-negatives when another process owns the topmost window; refines the merged overlay exclusion (`44c9d1f6`, #3704 / #2829). | #2829, #3704 | #3785 (APPROVED) |
| P15 | `feat(macos): probe delivery of background clicks` | A background click on a window that ignores synthetic input reported success because the events were posted; pre/post AX probe classifies delivery and escalates a no-op. | #3389 is the `type_text` sibling of the same class; PR #3373 overlaps the evidence rule (see P6) | conditional on #3373 — unfiled |
| L1 | `feat(linux): report background_input routes on get_window_state` | Linux refuses background routes with the right predicates but never publishes the core `background_input` routes object the agent needs to choose a route up front. | none found | #3791 |
| L2 | `fix(linux): return a structured foreground_unavailable code on X11 foreground failure` | The X11 foreground path builds `anyhow!("foreground_unavailable: ...")`, which surfaces as untyped error text; the Hyprland path already emits the typed `{code, detail}` envelope. | none found | yes — unfiled |
| L3 | `fix(linux): set screenshot_frame_valid on successful get_window_state` | Linux only ever reported `screenshot_frame_valid: false` on the refusal path and never `true` for a delivered per-window frame, which is by construction identity-validated on that path; macOS reports `true`. | none found | #3789 |

Dropped from the pre-0.28 fork or from the original plan:

- The fork's `manifest.json` and `cua_driver_abi.h` hunks: regenerated, not merged.
- `MousePostMode::Both` (upstream #2907) is renamed `SkyLightFirst` in P7 because
  its dual post is exactly the duplicate delivery P7 removes; foreground
  `PublicOnly` is unchanged.
- P19 (`layer` on Linux `list_windows` rows) was dropped: the 0.28.0 contract
  makes `layer` nullable, so the consumer's parser is relaxed instead.
- Carrying upstream PR #3658 (`press_key` `return` on a pty, #3657) was
  dropped: the Linux smoke baseline on 0.28.0 shows `press_key {key:"return"}`
  landing at an xterm, so the regression is not present here.
- Linux emits only the `routes` array of `background_input`; macOS's
  `exact_window` / `observation` sections are WindowServer-specific and have no
  honest Linux analogue.

Ordering note: the stack is applied in the order of the table (P1 → P8, then
P17, P18, P9, P16, then P10 → P15, then L1 → L3). P16 comes after P9/P17/P18
so that the AX walk budget lands on the final shape of `ax/tree.rs`; each
commit compiles and its crate tests pass on its own.

Behavioural fixes made while porting (not in the pre-0.28 fork):

- P2: an explicitly shut-down runtime no longer schedules a second, unobservable
  cleanup thread from its finalizer, and a trusted session abandoned while
  shutdown is in progress leaves the drain to shutdown instead of racing it
  (upstream's `direct_runtimes_have_independent_sessions_and_shutdown`).
- P3: shutdown interrupts pacing but does not rewrite a completed call's result
  as `cancelled` (upstream's `shutdown_drains_an_already_admitted_call`); a
  cancel that arrives before its call registers (concurrent dispatch) is held
  briefly and applied at registration.
- P2: a capture whose caller cancelled stops after the AX walk and publishes
  nothing (`operation::cancelled_by_caller`, get_window_state.rs post-walk
  gate); shutdown only interrupts, so that call still publishes and is retired
  (upstream `sdk_cancelled_capture_does_not_publish_after_shutdown` /
  `sdk_shutdown_drains_snapshot_publication_and_retires_the_result`).

# Bench fixes carried on `will/bench-fixes`

`will/bench-fixes` is `will/omp` at `54a2437d8` plus the patches below, in
stack order, one commit per row. Every one comes from a measured agent run
(`trycua-omp/research/bench-set-20260911/`), so the "what the runs showed"
column cites the transcripts rather than a design opinion. Generated files
are regenerated, never merged. Waves 1–4 add nothing to the public contract;
from wave 5 on, the contract version moves and the rule is the additive-over-
0.8 one stated at the top.

| # | Patch (commit title) | What the runs showed | Upstream PR? |
|---|---|---|---|
| B1 | `fix(macos): commit set_value writes through the app's editing pipeline` | An `AXValue` write reads back correctly while the app's editor never sees it; a Save-panel filename was discarded and the file landed as `Untitled.txt`. Adds the `committed` flag. | yes |
| B2 | `fix(macos): report an unobserved click as unverified, not undelivered` | The four-signal probe cannot see every effect, yet the reply asserted "NOT delivered" and blamed `pointerdown` on AppKit targets, routing the model away from presses that had landed. | yes |
| B3 | `fix(macos): clamp the scroll amount on the keystroke path` | The window path passed the raw `amount` into its keystroke loop: one `amount: 1100` call spent 82 s scrolling. | yes |
| B4 | `fix(macos): read typed text back from the element that received it` | AppKit installs a field editor over the edited field, so the pinned pointer kept reading the pre-edit string and every complete insertion in Contacts was reported `type_text_incomplete`. | yes |
| B5 | `fix(macos): right-click an element by pixel when AXShowMenu opens no menu` | `AXShowMenu` returns success on controls that open no menu, so `click(button:"right")` reported success with no menu anywhere. | yes |
| B6 | `feat(macos): ship AXHelp and AXDescription in structured elements` | `label` collapses title/description/value, so a caller reading structured rows could not tell Reminders' completion toggle from an open action. | `help` yes; `description` blocked by the manifest generator stripping keys named `description` |
| B7 | `fix(macos): report elements_complete from the AX walk's own verdict` | `elements_complete` was hard-coded `false`, so a predicate that matched nothing was never unsatisfied, only unknown. | yes |
| B8 | `fix(linux): scope the AT-SPI element cap to the requested window` | `max_elements` was charged to every node of every window of the application, so asking for 10 elements of a second window returned no controls at all. | yes |
| B9 | `fix(macos): report an AX action the app answered as dispatched, not failed` | `AXUIElementPerformAction` replying `-25200`/`-25205`/`-25206` was a tool-invocation failure, which aborts the caller's cell before the observe inside it — 17 steps across the Mac leg, on presses that had usually landed. Now `effect: "unverifiable"` with `delivery.mode: "unknown"`, the post-dispatch probe's evidence and the window-change suffix. | yes |

B9 keeps the error contract for the framework-level codes (`-25201`
illegal argument, `-25202` dead element, `-25204` messaging timeout, `-25211`
API disabled): none of those is an application answering an action it
received, and `-25204` in particular is the AX messaging deadline the walk
budget also raises, where nothing having happened is the common case.

## Wave 4 (7a520407d..72c4c94e0) — rows missing from the ledger

| # | Patch (commit title) | What the runs showed | Upstream PR? |
|---|---|---|---|
| B10 | `fix(macos): settle the AX write's read-back before calling it partial` (7a520407d) | A field that publishes `AXValue` after the write read back short once and was reported `type_text_incomplete`. | #3842 (hold: #3897) |
| B11 | `fix(macos): watch a dispatched action long enough to see it land` (8a01c1079) | The post-dispatch probe's settle window closed before AppKit had rendered the effect. | unfiled; with the delivery-probe PR (#3373-conditional) |
| B12 | `fix(testkit): report why the driver child stopped answering` (95ee4cf1e) | A daemon/driver version mismatch surfaced as "missing window" (#3843). | #3844 |
| B13 | `fix(macos): keep a foreground key chord's modifiers on its base key` (cb2533ac7) | Foreground chords arrived with no modifier flags on the base key (#3849). | #3855 |
| B14 | `fix(macos): a click on a text role focuses it instead of pressing it` (a100d2c80) + `keep the no-op wording off a click that focused a text control` (3a2a160e6) | Plain click on a text field dispatched an unadvertised `AXPress` (#3850); the focusing click then read "no-op". | #3856 (fold 3a2a160e6) |
| B15 | `fix(contract): report a value write's commit as a verdict, not a boolean` (c114d3cec) | A bound control echoes the read-back, so `committed: true/false` cannot be honest (#3851). | #3857 → fold into #3858; needs the contract RFC |
| B16 | `fix(macos): write a bound text field by typing, and judge the commit honestly` (58f557bd3) + `only a typed edit session ending is a commit, and keep a save panel reachable` (aaa61d980) | `set_value` on a native single-line field never reached the binding (#3852); a save panel became unreachable after the typed route. | #3858 (fold aaa61d980) |
| B17 | `fix(macos): classify an AX reply before the selection fallback` (24239691c) | Reviewer-found: −25200/−25205/−25206 then fell into `select_nearest_container` and wrote `AXSelected`. | #3836 (c2e6298df) |
| B18 | `fix(macos): stop reporting the walk deadline as a native failure and tolerate isolated ones` (15ea14ee8) | A single wedged element's timeout ended the snapshot as a native error. | unfiled; with P16 |
| B19 | `fix(macos): stop walking rows a scrolling container is not showing` (c76cd31db) | Notes' list spent the whole deadline on unrealized rows (#3906). | #3907 |
| B20 | `feat(macos): name the windows that appeared in a click's delivery evidence` (94a7fba9a) | Evidence said "a window changed" without naming which. | unfiled; delivery-probe PR |
| B21 | `feat(macos): classify the window-sharing indicator in list_windows` (4937e10a4) + `publish whether a window row is accessibility-backed` (07d2deb5a) | The 66×20 capture indicator listed as an ordinary window (#3909); consumers could not tell an AX-backed panel from a bare CGWindow. | #3910 (fold 07d2deb5a) |
| B22 | `fix(macos): classify typed delivery from the settled read, not the window's strongest one` (258e289b6) | The strongest-focused-value heuristic mis-classified the settled read. | #3921 (hold: #3897) |
| B23 | `feat(macos): name the window that holds focus when foreground HID delivery is refused` (91dcc591c) | The refusal withheld the focus holder the driver had just read (#3918). | #3923 |
| B24 | `feat(macos): give a drag the same delivery evidence a click carries` (f3a4e3959) | Drags reported posted, never probed. | unfiled; delivery-probe PR |
| B25 | `fix(macos): read an application's own actions out of the envelope macOS ships them in` (4af44c0ac) + `dispatch a custom action by the name the element advertises` (e84bdf625) | macOS's three-line action envelope broke `tree_markdown` (#3919); named custom actions were not invocable. | #3947 |
| B26 | `fix(macos): refuse a click action the element does not advertise` (da38cc568) + `…on every dispatch path` (bd4251f94) | An unrecognised action name dispatched as `AXPress` (#3920). | #3922 |
| B27 | `feat(macos): let a caller that enumerates windows itself decline the post-action window poll` (63dbd1af7) (+ fixture 932f52297) | ~1 s unconditional post-action poll, 64–93% of in-driver time, opt-out unreachable (#3924). | #3946 |
| B28 | `perf(macos): do not re-activate a drag target that is already frontmost` (259895a41) | Redundant activation per drag. | #3925 |
| B29 | `feat(macos): list a menu submenu's items instead of pressing the path's final segment` (17fabf889) + `let a menu submenu listing answer without an ActionResult` (a8d6357d5) | `invoke_menu` pressed a submenu-bearing segment and discarded the labels it enumerated (#3944). | #3945 |
| B30 | `perf(macos): cap the probe settle when there is no element state to compare` (096cbdd0b) + `stop claiming signals the probe never watched` (b4bf36937) | Probe waited its full budget with nothing to compare; replies claimed unwatched signals. | unfiled; delivery-probe PR |
| B31 | `feat(macos): keep a disabled control addressable and publish its subrole` (2237aa939) + `drop the subrole field doc the TS generator embeds` (6840dd292) | Disabled controls got no `element_index`; `AXSubrole` never read (#3949). | #3950 |

## Wave 5 (72c4c94e0..6ba974075)

`will/bench-fixes` at 6ba974075 is wave 4 (72c4c94e0) plus the two merged topic branches `will/w5-text` (cd10bf957) and `will/w5-click` (b3b0174d1), merged at a5c3df23f, then four post-bench fixes. Contract 0.9.0 (6ee6d7632) is a declared break: typed SDK callers must add `ClickInput.detect_window_change`, and `ActionEvidenceKind::WindowChange` is renamed. Every row below is measured (`~/bench-runs/20260919*`); none has an upstream issue or PR yet except where a column says so.
 Superseded at wave 6: b76016c77 makes 0.10 additive over 0.8, so the "declared break" no longer holds — the `signal` / `RetainedElement` / fixture re-expressions W5-2, W5-24 and W5-28 ask for are the wave-6 rows below.
| # | Patch (commit title) | What the runs showed | Upstream PR? |
|---|---|---|---|
| W5-1 | `fix(macos): type into the window's focused element on both rungs` (4bbcdda9e) | Notes, window 16933, search field focused: both rungs replied "Sent (unverified) 22 char(s) via CGEvent"; foreground rung landed nowhere — AppKit reinstalled the remembered responder (note list) after activation and the keystroke path's focus re-apply was gated on an element index a window-scoped call never has. | premise unfiled; re-express on #3897's `focused_element_in_window` + retained target |
| W5-2 | `feat(contract): publish the moved signal on observed-change evidence` (74c3da106) | Live AX background click: `{"effect":"unverifiable","evidence":[{"kind":"window_change"}]}` while only `element_state` moved; `detect_window_change` is false on every OMP action, so `window_change` was never a window change. | RFC needed (public contract); re-express additively (`signal` field, keep the kind name) |
| W5-3 | `feat(contract): add element and snapshot escalation targets` (8e987cf67) | Refusal prose named `get_window_state` to a consumer whose surface has no such tool; harness printed the raw token. | same RFC |
| W5-4 | `feat(contract): typed refusal codes and bring_to_front obscured_by` (3d011044f) | Dead-element refusal carried only `{"code":"element_outside_target_window"}`; disabled control was `tool_invocation_failed`. Docs only (contract README + action-result-contract.md). | same RFC (docs half) |
| W5-5 | `fix(macos): a click on a selectable row selects; press needs an explicit action` (490e3fce3) | Reminders: click on AXCell "Incomplete, Buy milk" → "Performed AXPress", reminder COMPLETED, nothing selected; a pixel click at the same centre selects. | yes — issue + PR (fixture row `row-pressable`) |
| W5-6 | `fix(macos): say when no text destination resolved instead of pointing at a screenshot` (f21e6af9f) | Notes window-scoped type with no resolvable field: "verify via screenshot" on both rungs; foreground named as the fix when the path was the problem. | with the #3897 re-expression (its wording is "observe the target before retrying") |
| W5-7 | `fix(macos): name the element a reply acted on` (86389bfc3) | `Performed AXPress on [40] AXCell ""` / `Selected nearest AXRow for [18] AXCell ""` — AppKit cells carry the label in `AXDescription`; the reply read `AXTitle` only. | yes — small PR |
| W5-8 | `fix(core): carry the platform's observed escalation reason and name the pid-routed key route` (14f81c34b) | Background chord never probed → `{"reason":"delivery_failed","target":"foreground"}` fabricated from a static table; `key_events_fg` published `route: global_input` for a pid-routed post. | projection half: same RFC; route token: chord PR; drop the `from_legacy` half (RFC 3473) |
| W5-9 | `fix(macos): keep a content-bearing AXGroup addressable` (ca32c1c2d) | Reminders row `AXGroup` with actions [AXPress, Move Up, …] and help "To mark as completed, press Control-Option-Space." collapsed before its attributes were read. | yes — with #3950's family |
| W5-10 | `fix(macos): verify typed text against the field's prior value` (324dbaf76) | Notes search field still held "warehouse pallet audit" from a refused clear → "[OK] Inserted 22 char(s) … verified" for a write that delivered nothing; TextEdit autocapitalised → `Partial(22)`. | issue #3917 filed; the echo rule is what #3897's `typed_progress` does (`before == after → Unchanged`); `Normalized` conflicts with #3897 (non-byte-identical growth stays Unverifiable) — do not file that half |
| W5-11 | `chore(contract): bump CONTRACT_VERSION to 0.9.0 and regenerate` (6ee6d7632) | Manifest/UniFFI/TS/Python regenerated; `manifest_is_sorted_and_versioned` now asserts `CONTRACT_VERSION`. | fork-only (upstream bumps in its own release) |
| W5-12 | `fix(macos): spell the undelivered remainder on a partial type` (f8c44d6d2) | "retry only the remaining suffix" left the caller slicing by codepoint; reply now carries `retry_text`. | do not file — #3897 makes trusted partials `retryable: false` and drops `retry_from_character` |
| W5-13 | `fix(macos): refuse a disabled control with the state that disabled it` (8ba9550f0) | Chrome and Notes, four states: byte-identical "Retry with delivery_mode:\"foreground\" or call bring_to_front first" while both routes were already taken; no `code`. Now `code: element_disabled`, `effect: not_dispatched`, `obscured_by`. | yes — one PR with W5-16/W5-24/W5-25/W5-26 (mirrors #3888's structured escalation on Windows) |
| W5-14 | `fix(macos): a confirmed set_value on a search field reports committed when the value changed` (5082b2c90) | 0917 write census: 23 writes landed, 22 reported `committed: unproven` because `ValueThenConfirm` could never show a typed edit ending. | fold into #3858 |
| W5-15 | `fix(macos): probe a chord's effect instead of reporting it pressed` (174797eaa) | Notes `cmd+option+f`: 0/12 dispatches landed, every reply "Pressed cmd+option+f on pid 91895". | chord PR (DriverT11 owns hotkey.rs) |
| W5-16 | `fix(macos): bring_to_front verifies behind the app's own panel and names it` (3bf900306) | Notes/Chrome `reveal()` → `bring_to_front_exact_window_unverified` with `front_in_process=false`; the blocker (17013) was the same pid's panel; both candidates refused. | follow-up to #3785 after it merges; `obscured_by` resolver shared with W5-13 |
| W5-17 | `fix(macos): make the target window key before a window-scoped chord` (a4dcd1948) | `kCPSNoWindows` front never made the window key: 0/12 chords while not key, 6/6 once key; toolbar search control collapsed by front-and-restore. | chord PR — DriverT11 is replacing this mechanism (dispatch the chord as the menu command it is a key equivalent of); land T11's version |
| W5-18 | `fix(macos): compose the chord escalation from the key-window fact` (3207f2963) | Static "menu key-equivalents often need the window fronted" emitted after chords that had landed; now `key_window: {target_window_id, focused_window_id, is_key}` + escalation only when not key. | chord PR |
| W5-19 | `fix(macos): say an element no longer exists instead of doubting its window` (addde6ba1) | Destroyed Reminders row → `element_outside_target_window … could not be proven to belong to window 17268` while `AXRole`/`AXWindow` answered −25202; now `element_no_longer_exists`, `escalation.target: snapshot`; `BackgroundAdvice` typed. | yes — one PR with W5-21 + W5-3's core arm |
| W5-20 | `fix(macos): name the HID tap's own path token on foreground keys` (d1ef14c6c) | `key_events_fg` labelled both the HID tap and the pid post; 0.9.0 maps it to `synthetic_events`, so the HID branches needed `key_events_hid_fg`. | with W5-8's route half / chord PR |
| W5-21 | `fix(macos): typed advice instead of tool names in background refusals` (63b5e43c4) | Refusals named `get_window_state`/`page` (consumer tools); `ProvenAppMenu` shared `Unproven`'s arm and was told to re-observe. | with W5-19; the `ProvenAppMenu` arm goes into #3793's next push |
| W5-22 | `test(macos): a menu key equivalent through hotkey on both rungs` (8b1da0438) + `assert the menu chord on the surface the contract publishes` (a7415f702) | No cell asserted a chord reaches NSMenu; wave-4's silent chord drop was invisible. Fixture Window ▸ Arrange ▸ Left owns `cmd+option+l`; cell asserts route/effect/escalation only. | with the chord PR |
| W5-23 | `fix(macos): state the key-window refusal without naming one transport` (dacdce172) | `ForegroundActivationRefused` said "for foreground HID delivery" on the pid-routed menu branch. | with the chord PR |
| W5-24 | `fix(macos): keep the probed element alive across the dispatch it may destroy` (d004e4c59) | `press_key escape` SIGTRAP 5/5: `CF_IS_OBJC ← CFGetTypeID ← _AXUIElementValidate ← try_copy_string_attr ← DeliveryProbe::compare`; the probe held a bare `usize` the dispatch freed. Probe now retains; `ElementRead::{State,Gone,Unreadable}`; `Evidence::ElementGone`. | yes, urgent (daemon crash) — re-express on upstream `RetainedElement::retain` (+ add `adopt`), delete `ax/owned.rs` |
| W5-25 | `fix(macos): a system overlay is never the panel in front` (7c7f4a853) | `element_disabled` named the 66×20 capture-lease indicator (19083) as the panel in front and told the agent to dismiss its own capture. | with W5-13 |
| W5-26 | `fix(macos): a disabled control on a window that is not key names the foreground rung` (5a2dbf1d5) | Notes toolbar search field `AXEnabled=false` in background; `ChordProbe` saw it enable once the window was key; reply said no route exists. Fourth arm + `key_window` facts + `escalation: {target: foreground, reason: route_unavailable}`. | with W5-13 |
| W5-27 | `fix(macos): classify the capture-lease indicator from the full window list` (6ba974075) | W5-25 did not fire live: the provider view is enumerated off-screen, so `visible_windows()` never enclosed the host row; resolver now takes `all_windows()` with on-screen gating for nameable rows. | with W5-13 |
| W5-28 | fixture fixes `carry/restore the new ClickInput field` (64d906624, a90ad3d07) | 0.9.0 struct-literal break took the whole `cua-driver` test crate down; same one-line fix on each topic branch. | fork-only; squash into W5-11 |

## Sync with trycua/cua main (c5550997b)

`Merge trycua/cua main (9bbfa7dd3) into will/bench-fixes` (c5550997b) brings
the fifty upstream commits since the fork point e7e141aec: the runtime-owned
snapshot cache (#3616 a8a7b1e5e: `element_cache.resolve_element_args`
returning a retained `RetainedElement` guard, `retire_runtime_scope`), cursor
hooks (#3883), the PiP registry removal (#3752), Linux AT-SPI identity
(#3864), the perception-boundary RFC (#3934), AGENTS.md dedup (#3927) and
releases 0.28.1/0.28.2. Twelve files conflicted, every one #3616's new
element-cache prologue meeting our cancellation-aware workers, deadline-aware
AX walk and delivery probes; upstream's structure won and our semantics were
re-expressed inside it (`operation::spawn_blocking`, the double-click element
path retaining its guard from the resolved snapshot, `get_window_state`
awaited to completion, `cleanup_runtime_resources` retiring the scope through
`element_cache::retire_runtime_scope`). Upstream's
`sdk_cancelled_capture_does_not_publish_after_shutdown` was a direction
conflict with P2 (see the P2 bullet above): `operation::cancelled_by_caller()`
separates a vanished caller from a shutdown interrupt. Nothing upstream
superseded any row above (0 hits on origin/main for the stack's identifiers).

## Wave 6

| Patch (commit title) | What the runs showed | Upstream PR? |
|---|---|---|
| `feat(macos): dispatch a not-key window's disabled key equivalent as its menu command` | Notes keeps `Edit > Find > Note List Search…` (⌥⌘F) disabled until its window is key; a background chord there landed 0/12 and the model never took the foreground rung it was offered (0/7, T11 on 0919b). Measured live: `invoke_menu` fronts Notes, makes the window key, presses and restores ~350 ms later — before Notes runs the command, so the search field never held focus through it either (0/2); with the window kept key it does (3/3) and it releases the moment the prior app is re-fronted (3/3). The chord now resolves to the item by `AXMenuItemCmdChar`/`CmdVirtualKey`/`CmdModifiers`, is dispatched as that item with the window key, the app's reaction is waited for before the restore, and the reply says what moved: `route: menu_command`, `delivery: foreground`, `menu_path`, and whether the reaction survived the restore (`escalation.target: element` when it did not). Contract 0.10.0: `ActionRoute::MenuCommand` and the optional `ActionResult.menu_path`. | pending |
| `refactor(macos): retain the delivery probe's element on RetainedElement and drop ax::OwnedElement` (cd0861727) | Consolidation after the sync, not a run: `ax::OwnedElement` (W5-24, d004e4c59) and upstream #3616's `RetainedElement` were the same guard, so the probe, `press_key` and `type_text` now retain on `RetainedElement`, which gains the +1 handoff `adopt`; owned.rs is gone (−128 LOC) and W5-24's tests move with the primitive. The press_key SIGTRAP fix is unchanged: the probe owns what it watches. | yes — "the delivery probe retains its watched element", on #3616's primitive (W5-24's row) |
| `refactor(macos): build the click and set_value action records directly` (ebcbb451a) | RFC 3473 direction: producers state `ActionExecutionRecord`; `from_legacy` loses branches per migrated family. The AX click path (`ax_click_record`, probe verdict as `ProbeReport`) and `set_value` (`action_record`, builder gains `committed`) now state their contract; the `delivery_mode: "unknown"` legacy branch and its test are deleted. Still on the legacy payload and its vocabulary: `type_text` (`committed`, `escalation.target`; #3897 rewrites it), `hotkey` (`target: element`, `key_events_hid_fg`, `menu_command`, `menu_path`), `drag`/`hotkey` (observed-change rows). Wire unchanged for every legacy input. | with the contract RFC's "slice 2" (W5-8's from_legacy half is now partly gone) |
| `fix(contract): keep 0.10 additive over 0.8 — window_change stays the kind, typed inputs gain constructors` (b76016c77) | Reverts W5-2's rename (`ActionEvidenceKind::ObservedChange` → `WindowChange` again, `signal` kept; manifest delta is exactly the nine spellings, schema keeps 0.8's `enum` shape) and answers W5-11/W5-28's struct-literal break: `ClickInput`/`DragInput`/`ScrollInput`/`TypeTextInput`/`PressKeyInput`/`HotkeyInput::new(required…)` leave the optional fields unset and `detect_window_change` carries `#[uniffi(default = None)]`, so the generated Python constructors no longer require it. `CONTRACT_VERSION` stays 0.10.0; manifest, both UniFFI binding sets regenerated; C ABI header unchanged. OMP reads `kind === "window_change"` again and re-vendors the manifest fixture. | the contract RFC (W5-2/W5-3/W5-4/W5-8) now describes a purely additive 0.10 |
