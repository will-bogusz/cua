#!/bin/sh
# Test-only driver shim: rewrites `mcp --socket <daemon>` into `mcp --direct` so the AppKit
# harness runs the installed, TCC-granted binary as a child of the granted terminal session.
# Everything else passes through. Selected via CUA_TEST_DRIVER_BIN; CUA_SHIM_REAL names the binary.
REAL="${CUA_SHIM_REAL:-$HOME/.omp/natives/cua-driver/cua-driver}"

if [ "$1" = "mcp" ]; then
	shift
	direct=0
	for arg in "$@"; do
		shift
		if [ "$arg" = "--socket" ]; then
			direct=1
			skip=1
			continue
		fi
		if [ "${skip:-0}" = "1" ]; then
			skip=0
			continue
		fi
		set -- "$@" "$arg"
	done
	if [ "$direct" = "1" ]; then
		exec "$REAL" mcp --direct "$@"
	fi
	exec "$REAL" mcp "$@"
fi

exec "$REAL" "$@"
