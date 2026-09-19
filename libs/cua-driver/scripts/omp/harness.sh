#!/usr/bin/env bash
# OMP AppKit harness recipe: run harness_appkit_test at HEAD through the installed, TCC-granted
# cua-driver in `mcp --direct` mode and render exact-head evidence. See README.md beside this file.
# Usage: harness.sh <preflight|fixture|build|install|run [filter]|evidence [file]|restore|allowlist-check> [--dry-run] [--allow-dirty]
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
RUST_ROOT="$REPO_ROOT/libs/cua-driver/rust"
TEST_FILE="$RUST_ROOT/crates/cua-driver/tests/harness_appkit_test.rs"
E2E_SCRIPT="$REPO_ROOT/scripts/ci/macos/run-rust-e2e.sh"
FIXTURE_BUILD="$REPO_ROOT/libs/cua-driver/tests/fixtures/build/macos.sh"
FIXTURE_APP="$RUST_ROOT/test-apps/harness-appkit/CuaTestHarness.AppKit.app"
SHIM="$SCRIPT_DIR/direct-shim.sh"

INSTALL_DIR="${OMP_CUA_DRIVER_DIR:-$HOME/.omp/natives/cua-driver}"
INSTALLED="$INSTALL_DIR/cua-driver"
MANIFEST="$INSTALL_DIR/manifest.json"
OUT_DIR="${HARNESS_OUT_DIR:-$HOME/tmp/cua/harness-out}"
STAGED="$OUT_DIR/cua-driver"
BUILD_JSON="$OUT_DIR/build.json"
EVIDENCE="$OUT_DIR/evidence.jsonl"
LOG_DIR="$OUT_DIR/logs"
DUMMY_SOCK="$OUT_DIR/daemon.sock"
IDENTITY="${CODESIGN_IDENTITY:-OMP Computer Use}"
BUNDLE_ID="com.ohmypi.cua-driver"
PROCESS_PATTERN='^(cua-driver|CuaDriver|CuaTestHarness)'
STALE_SOCKETS=("$HOME/Library/Caches/cua-driver/cua-driver.sock" "$HOME/Library/Caches/cua-driver-local/cua-driver-local.sock" "$DUMMY_SOCK")

DRY=0
ALLOW_DIRTY=0
FAILS=0

die() { printf 'harness: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }
run_cmd() {
	printf '+'; printf ' %q' "$@"; printf '\n'
	if [ "$DRY" = 1 ]; then return 0; fi
	"$@"
}
sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
head_sha() { git -C "$REPO_ROOT" rev-parse --short=9 HEAD; }
tree_dirty() { [ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no -- libs/cua-driver/rust)" ]; }
need_target_dir() {
	[ -n "${CARGO_TARGET_DIR:-}" ] || die "set CARGO_TARGET_DIR to a private lane directory (never a shared one)"
	case "$CARGO_TARGET_DIR" in *shared*) die "CARGO_TARGET_DIR=$CARGO_TARGET_DIR looks shared; use a private lane directory" ;; esac
}
codesign_field() { codesign -dvv "$1" 2>&1 | sed -n "s/^$2=//p" | head -1; }
macos_version() { sw_vers -productVersion; }
json_escape() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }

check() {
	local name="$1"; shift
	if [ "$DRY" = 1 ]; then
		printf 'CHECK %-32s %s\n' "$name" "$*"
		return 0
	fi
	if "$@" >/dev/null 2>&1; then
		printf 'PASS  %s\n' "$name"
	else
		printf 'FAIL  %s\n' "$name"
		FAILS=$((FAILS + 1))
	fi
}
no_driver_processes() { ! pgrep -f "$PROCESS_PATTERN" >/dev/null; }
no_stale_sockets() { local s; for s in "${STALE_SOCKETS[@]}"; do [ ! -e "$s" ] || return 1; done; }
installed_matches_manifest() {
	local recorded
	recorded="$(sed -n 's/.*"sha256": *"\([0-9a-f]*\)".*/\1/p' "$MANIFEST")"
	[ -n "$recorded" ] && [ "$recorded" = "$(sha256 "$INSTALLED")" ]
}
installed_identity_ok() {
	codesign --verify --strict "$INSTALLED" &&
		[ "$(codesign_field "$INSTALLED" Identifier)" = "$BUNDLE_ID" ] &&
		[ "$(codesign_field "$INSTALLED" Authority)" = "$IDENTITY" ]
}
signing_identity_present() { security find-identity -p codesigning 2>/dev/null | grep -Fq "\"$IDENTITY\""; }
session_permissions_granted() {
	python3 - <<'EOF'
import ctypes, sys
ax = ctypes.CDLL('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
cg = ctypes.CDLL('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
ax.AXIsProcessTrusted.restype = ctypes.c_bool
cg.CGPreflightScreenCaptureAccess.restype = ctypes.c_bool
sys.exit(0 if ax.AXIsProcessTrusted() and cg.CGPreflightScreenCaptureAccess() else 1)
EOF
}
target_dir_private() { [ -n "${CARGO_TARGET_DIR:-}" ] && case "$CARGO_TARGET_DIR" in *shared*) false ;; *) true ;; esac; }

