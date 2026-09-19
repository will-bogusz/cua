# Action results and postcondition verification

Cua Driver 0.15 separates two facts that earlier releases mixed together:

- `ActionResult` says what route the driver used and how strongly it can
  account for the action itself.
- `VerifyStateOutput` says whether a caller-defined postcondition is
  `satisfied`, `unsatisfied`, or `unknown`.

The driver reports these facts. The agent harness owns task meaning, visual
reading, stop/retry decisions, and movement through the action ladder.

## MCP action result

Every successful action returns a closed `structuredContent` object:

```json
{
  "effect": "confirmed",
  "route": "accessibility",
  "delivery": {"mode": "background"},
  "evidence": [{"kind": "value_readback"}]
}
```

An action that only proved the target reacted publishes the `window_change`
kind plus the signal the platform watched — the kind keeps its 0.8 name, and
a row without `signal` is the 0.8 window poll:

```json
{
  "effect": "unverifiable",
  "route": "synthetic_events",
  "delivery": {"mode": "background"},
  "evidence": [{"kind": "window_change", "signal": "app_focus"}]
}
```

`effect` and `route` are required.

| Field | Values |
| --- | --- |
| `effect` | `confirmed`, `partial`, `unverifiable`, `suspected_noop`, `refused` |
| `route` | `accessibility`, `synthetic_events`, `global_input`, `system_api`, `dom`, `trusted_input`, `menu_command` |
| `delivery.mode` | `background`, `foreground`, `not_applicable`, `unknown` |
| `evidence[].kind` | `value_readback`, `window_change` |
| `evidence[].signal` | `element_state`, `app_focus`, `window_tree`, `window_change` — optional on `window_change` rows; absent when the producer named no signal (the 0.8 window poll), or named one this version does not publish |
| `escalation.target` | `pixel`, `foreground`, `page`, `session`, `element`, `snapshot` |
| `escalation.reason` | `route_unavailable`, `delivery_failed`, `effect_unconfirmed`, `suspected_noop`, `permission_required` |
| `committed` | value-setting actions only (`set_value`, `type_text`): `committed`, `not_committed`, `unproven` — what the driver observed of the application's own end-of-edit, beside `effect`; absent on every other action |
| `menu_path` | `menu_command` routes only: the application's own menu titles the driver dispatched, top level first (`["Edit", "Find", "Find…"]`); absent on every other route |

The action-result tools are:

`click`, `double_click`, `right_click`, `scroll`, `drag`, `mouse_drag`,
`parallel_mouse_drag`, `move_cursor`, `mouse_button_down`, `mouse_button_up`,
`type_text`, `type_text_chars`, `press_key`, `hotkey`, `set_value`,
`set_window_frame`, `invoke_menu`, `browser_click`, `browser_pointer`, and
`browser_type`.

Other mutating tools such as application launch, window activation, browser
navigation, dialogs, uploads, and downloads retain their own typed results.

The contract is deliberately closed. It does not echo selectors, coordinates,
scope, targets, platform transport names, diagnostic pointers, or the old
`verified` boolean.

The invariants are:

- `confirmed` has publishable readback or window-change evidence;
- `partial` has `delivery.delivered_count`;
- `refused` has neither delivery nor evidence;
- `menu_command` is never a background delivery: the driver made the target
  window key (and fronted its application when it was not frontmost) before
  pressing the item, then restored the prior frontmost. `invoke_menu` reports
  it always; `hotkey` reports it when a chord posted at a window that is not
  key names the key equivalent of a menu item the application keeps disabled
  until that window is key, and the chord was dispatched as that item instead.

## Window target resolution

Window-scoped actions accept a PID without `window_id` only when that process
has exactly one eligible top-level window. The driver promotes that unique
match to an exact `(pid, window_id)` target before dispatch. If the PID owns
multiple eligible windows, the action fails before sending input with
`code: "ambiguous_window_target"`, `effect: "refused"`, and a `candidates`
array containing each candidate's `window_id`, title, application name when
available, and on-screen state. Call `list_windows({pid})`, select the intended
candidate, and retry with its explicit `window_id`.

A PID with no eligible windows fails with `code: "window_target_not_found"`.
Explicit `window_id` targets and `element_token` targets retain their exact
resolution semantics; the guard does not replace or reinterpret them.

An action that reached an actuator but lacks a trusted readback is
`unverifiable`, not `confirmed`. Screenshot change, native API acceptance,
event receipt, and operator observation may remain useful internal diagnostics,
but they do not independently justify `confirmed`.

## Structured refusals are a separate channel

