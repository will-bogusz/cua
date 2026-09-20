# OMP AppKit harness recipe

`harness.sh` runs `harness_appkit_test` at HEAD through the driver that OMP installed at
`~/.omp/natives/cua-driver/` and renders exact-head evidence for a PR `<details>` block.

Why it exists: on a host where no cua-driver app identity holds Screen Recording, a `serve`
daemon reports every window title as `""` and the harness aborts at `find_window` before a
single assertion runs. The same binary in `mcp --direct` mode, as a child of the granted
terminal session, does see titles. `direct-shim.sh` rewrites the testkit's
`mcp --socket <daemon>` into `mcp --direct`; a listening-only Unix socket satisfies the
testkit's reachability probe; nothing is ever spoken on it and no daemon is started.

## Subcommands

| command | effect |
|---|---|
| `preflight` | PASS/FAIL per check (no driver/harness process, no stale socket, installed binary matches its manifest and is signed `OMP Computer Use`, terminal session holds AX + capture, both fixtures built from HEAD sources, private `CARGO_TARGET_DIR`); exit 1 on any FAIL |
| `fixture` | builds `CuaTestHarness.AppKit.app` and the Electron sentinel `CuaTestHarness.Electron.app` (background cells launch it) via `tests/fixtures/build/macos.sh --only appkit` / `--only electron`, then writes the sha256 of each fixture's sources to `<bundle>/Contents/Resources/sources.sha256` |
| `build` | `cargo build --locked --release -p cua-driver`, stages a copy, codesigns it (`OMP Computer Use` / `com.ohmypi.cua-driver`), records head + sha256 in `build.json`; refuses a dirty `libs/cua-driver/rust` unless `--allow-dirty` |
| `install` | rm-then-copy the staged build into the installed location, keeps the previous binary and manifest as `*.prev`, verifies the installed sha256 equals the built one, writes `manifest.json` |
| `run [filter]` | one `cargo test … --ignored --exact <test> --nocapture --test-threads=1` per matching `harness_appkit_*` case through the shim, with `CUA_E2E_RECORDINGS_ROOT=<out>/recordings` as the canonical `scripts/ci/macos/run-rust-e2e.sh` sets it (the snapshot-publication cell requires it; every cell then leaves its trajectory under `recordings/`); appends `{test,result,duration_s,head,binary_sha256,…}` rows to `evidence.jsonl`, logs under `logs/` |
| `evidence [file]` | markdown table: head, binary sha256, daemon identity, macOS, per-test result and duration |
| `restore` | puts `cua-driver.prev` (and `manifest.json.prev`) back, rm-then-copy |
| `allowlist-check` | every `fn harness_appkit_*` in `tests/harness_appkit_test.rs` must appear in the allowlist in `scripts/ci/macos/run-rust-e2e.sh` |

`--dry-run` prints the commands a mutating subcommand would run. Environment: `CARGO_TARGET_DIR`
(required, private per lane), `HARNESS_OUT_DIR` (default `~/tmp/cua/harness-out`),
`OMP_CUA_DRIVER_DIR` (default `~/.omp/natives/cua-driver`), `CODESIGN_IDENTITY`
(default `OMP Computer Use`).

A fixture bundle is current only when its `sources.sha256` stamp equals the sha256 of its sources
at HEAD (`tests/fixtures/apps/macos/appkit/*.swift`; for Electron `build.sh`, `main.js`,
`preload.js`, `package.json`, `package-lock.json` and `shared/web/index.html`). `preflight` fails
and `run` refuses otherwise: a bundle built before a fixture-side commit silently fails every
cell that relies on the new behaviour (a `find_window` for a window the old build never
creates reads as a driver regression). `fixture` is the fix.

## Live sequence

```sh
export CARGO_TARGET_DIR=~/tmp/cua/target-<lane>
h=libs/cua-driver/scripts/omp/harness.sh
$h preflight && $h fixture && $h build && $h install && $h run && $h evidence; $h restore
```

Never replace the installed binary in place while a driver process still maps it: macOS
SIGKILLs every later reader of that path. `install` and `restore` unlink first for that reason.