cmd_preflight() {
	check "host is macOS" test "$(uname -s)" = Darwin
	check "no driver/harness process" no_driver_processes
	check "no stale daemon socket" no_stale_sockets
	check "installed driver present" test -x "$INSTALLED"
	check "installed sha matches manifest" installed_matches_manifest
	check "installed signature $IDENTITY" installed_identity_ok
	check "signing identity in keychain" signing_identity_present
	check "terminal session AX+capture" session_permissions_granted
	check "shim present" test -x "$SHIM"
	check "AppKit fixture built" test -x "$FIXTURE_APP/Contents/MacOS/CuaTestHarness.AppKit"
	check "CARGO_TARGET_DIR private" target_dir_private
	check "cargo on PATH" command -v cargo
	if [ "$DRY" = 1 ]; then return 0; fi
	[ "$FAILS" = 0 ] || die "$FAILS preflight check(s) failed"
}

cmd_fixture() {
	run_cmd "$FIXTURE_BUILD" --only appkit
}

cmd_build() {
	need_target_dir
	local head
	head="$(head_sha)"
	if tree_dirty && [ "$ALLOW_DIRTY" = 0 ]; then
		die "libs/cua-driver/rust has uncommitted changes at $head; commit them or pass --allow-dirty"
	fi
	run_cmd mkdir -p "$OUT_DIR"
	(cd "$RUST_ROOT" && run_cmd cargo build --locked --release -p cua-driver)
	run_cmd rm -f "$STAGED"
	run_cmd cp "$CARGO_TARGET_DIR/release/cua-driver" "$STAGED"
	run_cmd codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$STAGED"
	run_cmd codesign --verify --strict "$STAGED"
	if [ "$DRY" = 1 ]; then
		say "+ write $BUILD_JSON {head, dirty, sha256, size, built_at}"
		return 0
	fi
	local sum size dirty=false
	sum="$(sha256 "$STAGED")"
	size="$(stat -f '%z' "$STAGED")"
	tree_dirty && dirty=true
	printf '{"head":"%s","dirty":%s,"sha256":"%s","size":%s,"identity":"%s","built_at":"%s"}\n' \
		"$head" "$dirty" "$sum" "$size" "$(json_escape "$IDENTITY")" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$BUILD_JSON"
	say "head $head dirty=$dirty"
	say "sha256 $sum ($size bytes) $STAGED"
}

built_sha() {
	[ -f "$BUILD_JSON" ] || die "no build recorded at $BUILD_JSON; run build first"
	sed -n 's/.*"sha256":"\([0-9a-f]*\)".*/\1/p' "$BUILD_JSON"
}
built_head() { sed -n 's/.*"head":"\([0-9a-f]*\)".*/\1/p' "$BUILD_JSON"; }

cmd_install() {
	if [ "$DRY" = 0 ]; then
		[ -f "$BUILD_JSON" ] || die "no build recorded at $BUILD_JSON; run build first"
		[ -x "$STAGED" ] || die "no staged build at $STAGED; run build first"
		[ "$(sha256 "$STAGED")" = "$(built_sha)" ] || die "staged binary no longer matches $BUILD_JSON"
		no_driver_processes || die "a driver/harness process is still running; stop it before installing"
	fi
	run_cmd mkdir -p "$INSTALL_DIR"
	if [ -e "$INSTALLED" ]; then
		run_cmd rm -f "$INSTALLED.prev" "$MANIFEST.prev"
		run_cmd mv "$INSTALLED" "$INSTALLED.prev"
		if [ -e "$MANIFEST" ]; then run_cmd mv "$MANIFEST" "$MANIFEST.prev"; fi
	fi
	run_cmd cp "$STAGED" "$INSTALLED"
	run_cmd chmod 755 "$INSTALLED"
	if [ "$DRY" = 1 ]; then
		say "+ verify sha256($INSTALLED) == built sha256"
		say "+ write $MANIFEST {platform, version, sha256, source}"
		return 0
	fi
	local sum
	sum="$(sha256 "$INSTALLED")"
	[ "$sum" = "$(built_sha)" ] || die "installed sha256 $sum != built $(built_sha)"
	local version
	version="$("$INSTALLED" --version 2>/dev/null | sed -n 's/^cua-driver \([0-9][^ ]*\).*/\1/p')"
	printf '{\n  "platform": "darwin-arm64",\n  "version": "%s",\n  "sha256": "%s",\n  "source": "%s"\n}\n' \
		"${version:-unknown}" "$sum" \
		"$(json_escape "$(git -C "$REPO_ROOT" remote get-url fork 2>/dev/null || echo local) branch $(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD) commit $(built_head) (cargo build --locked --release -p cua-driver; codesign $IDENTITY)")" \
		>"$MANIFEST"
	say "installed $sum -> $INSTALLED (previous kept as cua-driver.prev)"
}

