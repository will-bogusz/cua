//! `mcp --direct` must let a client stop one in-flight call — through the MCP
//! `notifications/cancelled` notification and through the `cancel_operation`
//! fallback tool — on both the legacy `initialize` era and the per-request
//! modern era, and the cancelled call must answer with its typed outcome.
//!
//! `verify_state` against a window that never appears is the probe: it polls
//! for the whole `timeout_ms`, so a reply that arrives well before that
//! deadline can only be the cancellation.

use std::time::{Duration, Instant};

use cua_driver_testkit::RawDriver;
use serde_json::{json, Value};

const POLL_TIMEOUT_MS: u64 = 9_000;
const MODERN_META: &str = r#"{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

fn spawn() -> Option<RawDriver> {
    #[cfg(target_os = "macos")]
    {
        RawDriver::spawn_explicit_direct()
    }
    #[cfg(not(target_os = "macos"))]
    {
        RawDriver::spawn_direct()
    }
}

fn slow_call(id: u64, meta: Option<&Value>) -> Value {
    let mut params = json!({
        "name": "verify_state",
        "arguments": {
            "pid": 1,
            "window_id": 999_999,
            "timeout_ms": POLL_TIMEOUT_MS,
            "expect": [{"window": {"exists": true}}],
        },
    });
    if let Some(meta) = meta {
        params["_meta"] = meta.clone();
    }
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
}

fn cancel_operation(id: u64, request_id: u64, meta: Option<&Value>) -> Value {
    let mut params = json!({
        "name": "cancel_operation",
        "arguments": {"request_id": request_id},
    });
    if let Some(meta) = meta {
        params["_meta"] = meta.clone();
    }
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
}

#[track_caller]
fn assert_cancelled(response: &Value, id: u64) {
    assert_eq!(response["id"], id, "{response:?}");
    assert_eq!(response["result"]["isError"], true, "{response:?}");
    let structured = &response["result"]["structuredContent"];
    assert_eq!(structured["code"], "cancelled", "{response:?}");
    // A cancel that lands before the poll starts leaves nothing to reconcile;
    // one that interrupts it reports the samples already taken.
    if !structured["partial"].is_null() {
        assert_eq!(
            structured["partial"]["tool"], "verify_state",
            "{response:?}"
        );
    }
}

fn exercise(driver: &mut RawDriver, meta: Option<&Value>) {
    // Cancellation by notification: the spec-level path, which has no reply
    // of its own, so the cancelled call's own reply is the only evidence.
    let started = Instant::now();
    driver.send(&slow_call(10, meta));
    std::thread::sleep(Duration::from_millis(500));
    driver.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": 10, "reason": "test"},
    }));
    let reply = driver.recv();
    assert_cancelled(&reply, 10);
    assert!(
        started.elapsed() < Duration::from_millis(POLL_TIMEOUT_MS / 2),
        "cancellation did not interrupt the poll: {:?}",
        started.elapsed()
    );

    // Cancellation by tool: for clients whose tool loop cannot emit a
    // notification mid-call. The tool answers first with the named call id;
    // the cancelled call answers second.
    let started = Instant::now();
    driver.send(&slow_call(11, meta));
    std::thread::sleep(Duration::from_millis(500));
    driver.send(&cancel_operation(12, 11, meta));
    let first = driver.recv();
    let second = driver.recv();
    let (cancel_reply, cancelled_reply) = if first["id"] == 12 {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(cancel_reply["id"], 12, "{cancel_reply:?}");
    assert_eq!(
        cancel_reply["result"]["structuredContent"]["cancelled"], true,
        "{cancel_reply:?}"
    );
    assert_cancelled(&cancelled_reply, 11);
    assert!(
        started.elapsed() < Duration::from_millis(POLL_TIMEOUT_MS / 2),
        "cancel_operation did not interrupt the poll: {:?}",
        started.elapsed()
    );
}

#[test]
fn legacy_initialize_session_cancels_in_flight_calls() {
    let Some(mut driver) = spawn() else {
        return;
    };
    driver.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}},
    }));
    let initialized = driver.recv();
    assert_eq!(initialized["id"], 1, "{initialized:?}");

    exercise(&mut driver, None);
}

#[test]
fn modern_per_request_session_cancels_in_flight_calls() {
    let Some(mut driver) = spawn() else {
        return;
    };
    let meta: Value = serde_json::from_str(MODERN_META).expect("modern metadata");

    exercise(&mut driver, Some(&meta));
}