A refusal that stops before any actuator runs is an MCP error payload, not an
`ActionResult`. It carries a machine-readable `code`, a route-free `reason`,
and the facts the decision actually read. The stable codes are defined once in
`cua_driver_core::background_input::refusal_codes`:

| Code | Meaning |
| --- | --- |
| `window_not_found` | the requested window does not exist |
| `owner_pid_mismatch` | the window is not owned by the requested process |
| `off_space_or_ax_unresolved` | the window is off-space or its accessibility peer could not be resolved |
| `minimized_or_hidden_window` | the window cannot receive input in its current state |
| `same_pid_keyboard_ambiguity` | the process owns more than one candidate key window |
| `element_outside_target_window` | the addressed element could not be proven to belong to the requested window |
| `element_no_longer_exists` | the addressed element's accessibility reference is invalid; the window itself is unchanged |
| `element_disabled` | the application reports `enabled = false` on the target, so no delivery mode and no activation can act on it |

`element_no_longer_exists` escalates with `target: "snapshot"`: only a fresh
observation can produce an addressable element. `element_disabled` carries no
escalation — the application disabled the control, and no rung of the ladder
changes that. Its payload names the control and the state the decision read:
`action`, `role`, `label`, `window_id`, `pid`, `foreground`,
`front_in_process`, and `obscured_by` when another window of the same process
is in front.

These payloads may also carry `effect: "not_dispatched"`. That value is
deliberately not a member of the closed `ActionEffect` enum: an `ActionResult`
is only produced once an actuator ran, so nothing that reaches the typed
contract can be `not_dispatched`.

## Verification remains separate

After an action, use `verify_state` for a bounded structured postcondition.
`satisfied` is the only successful terminal status. `unsatisfied` can justify a
retry or another ladder route. `unknown` means the available observation could
not prove either answer and must never be promoted to success.

When `include_screenshot` is enabled, the screenshot is uninterpreted evidence.
A multimodal harness reads it and decides whether to stop, retry, or advance.

## SDK access

Rust, Python, and TypeScript keep the transport-neutral `ToolResult` envelope:
text, images, structured JSON, error state/code, degraded state, and raw JSON.
The ambiguous `verified` field is removed.

Successful action calls expose the typed value at `result.action`; successful
`verify_state` calls expose it at `result.verification`. In Rust the equivalent
borrow accessors are `result.action()` and `result.verification()`.

```python
result = await driver.click(click_input)
if result.action.effect is ActionEffect.CONFIRMED:
    verification = await driver.verify_state(expectation)
    if verification.verification.status is VerificationStatus.SATISFIED:
        return "done"
```

```ts
const result = await driver.click(input)
if (result.action?.effect === ActionEffect.Confirmed) {
  const checked = await driver.verifyState(expectation)
  if (checked.verification?.status === VerificationStatus.Satisfied) {
    return "done"
  }
}
```

## Escalation belongs to the harness

An optional escalation is advice, not an automatic retry:

| Target | Harness action |
| --- | --- |
| `pixel` | refresh visual state and choose an exact pixel target |
| `foreground` | explicitly select foreground delivery when session policy permits |
| `page` | bind the native window to a supported browser page route |
| `session` | prepare or explicitly widen the session only when policy permits |
| `element` | re-address the exact control: set its value, or act on the element instead of typing at whatever holds focus |
| `snapshot` | re-observe before acting again; the addressed state is no longer trustworthy |

SDK integrators, OpenClaw, Hermes, and other agent hosts can implement different
policies above this same narrow fact contract without duplicating platform
actuator details.

A driver never names the harness's own observation or activation tool in
prose. It emits one of these targets and the harness renders the route it
actually exposes.

## Migration from 0.14

- Replace `result.verified` checks with `result.action.effect` for action facts.
- Use `result.verification.status` only for `verify_state` postconditions.
- Replace imports of the removed `ClickOutput`, `DesktopActionOutput`, and
  `MoveCursorOutput` types with `ActionResult`.
- Do not read coordinates, `scope`, `path`, `transport`, or request targets from
  an action response; retain request context in the caller if it is needed.
- `move_cursor` no longer echoes `x`/`y`; call `get_cursor_position` when the
  observed pointer location is needed.
- Treat `unverifiable` as unknown action effect, not failure and not success.
- Treat MCP `isError` as transport/tool failure; inspect a successful
  `ActionResult` separately.
- Upgrade daemon and SDK together. A 0.15 SDK intentionally rejects legacy
  0.14 action payloads instead of guessing at their meaning.