cmd_restore() {
	if [ "$DRY" = 0 ]; then
		[ -e "$INSTALLED.prev" ] || die "nothing to restore: $INSTALLED.prev is missing"
		no_driver_processes || die "a driver/harness process is still running; stop it before restoring"
	fi
	run_cmd rm -f "$INSTALLED"
	run_cmd cp "$INSTALLED.prev" "$INSTALLED"
	run_cmd chmod 755 "$INSTALLED"
	if [ -e "$MANIFEST.prev" ] || [ "$DRY" = 1 ]; then
		run_cmd rm -f "$MANIFEST"
		run_cmd cp "$MANIFEST.prev" "$MANIFEST"
	fi
	run_cmd rm -f "$INSTALLED.prev" "$MANIFEST.prev"
	[ "$DRY" = 1 ] || say "restored $(sha256 "$INSTALLED") -> $INSTALLED"
}

DUMMY_PID=""
stop_dummy_socket() {
	[ -n "$DUMMY_PID" ] && kill "$DUMMY_PID" 2>/dev/null || true
	rm -f "$DUMMY_SOCK"
}
start_dummy_socket() {
	python3 - "$DUMMY_SOCK" <<'EOF' &
import os, socket, sys
path = sys.argv[1]
if os.path.exists(path):
    os.unlink(path)
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
server.listen(8)
while True:
    conn, _ = server.accept()
    conn.close()
EOF
	DUMMY_PID=$!
	local i
	for i in 1 2 3 4 5 6 7 8 9 10; do
		[ -S "$DUMMY_SOCK" ] && return 0
		sleep 0.2
	done
	die "dummy daemon socket never appeared at $DUMMY_SOCK"
}

cmd_run() {
	local filter="${1:-}"
	need_target_dir
	local -a cargo_test=(cargo test --locked -p cua-driver --test harness_appkit_test --)
	local -a env_lines=(
		"CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
		"CUA_TEST_DRIVER_BIN=$SHIM"
		"CUA_SHIM_REAL=$INSTALLED"
		"CUA_E2E_MACOS_DAEMON_SOCKET=$DUMMY_SOCK"
		"CUA_TEST_APPS_ROOT=$RUST_ROOT/test-apps"
		"CUA_TEST_REQUIRE_FIXTURES=1"
	)
	if [ "$DRY" = 1 ]; then
		say "+ python3 (unix listener) $DUMMY_SOCK &"
		say "+ cd $RUST_ROOT && ${env_lines[*]} ${cargo_test[*]} --list --ignored --format terse | sed -n 's/: test\$//p' | grep -F -- '$filter'"
		say "+ for each listed test: ${env_lines[*]} ${cargo_test[*]} --ignored --exact <test> --nocapture --test-threads=1 > $LOG_DIR/<test>.log; append {test,result,duration_s,...} to $EVIDENCE"
		say "+ kill dummy listener; rm -f $DUMMY_SOCK"
		return 0
	fi
	[ -x "$INSTALLED" ] || die "no installed driver at $INSTALLED"
	[ -x "$FIXTURE_APP/Contents/MacOS/CuaTestHarness.AppKit" ] || die "AppKit fixture missing; run: $0 fixture"
	no_driver_processes || die "a driver/harness process is still running; run preflight"
	mkdir -p "$LOG_DIR"
	local head bin_sha bin_head=unknown identity macos
	head="$(head_sha)"
	bin_sha="$(sha256 "$INSTALLED")"
	if [ -f "$BUILD_JSON" ] && [ "$(built_sha)" = "$bin_sha" ]; then bin_head="$(built_head)"; fi
	identity="$(codesign_field "$INSTALLED" Identifier)/$(codesign_field "$INSTALLED" Authority)"
	macos="$(macos_version)"
	trap stop_dummy_socket EXIT
	start_dummy_socket
	export "${env_lines[@]}"
	cd "$RUST_ROOT"
	local tests
	tests="$("${cargo_test[@]}" --list --ignored --format terse | sed -n 's/: test$//p' | grep -F -- "$filter" || true)"
	[ -n "$tests" ] || die "no harness_appkit test matches '$filter'"
	local test result start end duration log
	while IFS= read -r test; do
		log="$LOG_DIR/${test//::/__}.log"
		say "== $test"
		start="$(python3 -c 'import time; print(time.time())')"
		if "${cargo_test[@]}" --ignored --exact "$test" --nocapture --test-threads=1 >"$log" 2>&1; then
			result=ok
		else
			result=FAILED
		fi
		end="$(python3 -c 'import time; print(time.time())')"
		duration="$(python3 -c "print(round($end - $start, 2))")"
		pkill -f CuaTestHarness >/dev/null 2>&1 || true
		printf '{"test":"%s","result":"%s","duration_s":%s,"head":"%s","binary_head":"%s","binary_sha256":"%s","binary":"%s","identity":"%s","macos":"%s","log":"%s","at":"%s"}\n' \
			"$test" "$result" "$duration" "$head" "$bin_head" "$bin_sha" "$(json_escape "$INSTALLED")" "$(json_escape "$identity")" "$macos" "$(json_escape "$log")" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$EVIDENCE"
		say "   $result ${duration}s"
	done <<<"$tests"
	say "evidence appended to $EVIDENCE"
}

