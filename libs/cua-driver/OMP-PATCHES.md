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
| P5 | #3793: re-review at 5230ec1d2 (pushed 09-20 with the ProvenAppMenu advice arm; reply posted, "ready for another look"); clock restarted 09-20 |
| P6 | `fix(cua-driver-core): publish observed post-dispatch change as evidence without laundering it into confirmed` | Legacy structured output can carry a `window_change` observation after dispatch; the typed action record either dropped it or would let it promote the effect to `confirmed`. | PR #3373 (post-action rediscovery; typed `window_change`) — if it lands first, rebase onto its typed field | conditional on #3373 — unfiled |
| P7 | `fix(macos): post each mouse event through exactly one transport` | Upstream posts window-scoped mouse events through SkyLight `SLEventPostToPid` *and* `CGEvent::post_to_pid`; a receiver accepting both sees down/down/up/up. Upstream's `MousePostMode` (#2907) only moved foreground clicks to public-only; the background path still dual-posts. | #2874 (maintainer asked for exactly this regression) | yes — strongest candidate — unfiled |
| P8 | `feat(macos): cancellation-aware input and tool workers` | Depends on P2: input primitives prepare their release before the press and release on cancel/unwind; pacing wakes on cancellation; blocking tool workers carry the operation. | with P2 | RFC #3796 (with P2) |
| P17 | `fix(macos): capture exact window pixels via SCScreenshotManager; never fall back to the inset legacy API` | On macOS 26 `SCScreenshotManager::capture_image` returns an inset frame; every pixel coordinate the agent reads is then off. Uses `capture_screenshot` (macos_26_0 API, `screencapturekit` 8.0.1) with shadow/cursor excluded and validates geometry before bytes escape; a modern failure stays a failure. | none found (SCK dependency bump landed upstream as #3680) | yes — unfiled |
| P18 | #3787 pushed 09-20 at 87d032b9f as the element-row family home (#3813/#3817/#3907 folded and closed); never reviewed; no ping due |
| P9 | `feat(macos): session-owned rendering lease with bounded capture start (capture_timeout)` | Covered Chromium renderers stop painting without a display stream; upstream has no session-owned lease and `SCStream.start_capture` can hang forever (`cua-sck-window-capture` stuck in `SyncCompletion`, #2002). Adds a bounded start with a typed `capture_timeout` and drains the lease through P2's owned runtime resources. | #2002 | RFC #3796 (with P2) |
| P16 | `fix(macos): thread-scoped AX walk budget with deadline-aware bindings` | Upstream bounds each native AX request with a per-element messaging timeout, so a wedged app can extend an observation element by element. One thread-scoped deadline across the walk, deadline-aware binding wrappers, partial-state reporting (`ax_walk_timed_out`, `stop_reason`), and a refusal instead of substituting another window's controls. | #1537, draft PR #1755 (wall-clock guard this supersedes) | yes — unfiled |
| P10 | `feat(macos): read AXDocument/AXEdited for the requested window` | macOS half of P4: `AXDocument` and `AXEdited` (window, then close button). | none found | #3795 (with P4) |
| P11 | as P5 |
| P12 | `fix(macos): resolve menu activation from WindowServer, not cached NSWorkspace state` | `invoke_menu` polls `NSWorkspace.frontmostApplication`, an AppKit cached value that never refreshes on the blocking menu path; upstream still reads it. Kept alongside upstream's main-queue affinity for the embedded self-pid case (`2dbc1c2c`). | none found | #3781 |
| P13 | `fix(macos): poll for the Chromium AX tree instead of a fixed 0.5 s sleep` | After `AXManualAccessibility` upstream sleeps 500 ms, too short on a loaded machine and wasted otherwise. Side effect to disclose: VS Code shows its screen-reader toast on first assertion. | #1756 | #3783 (APPROVED) |
| P14 | `fix(macos): verify bring_to_front against the process it activated` | Upstream verifies by global z-order and false-negatives when another process owns the topmost window; refines the merged overlay exclusion (`44c9d1f6`, #3704 / #2829). | #2829, #3704 | #3785 (APPROVED) |
| P15 | `feat(macos): probe delivery of background clicks` | A background click on a window that ignores synthetic input reported success because the events were posted; pre/post AX probe classifies delivery and escalates a no-op. | #3389 is the `type_text` sibling of the same class; PR #3373 overlaps the evidence rule (see P6) | conditional on #3373 — unfiled |
| L1 | #3791: re-review at 6ea2a9531 (pushed 09-20 over #3864; ladder advertises ElementClickNeedsForeground / ClickActionUnavailable; reply with the Ubuntu 21/21 matrix posted); clock restarted 09-20 |
| L2 | `fix(linux): return a structured foreground_unavailable code on X11 foreground failure` | The X11 foreground path builds `anyhow!("foreground_unavailable: ...")`, which surfaces as untyped error text; the Hyprland path already emits the typed `{code, detail}` envelope. | none found | yes — unfiled |
| L3 | #3814 pushed 09-20 at d582a6a1f with #3789 folded (closed); parked as draft behind #3791; RFC 3931 calls screenshot_frame_valid non-portable, expect pushback when it is promoted |

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
| B1 | - (#3816 closed 09-20 into #3858; #3857 closed the same way; #3858 parked as draft until RFC #4009 records a decision) |
| B2 | `fix(macos): report an unobserved click as unverified, not undelivered` | The four-signal probe cannot see every effect, yet the reply asserted "NOT delivered" and blamed `pointerdown` on AppKit targets, routing the model away from presses that had landed. | yes |
| B3 | - (folded into #3946, pushed 09-20 at f3ccc1b27; #3806 closed) |
| B4 | #3811 (injaneity 09-16): hold acknowledged 09-20; #3842/#3921 closed "for now"; parked as draft until #3897 lands or closes |
| B5 | - (folded into #3922, pushed 09-20 at a798a5773; #3810/#3856 closed) |
| B6 | - (folded into #3787, pushed 09-20 at 87d032b9f; #3813 closed; `description` still blocked by the manifest generator, #3802) |
| B7 | - (folded into #3787, pushed 09-20 at 87d032b9f; #3817 closed) |
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
| `chore(cua-driver): add the OMP harness recipe as scripts/omp/harness.sh` + `test(cua-driver): register fork AppKit harness cases in the canonical macOS E2E allowlist` | Every Mac lane re-derived the same recipe: no cua-driver app identity here holds Screen Recording, so a `serve` daemon reports every window title as `""` and the AppKit harness dies at `find_window`; the installed `~/.omp/natives/cua-driver` binary in `mcp --direct` mode as a child of the granted terminal does see titles. `scripts/omp/harness.sh` (preflight / fixture / build / install / run / evidence / restore / allowlist-check, `--dry-run` on every mutating step) ships `direct-shim.sh` (rewrites `mcp --socket` to `mcp --direct`), a listening-only Unix socket for the testkit's reachability probe, rm-then-copy installs signed `OMP Computer Use`, `evidence.jsonl` → the exact-head markdown table. `allowlist-check` found 11 fork `harness_appkit_*` cases missing from `run-rust-e2e.sh`; they are registered now. | fork-only tooling; the allowlist wiring travels with each PR that adds its cell (maintainer expectation: same commit) |
| `chore(cua-driver): harness.sh builds both fixtures and refuses a stale one` (567299738) + `chore(cua-driver): harness.sh run exports the E2E recordings root` (35b5f14f7) | The 2026-09-20 exact-head pass ran the fork head against an AppKit bundle built on 09-18, before f772173f7 taught the fixture `CUA_APPKIT_SECOND_KEY_WINDOW` (and the sync brought `CUA_APPKIT_SNAPSHOT_DIR`): `harness_appkit_disabled_until_key_chord_lands_as_its_menu_command` died at `find_window("Second Key Window")` and the snapshot-publication cell at `fixture did not publish state`, both reading as driver regressions; the same binary passes both once the bundle is rebuilt. `fixture` now builds AppKit and the Electron sentinel (background cells launch it) and stamps each bundle with the sha256 of its sources; `preflight` fails and `run` refuses a bundle whose stamp is missing or stale. `run` exports `CUA_E2E_RECORDINGS_ROOT` as `run-rust-e2e.sh` does, so the snapshot-publication cell runs at all. | fork-only (recipe); the stale-fixture lesson is why a PR's evidence names the fixture build alongside the binary sha |
| `test(cua-driver): assert the competing-window cell against the postcondition bring_to_front ships` (cfbadaf1a) | `harness_appkit_exact_activation_refuses_competing_window` had been red on this branch since 01ac07878 (P14): the upstream cell requires a refusal when another application keeps its window ordered front, the exact case P14 reclassifies as verified. #3785's approved head (72a44c824 + 42052d0e9) rewrote the cell as `harness_appkit_exact_activation_ignores_competing_application_window` (competitor must lead the global layer-0 order first, then `bring_to_front_exact_window_verified` with `focused`/`front_in_process` true and the pointer unmoved) and renamed the allowlist row, on the PR branch only. The fork now carries that rewrite, asserting the fork's `front_in_process` field; a8636718a's per-display scoping (`front_in_process_on_display`) stays on the PR branch because it would rename a documented field here. Driver unchanged. | P14 (#3785, approved); nothing new to file |

## Maintainer expects (2026-09-20)

Posted 2026-09-20 (round 1 of the re-engagement): RFC #4009 (ActionResult vocabulary) filed; #3816/#3857 closed into #3858; five family homes force-pushed with consolidation comments (#3922 a798a5773, #3787 87d032b9f, #3950 e5eadfa19, #3946 f3ccc1b27, #3814 d582a6a1f) and their eleven source PRs closed; #3842/#3921 closed for now into the #3811 hold; #3791 (6ea2a9531) and #3793 (5230ec1d2) pushed with replies; twelve non-moving bodies rewritten; eight PRs parked as drafts (#3811 #3814 #3844 #3855 #3858 #3910 #3923 #3945); #3923 rebased to 7ae4073de. 33 open -> 20 (12 ready). Nothing else is due before 2026-09-27.

What each row still owes a maintainer, or `-` when nothing is pending, so a new session can pick any PR up from here without re-reading its thread. Dates are trycua/cua review or comment dates; "RFC" is the ActionResult vocabulary RFC (drafted, number assigned at filing). PR-level actions (fold, hold, close, slot order) are in the OMP skill reference `cua-upstream/references/consolidation-20260920.md`.
### will/omp

| Row | Maintainer expects |
|---|---|
| P1 | #3797 (f-trycua 09-13): uuid vs native_id vs primary rule, key-pinning contract test; answered 09-14 |
| P2 | RFC #3796 (f-trycua 09-13): cooperative stop separate from abandonment; slice 1 core + transports |
| P3 | #3796: drop cancel_operation from slice 1; notifications/cancelled and C ABI cancel only |
| P4 | #3795: maintainer Lume rerun of the document-state row at ba23b4419 (asked 09-13T13:25) |
| P5 | #3793: re-review at 0b26b62cf; two-window fixture if the reviewer wants the refusal native |
| P6 | #3373 first; then a direct ActionExecutionRecord, no from_legacy branch (RFC 3473) |
| P7 | #2874: the single-transport regression the maintainer asked for; no PR thread yet |
| P8 | #3796: platform loops in a later slice with limitations stated |
| P17 | RFC 3931 (#3934): validated capture geometry is a capture_id prerequisite; file after #3942 |
| P18 | #3787 unreviewed; rebase drops the cache.rs helper hunk (a8a7b1e5e); dedupe launch half with #3456 |
| P9 | #3796: bounded start after slice 1; Linux/Windows never advertised as full cancellation |
| P16 | - |
| P10 | as P4 |
| P11 | as P5 |
| P12 | #3781: re-review at b26880250; seam, z-order removal and native proof answered 09-13 |
| P13 | #3783 approved at a0dc87e15 (09-13T15:27); merge pending; nothing owed |
| P14 | #3785 approved at 42052d0e9 (09-13T15:00); merge pending; W5-16 follows after merge. The fork carries #3785's approved cell rewrite since cfbadaf1a (`…ignores_competing_application_window`); a8636718a's per-display scoping is PR-branch only |
| P15 | as P6 |
| L1 | #3791 (f-trycua 09-13): ladder must match the tools; after #3864 advertise its two refusals |
| L2 | - |
| L3 | RFC 3931 calls screenshot_frame_valid non-portable; folded into #3814, expect pushback |

### Bench fixes B1-B9

| Row | Maintainer expects |
|---|---|
| B1 | - (closing #3816 into #3858) |
| B2 | - |
| B3 | - (folds into #3946) |
| B4 | #3811 (injaneity 09-16): coordinate type_text.rs with #3897; hold until it lands |
| B5 | - (folds into #3922) |
| B6 | - (`description` blocked by manifest generator, #3802; folds into #3787) |
| B7 | - (folds into #3787) |
| B8 | - (#3864 conflict: test nodes need identity: None) |
| B9 | #3836 (injaneity 09-15): re-review at c2e6298df, rebased dea964ccb; P2 fallback fix answered 07:03 |

### Wave 4 B10-B31

| Row | Maintainer expects |
|---|---|
| B10 | hold: #3897 returns TypedProgress; re-express with #3811 |
| B11 | as P6 |
| B12 | - (retitle fix(cua-driver): before the next push) |
| B13 | - (chord PR after T11) |
| B14 | - (folds into #3922) |
| B15 | CONTRIBUTING: public ActionResult change needs the RFC first; closing #3857 into #3858 |
| B16 | RFC before the committed verdict lands; the macOS typed write path can go first |
| B17 | as B9 |
| B18 | - (with P16) |
| B19 | - (folds into #3787) |
| B20 | as P6 |
| B21 | - |
| B22 | hold: #3897 replaces the classifier; re-express with #3811 |
| B23 | RFC: element_disabled code and escalation are vocabulary; stacked on #3910 |
| B24 | as P6 |
| B25 | - (folds into #3950) |
| B26 | - |
| B27 | detect_window_change is an input contract change: Refs the RFC; #3373 adjacent |
| B28 | - (folds into #3946) |
| B29 | - (stacked on #3781) |
| B30 | as P6 |
| B31 | - |

### Wave 5

| Row | Maintainer expects |
|---|---|
| W5-1 | #3897: re-express on focused_element_in_window plus a retained target |
| W5-2 | RFC first (public contract); additive `signal`, kind name kept (done at b76016c77) |
| W5-3 | RFC first; upstream spells `escalation.recommended` (#3888), reader accepts both |
| W5-4 | RFC (docs half) |
| W5-5 | - |
| W5-6 | #3897 wording "observe the target before retrying"; file with the re-expression |
| W5-7 | - |
| W5-8 | RFC for the projection; no new from_legacy branches (RFC 3473) |
| W5-9 | - (with #3950) |
| W5-10 | #3897: before == after is Unchanged; do not file Normalized |
| W5-11 | - (fork-only) |
| W5-12 | do not file: #3897 makes trusted partials retryable:false |
| W5-13 | RFC for element_disabled and escalation; mirror #3888's Windows shape |
| W5-14 | with #3858 (RFC-gated) |
| W5-15 | - (chord PR after T11) |
| W5-16 | after #3785 merges (approved 09-13) |
| W5-17 | - (T11 replaces the mechanism) |
| W5-18 | - (chord PR) |
| W5-19 | RFC: element_no_longer_exists and escalation.target are vocabulary |
| W5-20 | - (chord PR) |
| W5-21 | ProvenAppMenu arm goes into #3793's next push (focused-window guard asked 09-13) |
| W5-22 | new native cases wired into the canonical allowlist in the same commit |
| W5-23 | - (chord PR) |
| W5-24 | re-expressed on #3616's RetainedElement (cd0861727); file as the crash fix, first free slot |
| W5-25 | - (with W5-13) |
| W5-26 | - (with W5-13) |
| W5-27 | - (with W5-13) |
| W5-28 | - (fork-only) |

### Sync and wave 6

| Row | Maintainer expects |
|---|---|
| Sync c5550997b | - |
| W6 menu-command dispatch | - (chord PR after T11; #3855 kept as its prerequisite) |
| W6 RetainedElement refactor | - (RFC 3473 slice 1 shape; nothing owed) |
| W6 direct click/set_value records | aligned with RFC 3473 slice 2; from_legacy branches shrink per family |
| W6 contract 0.10 additive | RFC before the vocabulary lands; additive over 0.8 is what RFC 3473 asks |
| W6 harness recipe + allowlist | exact-head evidence through the installed daemon; native cases in run-rust-e2e.sh |
| W6 fixture stamp + recordings root | - (recipe; a PR's evidence names the fixture build with the binary sha) |
| W6 competing-window cell | - (P14's approved cell, carried on the fork) |

## Wave 9

| Patch (commit title) | What the runs showed | Upstream PR? |
|---|---|---|
| `feat(macos): place the caret by content before type_text dispatches` (bbbb4e132) | Step-diff sink S3 (+14 steps over four native-act-notes runs): appending a line to a multi-line `AXTextArea` had no route — `cmd+End` is dropped in background delivery (4 cells), `computer.help()` shows no selection API (3), `type()` lands at the caret's resting position after a click, the start of the body (3: `0920-A/omp-2:33` `"Follow-up: owner assignedMeeting 047\n…"`), screenshots to find the caret (2), the "write it another way" repair (2). Codex does it in one cell with `selectText(id, text, {selectionType: "cursor_after"})`. `type_text` gains a platform-local `caret` argument (`"start"`, `"end"`, `{"after": s}`, `{"before": s}`; element-addressed calls only): AXValue is read, `AXSelectedTextRange` collapsed at the UTF-16 offset, read back, then the existing rungs run unchanged; the reply carries `caret_index` and `caret_anchor`. Absent anchor / unreadable value / range not taken / no element or terminal pid are typed refusals (`caret_anchor_not_found` with the searched text and the value's UTF-16 length, `caret_value_unreadable`, `caret_not_placed` with the observed range, `caret_unsupported`) that dispatch nothing. No `caret` = prior behaviour byte for byte. Unit-tested (offset arithmetic over ASCII / BMP / non-BMP / repeated / absent / empty, argument forms, refusal and evidence shapes); not exercised against a live text area — the desktop was reserved, Main runs the harness. | - (fork-only until the base implementation is dialed in) |
| `feat(macos): label a list row or cell that names nothing by its descendant text` (0c2194bed) | Stepdiff S11 (+3 steps on every native-act-notes run): after the search settles every note-list row reaches the model as `AXRow ""` > `AXCell ""` > `AXCell "ICMNoteListCell"` and it screenshots to tell which hit is which, while Codex's tree shows `text …warehouse pallet audit 5:10 AM` under the row. Cause: `elements[]` carries actionable nodes only and OMP renders from it; Notes' title, snippet and folder are non-actionable `AXStaticText` beneath the cell, and the row/cell own no title/value, so `label` fell to the identifier. The walker now fills `AXNode.descendant_text` after the walk for `AXRow`/`AXCell`/`AXMenuItem` nodes with no title/value/description/placeholder of their own (text-role descendants in DFS order, single-spaced, 120 chars + `…`), and `label` takes it ahead of the identifier. No new rows, no re-indexing, markdown row untouched, wire shape unchanged. Read-only walk of the running Contacts window: 46/46 indexed rows/cells gain text (`[5] AXRow -> "Google"` where the 0919 tree showed `AXRow ""`). Live Notes not re-run (desktop reserved). | not yet (fork-only until the base is dialed in) |
| `feat(macos): read a display's desktop surface as its own window` | StepDiff sink S1 (+35 steps): Codex verifies a saved file by reading Finder's desktop in one `getApp` call (`mixed-web-pdf/codex-1:48` `Window: "Desktop" … 2 image CleanShot …`); OMP had no route to the desktop at all. Measured against the live driver: the desktop icon window (CGWindow 9814, `kCGDesktopIconWindowLevel`, owned by Finder) was never in `list_windows` (layer-0 + `kCGWindowListExcludeDesktopElements`), `get_window_state` refused it as `window_id_not_found` (the any-layer identity lookup carried the same exclusion), and past identity no AXWindow claims it (Finder exposes the desktop as an `AXScrollArea "desktop"` child of the application element) → `ax_window_unresolved`. The bench's `[16026] "MacBook Pro"` was a blank phantom Finder window with no AXWindow; its empty tree was correct and is unchanged. Now: identity lookups enumerate every layer without the desktop exclusion; `list_windows` adds the desktop surfaces as `kind: "desktop"` rows from the same enumeration as the layer-0 rows; when no AXWindow claims an id that WindowServer files at the desktop icon level, `decide_desktop_surface_scope` walks exactly the application-level non-window, non-menu-bar children whose AX frame lies inside the window's bounds (1 pt slack), plus the menu bar, and the reply carries `tree_scope: {code: "desktop_surface", content_children, reason}`; nothing qualifying keeps `ax_window_unresolved`. Layer-0 same-pid windows without an AXWindow are untouched (12523/12517 re-checked: still empty). Background input on the desktop stays refused by the unchanged core gate (`ax_window_present` is still "an AXWindow claims it"); observation is the deliverable. Consumer follow-up in OMP: read the driver's `kind` so `computer.window({app:"Finder"})` can name/offer the desktop row (today it prints `[9814] ""`). | after the AppKit harness pass; candidate for a small PR (the exclusion flag on identity lookups is a latent #2237-family bug on its own) |

## Wave 10 (2026-09-21, from the 20260921-full leg diagnoses; branch will/bench-fixes)

Harness cases for every row are authored and compile; they run at the idle-gated verification pass (fixture rebuilt first), see `local://w10/idle-gated.md` in the session.

| Patch (commit title) | What the runs showed | Upstream PR? |
|---|---|---|
| `fix(macos): a surface the app hosts inside the target window is the target's, not a rival` (8c94a0bd6) | Finder's inline rename editor is a separate layer-0 CGWindow drawn inside the renamed window — and, measured live 2026-09-21, it is NOT "non-AX-mapped" as wave-10's brief and the finder-sort diagnosis assumed: it is in the application's own `AXWindows`, and `CFEqual` says that entry IS the focused `AXTextField` (role neither AXWindow nor AXSheet), mapped by `_AXUIElementGetWindow`. So it counted as a competing keyboard destination — the driver's own refusal said "one of 3 windows in its application" (target + a second document window + the editor) and refused both the background `type` and the background `Return` aimed at the very field the keyboard was in. While it is up, `AXFocusedWindow` on the app element is UNREADABLE (so `focused_window_id_of_pid` = None and `preserves_exact_existing_focus` was false, sending the chord/type rungs into `make_exact_window_key` on the parent), and the focused element carries no `AXWindow` and no `AXTopLevelUIElement` at all, its `AXParent` being the application — which is why its ancestry read `Unproven` and the exact write was refused `element_outside_target_window`. A per-pid keystroke with no activation landed in it, and the surface exists only while its application is frontmost, so activation can only take the keyboard away from where it already is. The driver now classifies such a surface by what accessibility says (published as a control, not as a window or sheet; frame inside the requested window only to pick which sibling owns it), excludes it from the two-window guard, treats an element in it as inside the requested window, skips activation in all three foreground rungs and posts the key PID-routed. Sheets keep their `AXSheet` role, their key-window status and their refusal, code and sentence unchanged. **Re-measured live 2026-09-21 14:40 (idle desktop, installed build 4dea9457 via `mcp --direct`):** background `type`/`Return` at the parent now count **1** other window (the user's real second Finder window), not 3; fg `Return` dismisses the editor (was a no-op); fg `cmd+a` and fg `type` land in the editor and it survives (both used to close it). Rung table in `research/wave10/idle-gated.md` § D1. | Upstream PR? Yes — the ownership rule and the three rung guards are app-independent macOS behaviour; the fixture's `child_editor` scenario carries the shape. |
| `feat(macos): count an opened menu as delivery, and report the rect a capture actually covers` (635b234f3) | Both calendar-recur runs pressed the alarm `AXPopUpButton`, the menu opened (omp-1 L69 / omp-2 L63 screenshots), and the driver reported `⚠️ Unverified … nothing changed … suspected_noop` (L66 / L60): the menu is an accessory CGWindow outside the target window's AX subtree, so element state, app focus and the window digest were all identical. Its own advice then sent both runs to a pixel click on a frame labelled `935×598 (1 px = 1 window point)` whose content was the window+popover(+menu) union squeezed to ~0.77× (L78 shows the 935-pt window drawn ~720 px wide; windows list at L81 gives a 1171×776 union) — omp-1's (643,491) closed the menu, omp-2's (765,472) closed the popover. Costs +14 steps in omp-1 and +9 in omp-2. The probe now samples the pid's on-screen accessory windows and publishes a gained one as the additive `menu_opened` signal (one-way: a dismissed menu is not a reaction, and an unreadable probe stays `Unusable`, never `suspected_noop`); the window-scoped walk admits an open menu the descent cannot reach; and the capture is sized to the rect it covers and publishes it as `screenshot_content_bounds`, which OMP labels the grid from. | Upstream PR? Yes — the probe signal, the menu admission and the capture-rect report are all app-agnostic macOS facts. |
| `fix(macos): judge a set_value commit by what the control reads back, and follow a control its app re-creates` (f4f5fc5b9) | 20260921-full: 3 `not_committed` verdicts on writes the graders PASSed — native-act-contacts omp-1 L18 and omp-2 L45 (phone reformatted to `(555) 789-0123` in the same reply's tree, line 39), omp-2 L54 (Job Title, editor moved on Tab); mixed-web-contact omp-1 L21 vs L24 (`value="Apple Park Visitor Center"` present after the verdict said the app kept its own value) and omp-2 L21/L54, and 20260919/omp-2 PASSed with the same three verdicts. Cause: an exact read-back on the pointer Contacts re-created at end-of-edit, with every non-match reported as "the app still holds its own value". Now `NotCommitted` requires the read-back to equal the pre-write value; a rewritten value is `unproven` with the value quoted, an unreadable pointer is re-resolved by identity (parent+role+label+ordinal) and only then `unproven`. No value normalisation — digits, whitespace and bidi isolates stay significant. | fold into #3858 (B16/W5-14's home) |
| `fix(macos): select a bound field's whole value in UTF-16 units before retyping it` (c048fdd61) | No run evidence — found while reading the retype route: the select-all length was `chars().count()` while `AXSelectedTextRange` is UTF-16 units (type_text.rs:920-926 documents this and the caret path counts correctly), so retyping over `😀AB` left `zedB`. | yes — standalone Unicode-correctness fix, or fold into #3858 with the row above |
| `fix(macos): focus the target before an AX text insert, and stop calling an unfocusable target a retryable partial` (1e750e67b) | `type()` wrote `AXSelectedText` with no focus write: Contacts' search field read back as written while the app never ran the query (native-act-contacts omp-2 L21, 5 excursion cells), and Maps answered 0 of 24 (native-read-maps-walk omp-1 L12). At a Finder row name cell the same 0-delivered reply carried `retryable:true` + a remainder and bought two identical retries (native-act-finder-sort omp-1 L66→L70→L72, 2 cells). The `SemanticOnly` refusal returned the gate's "address the field itself" to a caller that had done exactly that (native-act-login-item omp-1 L25→L27, 1 cell per run). `launch({name:"Finder"})` answered `APP_NOT_INSTALLED` because the system Finder is filed outside every scanned root (native-act-finder-sort omp-1 L24). | Upstream PR? yes — driver-only, contract-additive (`target_focused`, `retryable:false`), no OMP dependency |
| `fix(macos): read the enabled flag after the activation, and publish modal dialogs and date values` (3e2d0ad4a) | Both Notes runs refused a toolbar search field with `element_disabled` under `delivery:"foreground"` (omp-1 L16/L18) while the identical call succeeded once the read landed later (omp-2 L18) — the AX-only `await_window_focused` predicate was already true before activation, so the wait never engaged and the `AXEnabled` read raced AppKit's key-window install; the same staleness hotkey.rs:280-287 measured at ~170 ms on Notes. Calendar's repeat sheet rendered `AXDateTimeArea` with no value and no settable marker against Codex's `142 date time area (settable, date) 9/25/26` (omp-1 #15-16), and the app-modal recurrence alert never appeared in four observations of the window it blocked (omp-1 L102/L105/L108/L111) — announced only as a gained window. Now: the foreground wait gates on WindowServer's front-process transition too, a disabled control is re-read on a 10 ms tick inside the activation budget, an unlanded activation says so instead of blaming the application, CFDate values render as local ISO-8601 with a `settable` flag for the value-control family, and a same-pid AXModal dialog is walked into the blocked window's tree with `modal_windows` + roster `kind: "app-modal"`. | Upstream PR? yes — all five pieces are app-agnostic macOS/AppKit mechanisms with fixture tests; the `value_settable` field and `modal_windows` key are additive. |
| `fix(macos): raise the exact window the foreground rung made key, and address a modal dialog from the window it blocks` (b8c833f09) | First real-desktop run of the wave-10 tree (2026-09-21, runs r1–r5 in `~/tmp/cua/harness-out-w10*`): 50 cases, 40 → 48 green, the 2 left being the documented direct-mode limits (agent-cursor overlay, pending-snapshot cell). What the desktop refuted: (1) `with_foreground_assist` posted `set_front` + the make-key records and stopped — enough to activate an *inactive* application onto its remembered key window (WhatsApp, Notes), but with the application already frontmost and a sibling window key the target never became key in the whole 400 ms wait, so the key-gated toolbar field stayed `AXEnabled=false` (`key_gated_toolbar` + `second_key_window` fixture, measured before and after). The rung now ends the way `bring_to_front`'s exact path always did, with `AXRaise`/`AXMain`/`AXFocused` on the exact AX window (`raise_exact_ax_window`, moved to `ax/bindings.rs` and shared). (2) `list_windows` labelled `kind: "app-modal"` on a row that could not exist: `NSAlert.runModal` runs at the modal-panel level and the roster's layer filter admits layer 0 only, so the one window the roster most needed to name was absent — a roster-named modal id the filter dropped is now admitted from the any-layer lookup, front of the process's rows. (3) The blocked window's observation carried the dialog's rows but a press on one was refused `element_outside_target_window`: new `ElementAncestry::ProvenAppModal` (core, additive) with the standing of an attached sheet — semantic AX exactly addressed, window-aimed routes refused naming the dialog; `AxWindowRecord.modal` carries the app's `AXModal` into `exact_target`. (4) `ObscuringWindow.modal` + `holds_keyboard_through_raise()`: only a non-focusable or modal window in front decides `OwnedPanelInFront`; an ordinary sibling in front leaves the foreground rung open. Fixture facts the harness forced: an inline editor must publish itself as the application's `AXFocusedUIElement` (it resolved to nothing, active or not, until the window answered it — and `NSWindow` drops `setAccessibilityValue:` unless `isAccessibilitySelectorAllowed` admits it); a popup button's `AXPress` orders its window to the front of its layer without activating the process (measured against a covering TextEdit window), so `press_opens_menu` holds the sentinel's contract (no activation, no focus change, no cursor motion), not target occlusion; the sheet's title must not be a superstring of the harness window's; the modal alert must be deferred ≥ 0.4 s past the press or the AXPress reply times out (-25204) behind `runModal`'s nested loop. | Upstream PR? yes — (1)–(4) are app-agnostic macOS mechanisms with fixture cases; `ProvenAppModal` is an additive enum variant; fixture/test changes travel with their cells. |
