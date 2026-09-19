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
- Every field the stack adds to a tool result rides through upstream's
  0.28.0 result boundary (`conforming_tool_result`) unchanged: the window
  snapshot schema is open, and typed refusals (`cancelled`, `capture_timeout`,
  `foreground_unavailable`) already carry a `code`, which the boundary keeps.
- Nothing here is OMP-specific behaviour; `cancel_operation`, `_call_id`, and
  `notifications/cancelled` are generic MCP.

| # | Patch (commit title) | Why upstream lacks it | Related upstream issue / PR | Upstream PR? |
|---|---|---|---|---|
| P1 | `feat(cua-driver-contract): report display identity and screen origin` | `get_screen_size` / `get_desktop_state` publish only width/height/scale, so two equal-sized displays are indistinguishable and desktop consent grants cannot be bound to a source. Fields are optional; Linux already computes the identity internally (`platform-linux/wayland/hyprland.rs`) but does not publish it. | none found | yes |
| P2 | `feat(cua-driver-core): cooperative operation cancellation and owned native cleanup` | Upstream has no operation-scoped cancellation: a caller that disappears cannot stop a long native input sequence, and runtime shutdown cannot drain native resources it does not own. Maintainers deferred a "general cancelled-blocking-worker drain redesign" (#3473). | #3473 (deferral), #2002 (idle CPU / stuck capture after session) | yes, as an RFC-sized draft framed against #3473 |
| P3 | `feat(cua-driver): honor notifications/cancelled and cancel by call id over the C ABI` | MCP `notifications/cancelled` is spec-level and ignored upstream; there is no way to stop one in-flight call. Adds transport-scoped call ids, the `cancel_operation` fallback tool, `cua_driver_invoke_call_v1` / `cua_driver_cancel_call_v1` (additive C ABI), and concurrent dispatch on the direct stdio path so a cancel can be read mid-call. Shutdown only interrupts pacing; a call that still completes under it keeps its own result (upstream's drain contract). | none found (spec gap) | yes |
| P4 | `feat(cua-driver-core): attach document path and dirty bit to window snapshots` | `get_window_state` has no notion of the document a window edits or whether it has unsaved changes; absence must stay absent rather than default to "clean". | none found | yes, together with P10 |
| P5 | `fix(cua-driver-core): admit semantic AX on app menu bars and attached sheets; name the owning window in refusals` | An app's own `AXMenuBar` and an `AXSheet` attached to the requested window have no ancestry to the requested CGWindowID by construction, so window-scoped semantic actions on them are refused as "outside the target window". | #3351; alternative design to draft PR #3353 (which walks a bounded ancestry chain to the parent window for every route) | yes, proposed on #3353's thread rather than as a competing PR |
| P6 | `fix(cua-driver-core): publish observed post-dispatch change as evidence without laundering it into confirmed` | Legacy structured output can carry a `window_change` observation after dispatch; the typed action record either dropped it or would let it promote the effect to `confirmed`. | PR #3373 (post-action rediscovery; typed `window_change`) — if it lands first, rebase onto its typed field | conditional on #3373 |
| P7 | `fix(macos): post each mouse event through exactly one transport` | Upstream posts window-scoped mouse events through SkyLight `SLEventPostToPid` *and* `CGEvent::post_to_pid`; a receiver accepting both sees down/down/up/up. Upstream's `MousePostMode` (#2907) only moved foreground clicks to public-only; the background path still dual-posts. | #2874 (maintainer asked for exactly this regression) | yes — strongest candidate |
| P8 | `feat(macos): cancellation-aware input and tool workers` | Depends on P2: input primitives prepare their release before the press and release on cancel/unwind; pacing wakes on cancellation; blocking tool workers carry the operation. | with P2 | with P2 (or follow-up) |
| P17 | `fix(macos): capture exact window pixels via SCScreenshotManager; never fall back to the inset legacy API` | On macOS 26 `SCScreenshotManager::capture_image` returns an inset frame; every pixel coordinate the agent reads is then off. Uses `capture_screenshot` (macos_26_0 API, `screencapturekit` 8.0.1) with shadow/cursor excluded and validates geometry before bytes escape; a modern failure stays a failure. | none found (SCK dependency bump landed upstream as #3680) | yes |
| P18 | `fix(macos): exact launch identity, list_windows AX metadata, lossless AX values, file-item actions` | Several exact-targeting gaps: launch by name could match the wrong bundle; `list_windows` lacked AX metadata; empty/whitespace `AXValue` was replaced by the placeholder hint and raw newlines could fabricate tree rows; hosted Open/Save panels and sheets could not be addressed as part of the exact window; file items hid their open/confirm actions. | PR #3456 (accessory-app running state) overlaps the launch-identity part | lossless-value part yes; launch-identity part maybe (dedupe against #3456) |
| P9 | `feat(macos): session-owned rendering lease with bounded capture start (capture_timeout)` | Covered Chromium renderers stop painting without a display stream; upstream has no session-owned lease and `SCStream.start_capture` can hang forever (`cua-sck-window-capture` stuck in `SyncCompletion`, #2002). Adds a bounded start with a typed `capture_timeout` and drains the lease through P2's owned runtime resources. | #2002 | yes |
| P16 | `fix(macos): thread-scoped AX walk budget with deadline-aware bindings` | Upstream bounds each native AX request with a per-element messaging timeout, so a wedged app can extend an observation element by element. One thread-scoped deadline across the walk, deadline-aware binding wrappers, partial-state reporting (`ax_walk_timed_out`, `stop_reason`), and a refusal instead of substituting another window's controls. | #1537, draft PR #1755 (wall-clock guard this supersedes) | yes |
| P10 | `feat(macos): read AXDocument/AXEdited for the requested window` | macOS half of P4: `AXDocument` and `AXEdited` (window, then close button). | none found | with P4 |
| P11 | `fix(macos): prove sheet and menu-bar ancestry in exact_target` | macOS half of P5: proves `ProvenAppMenu` / `ProvenAttachedSheet` ancestry and names the owning window in refusals. | #3351 / #3353 | with P5 |
| P12 | `fix(macos): resolve menu activation from WindowServer, not cached NSWorkspace state` | `invoke_menu` polls `NSWorkspace.frontmostApplication`, an AppKit cached value that never refreshes on the blocking menu path; upstream still reads it. Kept alongside upstream's main-queue affinity for the embedded self-pid case (`2dbc1c2c`). | none found | yes |
| P13 | `fix(macos): poll for the Chromium AX tree instead of a fixed 0.5 s sleep` | After `AXManualAccessibility` upstream sleeps 500 ms, too short on a loaded machine and wasted otherwise. Side effect to disclose: VS Code shows its screen-reader toast on first assertion. | #1756 | yes |
| P14 | `fix(macos): verify bring_to_front against the process it activated` | Upstream verifies by global z-order and false-negatives when another process owns the topmost window; refines the merged overlay exclusion (`44c9d1f6`, #3704 / #2829). | #2829, #3704 | yes |
| P15 | `feat(macos): probe delivery of background clicks` | A background click on a window that ignores synthetic input reported success because the events were posted; pre/post AX probe classifies delivery and escalates a no-op. | #3389 is the `type_text` sibling of the same class; PR #3373 overlaps the evidence rule (see P6) | conditional on #3373 |
| L1 | `feat(linux): report background_input routes on get_window_state` | Linux refuses background routes with the right predicates but never publishes the core `background_input` routes object the agent needs to choose a route up front. | none found | yes |
| L2 | `fix(linux): return a structured foreground_unavailable code on X11 foreground failure` | The X11 foreground path builds `anyhow!("foreground_unavailable: ...")`, which surfaces as untyped error text; the Hyprland path already emits the typed `{code, detail}` envelope. | none found | yes |
| L3 | `fix(linux): set screenshot_frame_valid on successful get_window_state` | Linux only ever reported `screenshot_frame_valid: false` on the refusal path and never `true` for a delivered per-window frame, which is by construction identity-validated on that path; macOS reports `true`. | none found | yes |

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

# Bench fixes carried on `will/bench-fixes`

`will/bench-fixes` is `will/omp` at `54a2437d8` plus the patches below, in
stack order, one commit per row. Every one comes from a measured agent run
(`trycua-omp/research/bench-set-20260911/`), so the "what the runs showed"
column cites the transcripts rather than a design opinion. The rules above
apply unchanged: generated files are regenerated, and each added result field
rides through the 0.28.0 `conforming_tool_result` boundary.

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

## Wave 6

| Patch (commit title) | What the runs showed | Upstream PR? |
|---|---|---|
| `feat(macos): dispatch a not-key window's disabled key equivalent as its menu command` | Notes keeps `Edit > Find > Note List Search…` (⌥⌘F) disabled until its window is key; a background chord there landed 0/12 and the model never took the foreground rung it was offered (0/7, T11 on 0919b). Measured live: `invoke_menu` fronts Notes, makes the window key, presses and restores ~350 ms later — before Notes runs the command, so the search field never held focus through it either (0/2); with the window kept key it does (3/3) and it releases the moment the prior app is re-fronted (3/3). The chord now resolves to the item by `AXMenuItemCmdChar`/`CmdVirtualKey`/`CmdModifiers`, is dispatched as that item with the window key, the app's reaction is waited for before the restore, and the reply says what moved: `route: menu_command`, `delivery: foreground`, `menu_path`, and whether the reaction survived the restore (`escalation.target: element` when it did not). Contract 0.10.0: `ActionRoute::MenuCommand` and the optional `ActionResult.menu_path`. | pending |