cmd_evidence() {
	local file="${1:-$EVIDENCE}"
	[ -f "$file" ] || die "no evidence file at $file"
	python3 - "$file" <<'EOF'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
if not rows:
    sys.exit("evidence file is empty")
def one(key):
    values = sorted({row[key] for row in rows})
    return values[0] if len(values) == 1 else " / ".join(values)
passed = sum(1 for row in rows if row["result"] == "ok")
print("| field | value |")
print("|---|---|")
print(f"| head (tests) | `{one('head')}` |")
print(f"| head (driver binary) | `{one('binary_head')}` |")
print(f"| driver binary | `{one('binary')}` |")
print(f"| binary sha256 | `{one('binary_sha256')}` |")
print(f"| daemon identity | `{one('identity')}` |")
print(f"| macOS | {one('macos')} |")
print(f"| result | {passed}/{len(rows)} pass |")
print()
print("| test | result | duration |")
print("|---|---|---|")
for row in rows:
    print(f"| `{row['test']}` | {row['result']} | {row['duration_s']:.2f}s |")
EOF
}

cmd_allowlist_check() {
	local declared missing=0 name
	declared="$(sed -n 's/^[[:space:]]*fn \(harness_appkit_[A-Za-z0-9_]*\)().*/\1/p' "$TEST_FILE")"
	while IFS= read -r name; do
		if grep -Eq "^[[:space:]]*([A-Za-z0-9_]+::)?${name}([^A-Za-z0-9_].*)?$" "$E2E_SCRIPT"; then
			printf 'PASS  %s\n' "$name"
		else
			printf 'FAIL  %s (absent from %s)\n' "$name" "${E2E_SCRIPT#"$REPO_ROOT/"}"
			missing=$((missing + 1))
		fi
	done <<<"$declared"
	[ "$missing" = 0 ] || die "$missing harness_appkit case(s) missing from the canonical allowlist"
}

main() {
	local cmd="" arg
	local -a rest=()
	for arg in "$@"; do
		case "$arg" in
			--dry-run) DRY=1 ;;
			--allow-dirty) ALLOW_DIRTY=1 ;;
			-h | --help) sed -n '2,4p' "$0"; exit 0 ;;
			*) if [ -z "$cmd" ]; then cmd="$arg"; else rest+=("$arg"); fi ;;
		esac
	done
	case "$cmd" in
		preflight) cmd_preflight ;;
		fixture) cmd_fixture ;;
		build) cmd_build ;;
		install) cmd_install ;;
		run) cmd_run ${rest[@]+"${rest[@]}"} ;;
		evidence) cmd_evidence ${rest[@]+"${rest[@]}"} ;;
		restore) cmd_restore ;;
		allowlist-check) cmd_allowlist_check ;;
		"") sed -n '2,4p' "$0"; exit 2 ;;
		*) die "unknown subcommand: $cmd" ;;
	esac
}

main "$@"
