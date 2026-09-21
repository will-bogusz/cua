//! Integration test against the CuaTestHarness.AppKit Swift app.
//!
//! Mirror of `harness_wpf_test.rs` for the macOS AppKit hosting pattern.
//! The harness app lives at `libs/cua-driver/tests/fixtures/apps/macos/appkit`
//! and is published into `libs/cua-driver/rust/test-apps/harness-appkit/`
//! by `libs/cua-driver/tests/fixtures/build/macos.sh`.
//!
//! Scenarios (see `libs/cua-driver/tests/fixtures/shared/scenarios.json`
//! `appkit` section):
//!   - counter        : NSButton AXPress invocation increments counter
//!   - text_body      : get_window_state extracts known marker text
//!   - text_input     : type_text into NSTextField updates mirror label
//!   - click_target   : right_click / double_click recognised by NSView
//!   - scroll_target  : scroll updates VerticalOffset label
//!   - ns_menubar     : main menubar item enumerable (Mac-specific)
//!
//! Run locally (after `libs/cua-driver/tests/fixtures/build/macos.sh`):
//!   cargo test --test harness_appkit_test -- --ignored --nocapture
//!
//! Tests are `#[ignore]` so they don't run in plain `cargo test`.
//!
//! The macOS lane preflight verifies the installed daemon identity and TCC
//! grants before these tests run. Missing fixtures or AX trees fail here too.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use cua_driver_testkit::ax::{element_index_by_id, element_index_containing, has_id, looks_empty};
use cua_driver_testkit::e2e::{
    execute_case, native_background_case, native_foreground_case, native_readonly_case,
    recording_evidence, DriverRoute, Evidence, Observation, OracleKind, RefusalCode, Targeting,
};
use cua_driver_testkit::observer::{NativeObserver, ObserverBackend, TargetWindow};
use cua_driver_testkit::sentinel::{run_with_background_oracles, ForegroundSentinel};
use cua_driver_testkit::{Driver, McpDriver, ToolResponse};

#[path = "support/appkit_snapshot_publication.rs"]
mod snapshot_publication;

// ── paths ────────────────────────────────────────────────────────────────────

fn harness_app() -> PathBuf {
    if let Ok(p) = std::env::var("HARNESS_APPKIT_APP") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    cua_driver_testkit::harness_app("harness-appkit", "CuaTestHarness.AppKit.app")
}

fn harness_exe() -> PathBuf {
    harness_app().join("Contents/MacOS/CuaTestHarness.AppKit")
}

// ── harness fixture ──────────────────────────────────────────────────────────

struct Harness {
    _app: Child,
    pid: u32,
}

impl Harness {
    fn launch() -> Self {
        Self::launch_with_command_oracle(None)
    }

    fn launch_with_env(env: &[(&str, &str)]) -> Self {
        Self::launch_with(None, None, false, env)
    }

    fn launch_with_command_oracle(command_oracle: Option<&Path>) -> Self {
        Self::launch_with_oracles(command_oracle, None)
    }

    fn launch_with_oracles(command_oracle: Option<&Path>, pointer_oracle: Option<&Path>) -> Self {
        Self::launch_with_options(command_oracle, pointer_oracle, false)
    }

    fn launch_with_options(
        command_oracle: Option<&Path>,
        pointer_oracle: Option<&Path>,
        keep_ordered_front: bool,
    ) -> Self {
        Self::launch_with(command_oracle, pointer_oracle, keep_ordered_front, &[])
    }

    fn launch_with(
        command_oracle: Option<&Path>,
        pointer_oracle: Option<&Path>,
        keep_ordered_front: bool,
        env: &[(&str, &str)],
    ) -> Self {
        let exe = harness_exe();
        assert!(
            exe.exists(),
            "required AppKit harness is missing at {exe:?}; run the fixture build"
        );
        // Launch the binary directly (not via `open`) so we control the pid
        // and can kill it cleanly on Drop. The app still installs an AppKit
        // window via NSApp.run().
        let mut command = Command::new(&exe);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(path) = command_oracle {
            command.env("CUA_APPKIT_COMMAND_ORACLE", path);
        }
        if let Some(path) = pointer_oracle {
            command.env("CUA_APPKIT_POINTER_ORACLE", path);
        }
        if keep_ordered_front {
            command.env("CUA_APPKIT_KEEP_ORDERED_FRONT", "1");
        }
        for (name, value) in env {
            command.env(name, value);
        }
        let app = command
            .spawn()
            .unwrap_or_else(|error| panic!("launch AppKit harness {exe:?}: {error}"));
        let pid = app.id();
        // Settle for window creation + activation.
        std::thread::sleep(Duration::from_millis(800));
        Self { _app: app, pid }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self._app.kill();
        let _ = self._app.wait();
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ── window / element helpers ─────────────────────────────────────────────────

fn snapshot_elements(driver: &mut McpDriver, pid: u32, window_id: u64) -> ToolResponse {
    driver.call(
        "get_window_state",
        serde_json::json!({
            "pid": pid as i64,
            "window_id": window_id,
            "capture_mode": "ax"
        }),
    )
}

/// True when a chord reply blames a focus holder for the key-window state.
/// `key_window_note` produces exactly two shapes — another window held focus,
/// or the pid reported no key window at all — and both survive the closed
/// ActionResult only as reply prose.
fn key_window_complaint(reply_text: &str) -> bool {
    reply_text.contains("key window when the chord was posted")
}

fn element_token_by_id(snapshot: &ToolResponse, identifier: &str) -> String {
    let index = element_index_by_id(snapshot.tree_text(), identifier)
        .unwrap_or_else(|| panic!("{identifier} element_index not found"));
    snapshot.structured()["elements"]
        .as_array()
        .and_then(|elements| {
            elements
                .iter()
                .find(|element| element["element_index"].as_u64() == Some(index))
        })
        .and_then(|element| element["element_token"].as_str())
        .unwrap_or_else(|| panic!("{identifier} element_token not found"))
        .to_owned()
}

fn element_pixel_frame(snapshot: &ToolResponse, identifier: &str) -> (f64, f64, f64, f64) {
    let index = element_index_by_id(snapshot.tree_text(), identifier)
        .unwrap_or_else(|| panic!("{identifier} element_index not found"));
    let elements = snapshot.structured()["elements"]
        .as_array()
        .expect("AppKit structured elements");
    let element = elements
        .iter()
        .find(|element| element["element_index"].as_u64() == Some(index))
        .unwrap_or_else(|| panic!("{identifier} element frame not found"));
    let window = elements
        .iter()
        .find(|element| element["role"].as_str() == Some("AXWindow"))
        .expect("AppKit window frame");
    let scale = snapshot.structured()["screenshot_width"]
        .as_f64()
        .unwrap_or(1.0)
        / window["frame"]["w"].as_f64().unwrap_or(1.0).max(1.0);
    (
        (element["frame"]["x"].as_f64().unwrap_or(0.0)
            - window["frame"]["x"].as_f64().unwrap_or(0.0))
            * scale,
        (element["frame"]["y"].as_f64().unwrap_or(0.0)
            - window["frame"]["y"].as_f64().unwrap_or(0.0))
            * scale,
        element["frame"]["w"].as_f64().unwrap_or(0.0) * scale,
        element["frame"]["h"].as_f64().unwrap_or(0.0) * scale,
    )
}

/// One structured row of a snapshot, by the accessibility identifier its
/// markdown row carries.
fn element_by_id(snapshot: &ToolResponse, identifier: &str) -> serde_json::Value {
    let index = element_index_by_id(snapshot.tree_text(), identifier)
        .unwrap_or_else(|| panic!("{identifier} element_index not found"));
    element_where(snapshot, |element| {
        element["element_index"].as_u64() == Some(index)
    })
    .unwrap_or_else(|| panic!("{identifier} structured element not found"))
}

/// The first structured row satisfying `matches`, if the snapshot has one.
fn element_where(
    snapshot: &ToolResponse,
    matches: impl Fn(&serde_json::Value) -> bool,
) -> Option<serde_json::Value> {
    snapshot.structured()["elements"]
        .as_array()
        .and_then(|elements| elements.iter().find(|element| matches(element)))
        .cloned()
}

fn run_case(
    case: cua_driver_testkit::e2e::CaseSpec,
    test: impl FnOnce(u32, u64, &mut McpDriver) -> Observation,
) {
    run_case_with_env(case, &[], test);
}

fn run_case_with_env(
    case: cua_driver_testkit::e2e::CaseSpec,
    env: &[(&str, &str)],
    test: impl FnOnce(u32, u64, &mut McpDriver) -> Observation,
) {
    let cell_id = case.cell_id.clone();
    let delivery = case.delivery;
    execute_case(case, |evidence| {
        let mut driver = McpDriver::spawn_macos_daemon_proxy_named(&cell_id)
            .expect("start installed macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());
        let harness = Harness::launch_with_env(env);
        let (wid, _) = driver
            .find_window(harness.pid as i64, "CuaTestHarness AppKit")
            .expect("AppKit main window not found");
        if delivery != cua_driver_testkit::e2e::Delivery::Background {
            driver.start_behavior_recording();
        }
        test(harness.pid, wid, &mut driver)
    });
}

fn run_background_case(
    action: &str,
    route: DriverRoute,
    test: impl FnOnce(u32, u64, &mut McpDriver),
) {
    run_background_case_targeting(action, Targeting::Ax, route, test);
}

fn run_background_case_targeting(
    action: &str,
    targeting: Targeting,
    route: DriverRoute,
    test: impl FnOnce(u32, u64, &mut McpDriver),
) {
    run_background_case_with_env(action, targeting, route, &[], test);
}

fn run_background_case_with_env(
    action: &str,
    targeting: Targeting,
    route: DriverRoute,
    env: &[(&str, &str)],
    test: impl FnOnce(u32, u64, &mut McpDriver),
) {
    run_case_with_env(
        native_background_case("appkit", action, targeting, route),
        env,
        |pid, wid, driver| {
            let (_, passed) = run_with_background_oracles(
                driver,
                TargetWindow {
                    pid,
                    native_id: wid,
                },
                |driver| test(pid, wid, driver),
            )
            .unwrap_or_else(|error| panic!("background desktop contract failed: {error}"));
            Observation::delivered_with_fixture_state(passed)
        },
    );
}

// ── tests ────────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn harness_appkit_exact_activation_with_agent_cursor() {
    let mut case = native_foreground_case(
        "appkit",
        "exact_activation_with_agent_cursor",
        Targeting::NotApplicable,
        DriverRoute::WindowState,
    );
    case.oracles.extend([OracleKind::Focus, OracleKind::Cursor]);
    run_case(case, |pid, wid, driver| {
        let snapshot = snapshot_elements(driver, pid, wid);
        assert!(!snapshot.is_error(), "snapshot: {}", snapshot.text());
        let target = TargetWindow {
            pid,
            native_id: wid,
        };
        let observer = NativeObserver::new();
        let before = observer.snapshot(target).expect("observe native desktop");
        let socket = std::env::var("CUA_E2E_MACOS_DAEMON_SOCKET")
            .expect("canonical installed daemon socket");
        let mut peer = McpDriver::spawn_daemon_proxy_unrecorded(&socket)
            .expect("start concurrent cursor session");
        let verifies_target = |response: &ToolResponse| {
            let state = response.structured();
            !response.is_error()
                && state["activated"] == true
                && state["observed"]["focused_window_id"].as_u64() == Some(wid)
                && state["observed"]["frontmost_ordinary_window_id"].as_u64() == Some(wid)
                && state["observed"]["frontmost_pid"].as_u64() == Some(u64::from(pid))
        };
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let activated = std::thread::scope(|scope| {
            let moving = scope.spawn(|| {
                let snapshot = snapshot_elements(&mut peer, pid, wid);
                assert!(!snapshot.is_error(), "peer snapshot: {}", snapshot.text());
                let motion = peer.call(
                    "set_agent_cursor_motion",
                    serde_json::json!({"idle_hide_ms": 0, "glide_duration_ms": 0}),
                );
                assert!(!motion.is_error(), "cursor motion: {}", motion.text());
                let deadline = std::time::Instant::now() + Duration::from_secs(60);
                let mut first = true;
                let mut x = 120;
                while !stopped.load(std::sync::atomic::Ordering::Relaxed)
                    && std::time::Instant::now() < deadline
                {
                    let moved = peer.call(
                        "move_cursor",
                        serde_json::json!({
                            "target": {"kind": "window", "pid": pid, "window_id": wid},
                            "x": x,
                            "y": 100
                        }),
                    );
                    assert!(!moved.is_error(), "agent cursor: {}", moved.text());
                    if first {
                        ready.send(()).expect("cursor readiness");
                        first = false;
                    }
                    x = if x == 120 { 121 } else { 120 };
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(
                    stopped.load(std::sync::atomic::Ordering::Relaxed),
                    "cursor producer expired before the activation interval completed"
                );
            });
            started
                .recv_timeout(Duration::from_secs(15))
                .expect("live cursor ready");
            let mut result = driver.call(
                "bring_to_front",
                serde_json::json!({"pid": pid, "window_id": wid}),
            );
            for _ in 1..20 {
                if !verifies_target(&result) {
                    break;
                }
                result = driver.call(
                    "bring_to_front",
                    serde_json::json!({"pid": pid, "window_id": wid}),
                );
            }
            stopped.store(true, std::sync::atomic::Ordering::Relaxed);
            moving.join().expect("concurrent cursor transport");
            result
        });
        assert!(
            verifies_target(&activated),
            "active agent cursor must not invalidate exact activation: {}",
            activated.raw
        );
        let after = observer.snapshot(target).expect("observe activated target");
        assert_eq!(after.foreground, Some(u64::from(pid)));
        assert_eq!(after.cursor_pos, before.cursor_pos, "real pointer moved");
        Observation::delivered_with_fixture_state(vec![OracleKind::Focus, OracleKind::Cursor])
    });
}

struct FrontWindow {
    window_id: u64,
    z_index: i64,
    app_name: String,
    title: String,
}

impl std::fmt::Display for FrontWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "window {} (z_index {}, {} {:?})",
            self.window_id, self.z_index, self.app_name, self.title
        )
    }
}

/// The window WindowServer ranks first in the global layer-0 order, read from
/// `list_windows` (`z_index`: higher values are closer to the front) so the
/// observation does not depend on the tool under test.
fn front_layer_zero_window(driver: &mut McpDriver) -> Option<FrontWindow> {
    let response = driver.call("list_windows", serde_json::json!({"on_screen_only": true}));
    response.structured()["windows"]
        .as_array()?
        .iter()
        .filter(|window| window["layer"].as_i64() == Some(0))
        .filter_map(|window| {
            Some(FrontWindow {
                window_id: window["window_id"].as_u64()?,
                z_index: window["z_index"].as_i64()?,
                app_name: window["app_name"].as_str().unwrap_or_default().to_owned(),
                title: window["title"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .max_by_key(|window| window.z_index)
}

/// Poll the global layer-0 order until `window_id` leads it, up to 3s, and
/// return the last observation so a caller can name whatever leads instead.
fn await_front_layer_zero_window(driver: &mut McpDriver, window_id: u64) -> Option<FrontWindow> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let front = front_layer_zero_window(driver);
        if front.as_ref().map(|window| window.window_id) == Some(window_id)
            || std::time::Instant::now() >= deadline
        {
            return front;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Another application keeps its window ordered front while the requested
/// window is the focused, front window of its own process. `bring_to_front`
/// verifies: the global layer-0 order is not the requesting application's to
/// win, so losing it says nothing about where keyboard input goes. The refusal
/// path of the exact-window code stays covered by the modal-sheet case in the
/// macOS certification suite.
///
/// The competitor is a second instance of the same fixture bundle, so
/// NSWorkspace keeps reporting the other instance as frontmost; the cell
/// therefore relies on the accessibility focused-window oracle rather than
/// the workspace frontmost process.
#[test]
#[ignore]
fn harness_appkit_exact_activation_ignores_competing_application_window() {
    let mut case = native_foreground_case(
        "appkit",
        "exact_activation_competing_window",
        Targeting::NotApplicable,
        DriverRoute::WindowState,
    );
    case.oracles.push(OracleKind::Cursor);
    run_case(case, |pid, wid, driver| {
        let competitor = Harness::launch_with_options(None, None, true);
        let (competing_wid, _) = driver
            .find_window(competitor.pid as i64, "CuaTestHarness AppKit")
            .expect("find competing ordinary window");
        assert_ne!(competing_wid, wid);
        let snapshot = snapshot_elements(driver, pid, wid);
        assert!(!snapshot.is_error(), "target snapshot: {}", snapshot.text());
        let observer = NativeObserver::new();
        let target = TargetWindow {
            pid,
            native_id: wid,
        };
        let before = observer.snapshot(target).expect("observe competing window");
        let front = await_front_layer_zero_window(driver, competing_wid);
        let leader = front
            .as_ref()
            .map(FrontWindow::to_string)
            .unwrap_or_else(|| "no on-screen layer-0 window".to_owned());
        assert_eq!(
            front.map(|window| window.window_id),
            Some(competing_wid),
            "competing window {competing_wid} must lead the global layer-0 order before \
             bring_to_front; list_windows ranks {leader} first"
        );
        let response = driver.call(
            "bring_to_front",
            serde_json::json!({"pid": pid, "window_id": wid}),
        );
        assert!(
            !response.is_error(),
            "another application ordering its window front must not unverify activation: {}",
            response.raw
        );
        assert_eq!(
            response.structured()["code"],
            "bring_to_front_exact_window_verified"
        );
        assert_eq!(response.structured()["activated"], true);
        assert_eq!(response.structured()["process_activated"], true);
        assert_eq!(
            response.structured()["exact_window_effect"]["focused"],
            true
        );
        assert_eq!(
            response.structured()["exact_window_effect"]["front_in_process"],
            true
        );
        assert_eq!(
            response.structured()["observed"]["focused_window_id"].as_u64(),
            Some(wid)
        );
        let after = observer.snapshot(target).expect("observe activated target");
        assert_eq!(after.cursor_pos, before.cursor_pos, "real pointer moved");
        Observation::delivered_with_fixture_state(vec![OracleKind::Cursor])
    });
}

#[test]
#[ignore]
fn harness_appkit_foreground_single_click_has_one_ordered_native_pair() {
    let case = native_foreground_case(
        "appkit",
        "single_click_native_pair",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
    );
    execute_case(case, |evidence| {
        let mut driver =
            McpDriver::spawn_macos_daemon_proxy_named("appkit-single-click-native-pair")
                .expect("start macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());
        let directory = tempfile::tempdir().expect("create native pointer journal directory");
        let journal = directory.path().join("pointer.jsonl");
        std::fs::write(&journal, "").expect("initialize native pointer journal");
        let harness = Harness::launch_with_oracles(None, Some(&journal));
        let (wid, _) = driver
            .find_window(harness.pid as i64, "CuaTestHarness AppKit")
            .expect("find native receiver window");
        driver.start_behavior_recording();
        let read_events = || -> Vec<serde_json::Value> {
            std::fs::read_to_string(&journal)
                .expect("read native pointer journal")
                .lines()
                .map(|line| serde_json::from_str(line).expect("parse native pointer event"))
                .collect()
        };
        let initial = read_events();
        assert_eq!(
            initial.len(),
            1,
            "receiver must be idle before the request: {initial:?}"
        );
        assert_eq!(initial[0]["kind"], "ready");
        assert_eq!(initial[0]["window_id"].as_u64(), Some(wid));
        let snapshot = snapshot_elements(&mut driver, harness.pid, wid);
        assert!(
            !snapshot.is_error(),
            "capture receiver: {}",
            snapshot.text()
        );
        let width = snapshot.structured()["screenshot_width"]
            .as_f64()
            .expect("screenshot width");
        let height = snapshot.structured()["screenshot_height"]
            .as_f64()
            .expect("screenshot height");
        assert!(width > 0.0 && height > 0.0);
        let response = driver.call(
            "click",
            serde_json::json!({
                "pid": harness.pid,
                "window_id": wid,
                "x": width / 2.0,
                "y": height / 2.0,
                "count": 1,
                "delivery_mode": "foreground"
            }),
        );
        assert!(
            !response.is_error(),
            "single click request failed: {}",
            response.text()
        );
        std::thread::sleep(Duration::from_millis(750));
        let events = read_events();
        let received = &events[1..];
        assert_eq!(
        received.len(),
        2,
        "one request must deliver one native down/up pair: {received:?}; receiver={initial:?}; screenshot={width}x{height}"
    );
        assert_eq!(received[0]["kind"], "down");
        assert_eq!(received[1]["kind"], "up");
        let expected_x = initial[0]["width"].as_f64().unwrap() / 2.0;
        let expected_y = initial[0]["height"].as_f64().unwrap() / 2.0;
        for event in received {
            assert_eq!(event["window_id"].as_u64(), Some(wid));
            assert_eq!(event["click_count"], 1);
            assert!(
                (event["x"].as_f64().unwrap() - expected_x).abs() <= 1.0,
                "wrong horizontal target: {event}"
            );
            assert!(
                (event["y"].as_f64().unwrap() - expected_y).abs() <= 1.0,
                "wrong vertical target: {event}"
            );
        }
        assert!(
            received[0]["timestamp"].as_f64().unwrap()
                <= received[1]["timestamp"].as_f64().unwrap()
        );
        println!("native pointer events: {received:?}");
        Observation::delivered_with_fixture_state(vec![])
    });
}

#[test]
#[ignore]
fn harness_appkit_smoke() {
    run_case(
        native_readonly_case(
            "appkit",
            "ax_tree",
            Targeting::Ax,
            DriverRoute::AxRead,
            vec![OracleKind::AxState],
        ),
        |pid, wid, driver| {
            let snap = snapshot_elements(driver, pid, wid);

            assert!(
                !looks_empty(snap.tree_text()),
                "required AppKit AX tree is empty"
            );

            let text = snap.tree_text();
            println!("snapshot:\n{text}");

            // AppKit AX quirk (mirrors the WPF behavior documented in
            // harness_wpf_test.rs::harness_wpf_smoke): NSTextField in label mode
            // and other AXStaticText leaves do NOT propagate
            // setAccessibilityIdentifier into the AX tree's identifier slot, so
            // we don't assert on ids for labels. We assert on text-presence for
            // those, and on AX ids only for actionable controls whose AppKit
            // identifiers are actually propagated (Buttons and TextFields).
            // NSMenuItem behaves like the static leaves here: its title is
            // exposed, but setAccessibilityIdentifier is not.
            for aid in [
                "wnd-main", // NSWindow
                "btn-increment",
                "btn-reset", // NSButton
                "txt-input", // editable NSTextField
                "btn-exit",
            ] {
                assert!(
                    has_id(snap.tree_text(), aid),
                    "missing AX identifier {aid} in AppKit snapshot"
                );
            }

            // text_body marker carried by the visible string of the NSTextField
            assert!(
                text.contains("HARNESS_TEXT_MARKER_v1"),
                "text_body marker not in AppKit snapshot"
            );
            // The two label-mode NSTextFields under click_target render as
            // AXStaticText nodes — assert on their starting text instead of ids.
            assert!(text.contains("counter=0"), "counter label missing");
            assert!(text.contains("clicks=0"), "click_count label missing");
            assert!(
                text.contains("Harness Test Item"),
                "AppKit menu item title missing"
            );
            assert!(
                text.contains("last_action=none"),
                "last_action label missing"
            );
            Observation::delivered(vec![OracleKind::AxState], Evidence::default())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_query_projects_structured_elements() {
    run_case(
        native_readonly_case(
            "appkit",
            "query_projection",
            Targeting::Ax,
            DriverRoute::AxRead,
            vec![OracleKind::AxState],
        ),
        |pid, wid, driver| {
            let response = driver.call(
                "get_window_state",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "query": "btn-increment",
                    "include_screenshot": false
                }),
            );
            assert!(
                !response.is_error(),
                "query snapshot failed: {}",
                response.text()
            );
            let total = response.structured()["total_element_count"]
                .as_u64()
                .expect("total_element_count");
            let returned = response.structured()["returned_element_count"]
                .as_u64()
                .expect("returned_element_count");
            let elements = response.structured()["elements"]
                .as_array()
                .expect("projected elements");
            assert_eq!(returned as usize, elements.len());
            assert!(
                returned < total,
                "query did not compact {returned}/{total} elements"
            );
            assert!(has_id(response.tree_text(), "btn-increment"));
            let _ = element_token_by_id(&response, "btn-increment");
            Observation::delivered(vec![OracleKind::AxState], Evidence::default())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_stale_element_token_fails_closed() {
    run_case(
        native_readonly_case(
            "appkit",
            "stale_element_token",
            Targeting::Ax,
            DriverRoute::AxRead,
            vec![OracleKind::AxState],
        ),
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            assert!(first.tree_text().contains("counter=0"));
            let token = element_token_by_id(&first, "btn-increment");
            let index = element_index_by_id(first.tree_text(), "btn-increment").unwrap();
            let newer = snapshot_elements(driver, pid, wid);
            assert!(
                !newer.is_error(),
                "replacement read failed: {}",
                newer.text()
            );
            assert_ne!(first.snapshot_id(), newer.snapshot_id());
            let refused = driver.call(
                "click",
                serde_json::json!({"pid": pid as i64, "element_token": token}),
            );
            assert!(
                refused.is_error(),
                "stale token was accepted: {}",
                refused.text()
            );
            assert_eq!(
                refused.structured()["refusal"]["code"].as_str(),
                Some("stale_element_token")
            );
            let refused_index = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "snapshot_id": first.snapshot_id(),
                    "element_index": index
                }),
            );
            assert!(
                refused_index.is_error(),
                "stale snapshot/index was accepted"
            );
            assert_eq!(
                refused_index.structured()["refusal"]["code"].as_str(),
                Some("stale_element_token")
            );
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("counter=0"),
                "stale targeting mutated counter"
            );
            let fresh_token = element_token_by_id(&post, "btn-increment");
            let delivered = driver.call(
                "click",
                serde_json::json!({"pid": pid as i64, "element_token": fresh_token}),
            );
            assert!(
                !delivered.is_error(),
                "fresh recovery failed: {}",
                delivered.text()
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let recovered = snapshot_elements(driver, pid, wid);
                if recovered.tree_text().contains("counter=1") {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fresh recovery did not increment exactly once: {}",
                    recovered.tree_text()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            Observation::delivered(vec![OracleKind::AxState], Evidence::default())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_invoke_menu_live_path() {
    run_case(
        native_foreground_case(
            "appkit",
            "invoke_menu",
            Targeting::Ax,
            DriverRoute::MacosAxAction,
        ),
        |pid, wid, driver| {
            let refused = driver.call(
                "invoke_menu",
                serde_json::json!({
                    "pid": pid,
                    "window_id": wid,
                    "path": ["Window", "Arrange", "Missing"]
                }),
            );
            assert!(refused.is_error(), "missing menu path was accepted");
            assert!(snapshot_elements(driver, pid, wid)
                .tree_text()
                .contains("menu_action=none"));

            // A second native process deliberately steals AppKit activation
            // and key-window status. The target menu item validates against
            // both, so an AXFocused-only implementation cannot pass this cell.
            let _distractor = Harness::launch();

            let invoked = driver.call(
                "invoke_menu",
                serde_json::json!({
                    "pid": pid,
                    "window_id": wid,
                    "path": ["Window", "Arrange", "Left"]
                }),
            );
            assert!(
                !invoked.is_error(),
                "invoke_menu failed: {}",
                invoked.text()
            );
            assert_eq!(invoked.action_effect(), Some("unverifiable"));
            std::thread::sleep(Duration::from_millis(300));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("menu_action=window_arrange_left"),
                "menu action did not reach fixture: {}",
                post.tree_text()
            );
            Observation::delivered(vec![OracleKind::FixtureState], Evidence::default())
        },
    );
}

/// text_input: type_text into the NSTextField, verify the mirror label
/// shows the typed string. Exercises the AX type_text path
/// (AXSetAttribute on AXValue, or CGEvent fallback).
#[test]
#[ignore]
fn harness_appkit_text_input() {
    run_background_case(
        "set_value",
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            assert!(
                !looks_empty(snap_pre.tree_text()),
                "required AppKit AX tree is empty"
            );
            let idx = element_index_by_id(snap_pre.tree_text(), "txt-input")
                .expect("txt-input element_index not found");

            // set_value via AX is the deterministic background path; type_text would
            // also work but races with cursor focus on cold-launched windows.
            let resp = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id(),
                    "value": "hello-cua"
                }),
            );
            assert!(!resp.is_error(), "AppKit set_value failed: {}", resp.text());
            println!("set_value resp: {}", resp.text());

            std::thread::sleep(Duration::from_millis(250));
            let snap_post = snapshot_elements(driver, pid, wid);
            let post_text = snap_post.tree_text().to_owned();
            assert!(
                post_text.contains("hello-cua"),
                "text_input value did not propagate to mirror; snapshot:\n{post_text}"
            );

            // Exercise the real AX walk, not just the structured serializer.
            // Empty/whitespace AXValue used to become the field's placeholder.
            for raw in ["", "\n", " \tΩ café\n"] {
                let before = snapshot_elements(driver, pid, wid);
                let set = driver.call(
                    "set_value",
                    serde_json::json!({
                        "pid": pid as i64,
                        "window_id": wid,
                        "element_token": element_token_by_id(&before, "txt-input"),
                        "value": raw
                    }),
                );
                assert!(!set.is_error(), "set_value failed: {}", set.text());
                let after = snapshot_elements(driver, pid, wid);
                let index = element_index_by_id(after.tree_text(), "txt-input")
                    .expect("txt-input remains addressable");
                let field = after.structured()["elements"]
                    .as_array()
                    .and_then(|elements| {
                        elements
                            .iter()
                            .find(|element| element["element_index"].as_u64() == Some(index))
                    })
                    .expect("txt-input structured state");
                assert_eq!(field["value"], raw, "AXValue must remain lossless");
                assert_eq!(field["placeholder"], "Type here…");
            }
        },
    );
}

/// set_value must reach the app's own editing pipeline, not just the AX tree.
/// The fixture publishes `committed=<value>` from `controlTextDidEndEditing`
/// and mirrors `controlTextDidChange` into `lbl-input-mirror`. AppKit raises
/// neither notification for a programmatic `setStringValue:`, so the mirror is
/// what separates a write the editor processed from an `AXValue` echo.
#[test]
#[ignore]
fn harness_appkit_set_value_commits_the_edit() {
    run_background_case(
        "set_value_commit",
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before.tree_text().contains("committed=none"),
                "fixture did not start uncommitted:\n{}",
                before.tree_text()
            );
            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "txt-input"),
                    "value": "commit-cua"
                }),
            );
            assert!(!set.is_error(), "set_value failed: {}", set.text());
            assert_eq!(
                set.structured()["committed"],
                serde_json::json!("committed"),
                "set_value did not report a committed write: {}",
                set.raw
            );
            assert_eq!(
                set.action_route(),
                Some("synthetic_events"),
                "a bound field must be written through the keystroke rung: {}",
                set.raw
            );

            std::thread::sleep(Duration::from_millis(250));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after.tree_text().contains("committed=commit-cua"),
                "the app never registered the write:\n{}",
                after.tree_text()
            );
            // Labels carry no accessibility identifier in the published tree,
            // so the mirror is read as the static-text row holding the typed
            // value. `committed=commit-cua` is the commit label; a bare
            // `commit-cua` static text can only be the mirror.
            assert!(
                after.tree_text().contains("AXStaticText = \"commit-cua\""),
                "controlTextDidChange never fired, so the value was echoed rather than typed:\n{}",
                after.tree_text()
            );
        },
    );
}

/// `Not committed` makes one claim — the application put its own value back —
/// so only the read-back that shows that may carry it. `txt-reformat` keeps the
/// edit and rewrites it in `controlTextDidEndEditing` (Contacts' phone field
/// does the same, `555-789-0123` → `(555) 789-0123`), which left three bench
/// writes reported as lost while the same reply's tree printed the value.
#[test]
#[ignore]
fn harness_appkit_set_value_on_a_reformatting_field_is_unproven() {
    run_background_case(
        "set_value_reformat",
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before.tree_text().contains("reformat_committed=none"),
                "fixture did not start uncommitted:\n{}",
                before.tree_text()
            );
            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "txt-reformat"),
                    "value": "ramp"
                }),
            );
            assert!(!set.is_error(), "set_value failed: {}", set.text());
            assert_eq!(
                set.structured()["committed"],
                serde_json::json!("unproven"),
                "a kept-and-rewritten value was reported as the app's own: {}",
                set.raw
            );
            assert!(
                set.text().contains("reads back as \"[ramp]\""),
                "the reply did not quote what the control now holds: {}",
                set.text()
            );
            assert!(
                !set.text().contains("Not committed"),
                "the reply still claims the app kept its own value: {}",
                set.text()
            );

            std::thread::sleep(Duration::from_millis(250));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after.tree_text().contains("reformat_committed=[ramp]"),
                "the app never registered the write:\n{}",
                after.tree_text()
            );
        },
    );
}

/// An app may replace the control itself when the edit session ends, leaving
/// the `AXUIElementRef` the call addressed answering nothing — Contacts'
/// card editor does it, and the read-back on the retained pointer then missed
/// a value the app had kept. `txt-swap` re-creates itself in the same place,
/// with the same role and label, so the write is judged on the control that
/// now exists.
#[test]
#[ignore]
fn harness_appkit_set_value_follows_a_field_its_app_re_creates() {
    run_background_case(
        "set_value_field_swap",
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before.tree_text().contains("swap_committed=none swaps=0"),
                "fixture did not start unswapped:\n{}",
                before.tree_text()
            );
            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "txt-swap"),
                    "value": "ramp"
                }),
            );
            assert!(!set.is_error(), "set_value failed: {}", set.text());
            assert_eq!(
                set.structured()["committed"],
                serde_json::json!("committed"),
                "the re-created control was not read back: {}",
                set.raw
            );

            std::thread::sleep(Duration::from_millis(250));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after.tree_text().contains("swap_committed=ramp swaps=1"),
                "the app never registered the write, or never swapped:\n{}",
                after.tree_text()
            );
        },
    );
}

/// The other app: `txt-discard` puts its own value back on end-of-edit, which
/// is exactly what `not committed` claims — so that verdict and that sentence
/// stay.
#[test]
#[ignore]
fn harness_appkit_set_value_reports_a_discarded_edit_as_not_committed() {
    run_background_case(
        "set_value_discarded",
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "txt-discard"),
                    "value": "ramp"
                }),
            );
            assert!(!set.is_error(), "set_value failed: {}", set.text());
            assert_eq!(
                set.structured()["committed"],
                serde_json::json!("not_committed"),
                "a discarded edit was not reported as lost: {}",
                set.raw
            );
            assert!(
                set.text().contains("still holds its own value"),
                "the reply did not say what happened: {}",
                set.text()
            );

            std::thread::sleep(Duration::from_millis(250));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after.tree_text().contains("discard_committed=keep-me"),
                "the fixture did not keep its own value:\n{}",
                after.tree_text()
            );
        },
    );
}

/// A control that advertises `AXConfirm` and acts on it is written through that
/// action, not through keystrokes: `confirm_typed=0` is the fixture saying no
/// field editor ever saw a character, and `confirm_committed=` is the app's own
/// handler having run. A plain bound field keeps the typed route whatever it
/// advertises — measured on Automator's "Save as:" parameter, where the
/// value+confirm route left the app holding its own value.
#[test]
#[ignore]
fn harness_appkit_set_value_uses_an_advertised_confirm_as_the_commit() {
    run_background_case(
        "set_value_advertised_confirm",
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before
                    .tree_text()
                    .contains("confirm_committed=none confirm_typed=0"),
                "fixture did not start uncommitted:\n{}",
                before.tree_text()
            );
            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "txt-confirm"),
                    "value": "ramp"
                }),
            );
            assert!(!set.is_error(), "set_value failed: {}", set.text());
            assert_eq!(
                set.action_route(),
                Some("accessibility"),
                "a control whose confirm is its commit must not be typed into: {}",
                set.raw
            );
            assert_eq!(
                set.structured()["committed"],
                serde_json::json!("committed"),
                "the control's own confirm action ran and was not credited: {}",
                set.raw
            );

            std::thread::sleep(Duration::from_millis(250));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after
                    .tree_text()
                    .contains("confirm_committed=ramp confirm_typed=0"),
                "the confirm handler did not run, or keystrokes were sent:\n{}",
                after.tree_text()
            );
        },
    );
}

/// `AXSelectedTextRange` is in UTF-16 units, as `NSString` is. `txt-nonbmp`
/// starts out holding `😀AB` — four units in three characters — so a selection
/// measured in characters covers three of them and the replacement is appended
/// to the "B" left behind.
#[test]
#[ignore]
fn harness_appkit_set_value_replaces_a_non_bmp_value_whole() {
    run_background_case(
        "set_value_non_bmp",
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "txt-nonbmp"),
                    "value": "zed"
                }),
            );
            assert!(!set.is_error(), "set_value failed: {}", set.text());
            assert_eq!(
                set.structured()["committed"],
                serde_json::json!("committed"),
                "the replacement did not cover the whole value: {}",
                set.raw
            );

            std::thread::sleep(Duration::from_millis(250));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                !after.tree_text().contains("nonbmp_committed=zedB"),
                "the selection stopped one UTF-16 unit short:\n{}",
                after.tree_text()
            );
            assert!(
                after.tree_text().contains("nonbmp_committed=zed"),
                "the app never registered the write:\n{}",
                after.tree_text()
            );
        },
    );
}

/// `press_key` on the foreground rung built its events with a default source
/// and no flags, so a chord's base key arrived bare: `cmd+a` typed a literal
/// `a`. The fixture's accelerator requires the modifiers to be present on the
/// event itself (`flags.contains([.control, .shift])`).
#[test]
#[ignore]
fn harness_appkit_foreground_press_key_chord_carries_its_modifiers() {
    run_case(
        native_foreground_case(
            "appkit",
            "press_key_chord",
            Targeting::Ax,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before.tree_text().contains("accel_fired=0"),
                "fixture did not start with an unfired accelerator:\n{}",
                before.tree_text()
            );
            let chord = driver.call(
                "press_key",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "key": "k",
                    "modifiers": ["ctrl", "shift"],
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !chord.is_error(),
                "foreground press_key chord failed: {}",
                chord.text()
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let after = snapshot_elements(driver, pid, wid);
                if after.tree_text().contains("accel_fired=1") {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "ctrl+shift+k arrived without its modifiers:\n{}",
                    after.tree_text()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

/// A text control does not advertise `AXPress`, so a click used to dispatch
/// one anyway and report `-25206` plus "Action may have been a no-op" on the
/// route that works. A click on a text role means "put the caret here": the
/// proof is that the next unaddressed `type_text` lands in that field.
#[test]
#[ignore]
fn harness_appkit_click_on_a_text_role_focuses_it() {
    run_background_case(
        "click_text_focus",
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let clicked = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "txt-input")
                }),
            );
            assert!(!clicked.is_error(), "click failed: {}", clicked.text());
            assert!(
                !clicked.text().contains("does not advertise"),
                "a text role still had an AXPress dispatched at it: {}",
                clicked.text()
            );
            assert_eq!(
                clicked.action_effect(),
                Some("confirmed"),
                "focusing a text control is read-back verifiable: {}",
                clicked.raw
            );

            let typed = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "text": "focus-cua"
                }),
            );
            assert!(!typed.is_error(), "type_text failed: {}", typed.text());
            std::thread::sleep(Duration::from_millis(250));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after.tree_text().contains("focus-cua"),
                "the click did not leave the field focused:\n{}",
                after.tree_text()
            );
        },
    );
}

/// A collection row that advertises `AXPress` used to have that press
/// dispatched by a plain `click`, so Reminders completed a reminder where
/// every other app selected a row. A click on a row means select; the row's
/// own default action stays reachable through a named `action:"press"`.
#[test]
#[ignore]
fn harness_appkit_click_on_a_selectable_row_selects_without_pressing() {
    run_background_case(
        "click_selectable_row",
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before
                    .tree_text()
                    .contains("row_pressed=0 row_selected=false"),
                "pressable row did not start unselected and unpressed:\n{}",
                before.tree_text()
            );
            let clicked = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "row-pressable")
                }),
            );
            assert!(!clicked.is_error(), "click failed: {}", clicked.text());
            assert!(
                clicked.text().contains("Selected nearest AXRow"),
                "a plain click on a selectable row did not take the select route: {}",
                clicked.text()
            );
            assert!(
                clicked.text().contains("AXRow \"pressable row\""),
                "the reply did not name the row it acted on: {}",
                clicked.text()
            );
            assert!(
                clicked.text().contains("perform(\"press\")"),
                "the reply did not name the row's own press action: {}",
                clicked.text()
            );
            assert_eq!(
                clicked.action_effect(),
                Some("confirmed"),
                "selecting a row is read-back verifiable: {}",
                clicked.raw
            );
            std::thread::sleep(Duration::from_millis(300));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after
                    .tree_text()
                    .contains("row_pressed=0 row_selected=true"),
                "the click pressed the row instead of selecting it:\n{}",
                after.tree_text()
            );

            let pressed = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&after, "row-pressable"),
                    "action": "press"
                }),
            );
            assert!(!pressed.is_error(), "press failed: {}", pressed.text());
            std::thread::sleep(Duration::from_millis(300));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("row_pressed=1"),
                "a named press did not reach the row's own action:\n{}",
                post.tree_text()
            );
        },
    );
}

/// `AXScrollArea`/`AXGroup` collapsed before the walk read the node's
/// title, description, help or actions, so a group carrying the app's own
/// explanation of a row — Reminders' `help="To mark as completed, press
/// Control-Option-Space."` — never reached a caller and could not be
/// addressed. The collapse now waits until the node is known to hold nothing.
#[test]
#[ignore]
fn harness_appkit_a_content_bearing_group_stays_addressable() {
    run_background_case(
        "group_with_help",
        DriverRoute::MacosAxAction,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before
                    .tree_text()
                    .contains("help=\"To mark the row as done, press Control-Option-Space.\""),
                "the group's own help never reached the tree:\n{}",
                before.tree_text()
            );
            let pressed = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "grp-row-help"),
                    "action": "press"
                }),
            );
            assert!(
                !pressed.is_error(),
                "group press failed: {}",
                pressed.text()
            );
            std::thread::sleep(Duration::from_millis(300));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after.tree_text().contains("group_pressed=1"),
                "the collapsed group was not addressable:\n{}",
                after.tree_text()
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_element_foreground_press_key_commits_edit() {
    run_case(
        native_foreground_case(
            "appkit",
            "press_key_commit",
            Targeting::Ax,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            let field = element_token_by_id(&first, "txt-input");
            let typed = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": field,
                    "text": "inline-cua",
                    "delivery_mode": "foreground"
                }),
            );
            assert!(!typed.is_error(), "type_text failed: {}", typed.text());
            // The fixture is still `committed=none` at this point, so a
            // read-back that shows the text must not read as an accepted
            // value: type_text delivers no end-of-edit.
            assert_eq!(
                typed.structured()["committed"],
                serde_json::json!("unproven"),
                "type_text implied the app had taken the value: {}",
                typed.raw
            );

            let second = snapshot_elements(driver, pid, wid);
            assert!(
                second.tree_text().contains("inline-cua"),
                "transient edit value was not readable:\n{}",
                second.tree_text()
            );
            assert!(
                second.tree_text().contains("committed=none"),
                "fixture reported a commit before Return:\n{}",
                second.tree_text()
            );
            let field = element_token_by_id(&second, "txt-input");
            let commit = driver.call(
                "press_key",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": field,
                    "key": "return",
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !commit.is_error(),
                "foreground element press_key failed: {}",
                commit.text()
            );
            assert_eq!(
                commit.action_route(),
                Some("global_input"),
                "foreground press_key used the wrong public route: {}",
                commit.raw
            );
            assert_eq!(
                commit.action_delivery_mode(),
                Some("foreground"),
                "foreground press_key reported the wrong delivery: {}",
                commit.raw
            );
            assert_eq!(
                commit.action_effect(),
                Some("unverifiable"),
                "press_key claimed more truth than the tool itself observed: {}",
                commit.raw
            );

            std::thread::sleep(Duration::from_millis(250));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("committed=inline-cua"),
                "Return did not commit the addressed edit:\n{}",
                post.tree_text()
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_px_background_press_key_reports_honest_delivery_truth() {
    let case = native_background_case(
        "appkit",
        "press_key_command",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
    );
    let cell_id = case.cell_id.clone();
    execute_case(case, |evidence| {
        let mut driver = McpDriver::spawn_macos_daemon_proxy_named(&cell_id)
            .expect("start installed macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());
        let oracle_dir = tempfile::tempdir().expect("create command oracle directory");
        let oracle_path = oracle_dir.path().join("child-process-output.txt");
        let harness = Harness::launch_with_command_oracle(Some(&oracle_path));
        let (wid, _) = driver
            .find_window(harness.pid as i64, "CuaTestHarness AppKit")
            .expect("AppKit main window not found");

        let (_, passed) = run_with_background_oracles(
            &mut driver,
            TargetWindow {
                pid: harness.pid,
                native_id: wid,
            },
            |driver| {
                let first = snapshot_elements(driver, harness.pid, wid);
                let field = element_token_by_id(&first, "txt-input");
                let set = driver.call(
                    "set_value",
                    serde_json::json!({
                        "pid": harness.pid as i64,
                        "window_id": wid,
                        "element_token": field,
                        "value": "printf cua-press-key"
                    }),
                );
                assert!(!set.is_error(), "set command failed: {}", set.text());

                let focused = snapshot_elements(driver, harness.pid, wid);
                let (x, y, width, height) = element_pixel_frame(&focused, "txt-input");
                let pressed = driver.call(
                    "press_key",
                    serde_json::json!({
                        "pid": harness.pid as i64,
                        "window_id": wid,
                        "x": x + width / 2.0,
                        "y": y + height / 2.0,
                        "key": "return",
                        "delivery_mode": "background"
                    }),
                );
                assert!(
                    !pressed.is_error(),
                    "background Return failed: {}",
                    pressed.text()
                );
                assert_eq!(pressed.action_route(), Some("synthetic_events"));
                assert_eq!(pressed.action_delivery_mode(), Some("background"));
                assert_eq!(pressed.action_effect(), Some("unverifiable"));
                assert!(
                    pressed.structured()["escalation"].is_null(),
                    "accepted post without a positive oracle must not claim delivery_failed: {}",
                    pressed.raw
                );

                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                loop {
                    if std::fs::read_to_string(&oracle_path)
                        .is_ok_and(|value| value == "cua-press-key")
                    {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "background Return did not execute the controlled child process"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }

                let mut exited = Command::new("/usr/bin/true")
                    .spawn()
                    .expect("spawn posting-failure fixture");
                let exited_pid = exited.id();
                exited.wait().expect("wait for posting-failure fixture");
                let failed = driver.call(
                    "press_key",
                    serde_json::json!({
                        "pid": exited_pid,
                        // An explicit target bypasses the PID-only window resolver so
                        // this negative oracle reaches the posting preflight. Without
                        // one, the earlier and equally truthful result is
                        // window_target_not_found because /usr/bin/true owns no window.
                        "window_id": wid,
                        "key": "return",
                        "delivery_mode": "background"
                    }),
                );
                assert!(failed.is_error(), "dead-pid post unexpectedly succeeded");
                assert_eq!(failed.structured()["code"], "delivery_failed");
            },
        )
        .unwrap_or_else(|error| panic!("background desktop contract failed: {error}"));

        Observation::delivered_with_fixture_state(passed)
    });
}

#[test]
#[ignore]
fn harness_appkit_modified_click_preserves_selection() {
    run_case(
        native_foreground_case(
            "appkit",
            "modified_click_selection",
            Targeting::Ax,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            let alpha = element_token_by_id(&first, "selection-alpha");
            let select_alpha = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": alpha
                }),
            );
            assert!(
                !select_alpha.is_error(),
                "select alpha failed: {}",
                select_alpha.text()
            );
            std::thread::sleep(Duration::from_millis(200));

            let second = snapshot_elements(driver, pid, wid);
            assert!(
                second.tree_text().contains("selection=alpha"),
                "alpha was not selected:\n{}",
                second.tree_text()
            );
            let beta = element_token_by_id(&second, "selection-beta");
            let refused_background = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": beta,
                    "modifier": ["cmd"]
                }),
            );
            assert!(
                refused_background.is_error(),
                "background modified click was not refused: {}",
                refused_background.text()
            );
            assert_eq!(
                refused_background.structured()["code"],
                "background_unavailable",
                "background modified click returned the wrong refusal: {}",
                refused_background.structured()
            );
            std::thread::sleep(Duration::from_millis(300));
            let after_refusal = snapshot_elements(driver, pid, wid);
            assert!(
                after_refusal.tree_text().contains("selection=alpha"),
                "refused modified click changed the prior selection:\n{}",
                after_refusal.tree_text()
            );

            let beta = element_token_by_id(&after_refusal, "selection-beta");
            let add_beta = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": beta,
                    "modifier": ["cmd"],
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !add_beta.is_error(),
                "foreground modified click failed: {}",
                add_beta.text()
            );
            assert_eq!(
                add_beta.structured()["effect"],
                "confirmed",
                "modified click lacked settled selection proof: {}",
                add_beta.structured()
            );

            std::thread::sleep(Duration::from_millis(250));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("selection=alpha,beta"),
                "modified click replaced or lost the prior selection:\n{}",
                post.tree_text()
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

/// type_text: synthesize a keystroke into the NSTextField (CGEvent
/// path, distinct from set_value's AX path). Verifies the keyboard
/// dispatch chain reaches a backgrounded Cocoa text input.
#[test]
#[ignore]
fn harness_appkit_type_text_background() {
    run_background_case(
        "type_text",
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            assert!(
                !looks_empty(snap_pre.tree_text()),
                "required AppKit AX tree is empty"
            );
            let idx = element_index_by_id(snap_pre.tree_text(), "txt-input")
                .expect("txt-input element_index not found");

            // Address the field through type_text itself. AXTextField does not
            // advertise AXPress, so a preparatory click would test an invalid
            // action and fail before the keyboard/value delivery path runs.
            let resp = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid, "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id(),
                    "text": "kbd-cua", "delivery_mode": "background"
                }),
            );
            assert!(!resp.is_error(), "AppKit type_text failed: {}", resp.text());
            println!("type_text resp: {}", resp.text());
            std::thread::sleep(Duration::from_millis(250));

            let snap_post = snapshot_elements(driver, pid, wid);
            let post = snap_post.tree_text().to_owned();
            assert!(
                post.contains("kbd-cua"),
                "type_text keystroke did not land in the text field; snapshot:\n{post}"
            );
        },
    );
}

/// A window-scoped `type_text` must reach the element the target window's
/// focus resolves to, even when the activation installs a different first
/// responder. Measured in Notes: the window remembers its note list, so a
/// foreground `type_text` re-queried focus after the front, typed into the
/// list and read the list back — "Sent (unverified)" over an untouched field.
#[test]
#[ignore]
fn harness_appkit_window_scoped_type_reaches_the_focused_field() {
    run_case_with_env(
        native_foreground_case(
            "appkit",
            "type_text_remembered_responder",
            Targeting::Ax,
            DriverRoute::MacosCgEventPid,
        ),
        &[("CUA_APPKIT_REMEMBERED_RESPONDER", "1")],
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            let focus = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&first, "txt-input")
                }),
            );
            assert!(
                !focus.is_error(),
                "focusing the field failed: {}",
                focus.text()
            );

            // No element_index: the window-scoped form, which is what the
            // remembered responder competes with.
            let typed = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "text": "responder-cua",
                    "delivery_mode": "foreground"
                }),
            );
            assert!(!typed.is_error(), "type_text failed: {}", typed.text());
            assert_eq!(
                typed.action_effect(),
                Some("confirmed"),
                "the window's focused field was not read back: {}",
                typed.raw
            );

            std::thread::sleep(Duration::from_millis(250));
            let post = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                post.contains("responder-cua"),
                "the keystrokes went to the remembered responder:\n{post}"
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

/// A menu key equivalent has to actually reach NSMenu. Measured on Notes,
/// window-scoped `cmd+option+f` (Edit ▸ Find ▸ "Note List Search…"): 0 of 12
/// dispatches landed while the target window was not the app's key window,
/// and every reply still said "Pressed cmd+option+f on pid 91895". The
/// fixture's Window ▸ Arrange ▸ Left owns `cmd+option+l` and publishes
/// `menu_action=window_arrange_left#<firings>`.
///
/// Asserted on the surface a caller actually receives. The platform sets
/// `structured["key_window"]` on every windowed chord, but the published
/// ActionResult is the closed 0.9.0 schema, so a consumer sees only
/// `route` / `delivery` / `effect` / `evidence` / `escalation`: the
/// key-window fact survives as prose and, when the background rung is the
/// one that cannot fix it, as `escalation.reason = route_unavailable`.
/// The not-key arm has no live seam here — the fixture window is key from
/// launch and the app owns no second key-able window — so it stays unit
/// covered by `hotkey::tests::the_chord_reason_is_the_observed_key_window`.
#[test]
#[ignore]
fn harness_appkit_menu_key_equivalent_through_hotkey() {
    run_case(
        native_foreground_case(
            "appkit",
            "hotkey_menu_key_equivalent",
            Targeting::Ax,
            DriverRoute::MacosCgEventPid,
        ),
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before.tree_text().contains("menu_action=none"),
                "fixture did not start with an unfired menu action:\n{}",
                before.tree_text()
            );

            // Background rung: the window is key from launch, so the
            // auth-envelope post reaches NSMenu without any activation.
            let background = driver.call(
                "hotkey",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "keys": ["cmd", "option", "l"]
                }),
            );
            assert!(
                !background.is_error(),
                "background chord failed: {}",
                background.text()
            );
            assert!(
                !key_window_complaint(&background.text()),
                "the window was key, so the reply must not blame a focus holder: {}",
                background.raw
            );
            assert_eq!(
                background.structured()["escalation"],
                serde_json::Value::Null,
                "a chord posted at a key window has no route to escalate to: {}",
                background.raw
            );
            std::thread::sleep(Duration::from_millis(400));
            let after_background = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                after_background.contains("menu_action=window_arrange_left#1"),
                "the background chord never reached NSMenu:\n{after_background}"
            );

            // Foreground rung: the branch that used to front with
            // kCPSNoWindows and post at a window that was never made key.
            let foreground = driver.call(
                "hotkey",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "keys": ["cmd", "option", "l"],
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !foreground.is_error(),
                "foreground chord failed: {}",
                foreground.text()
            );
            assert!(
                !key_window_complaint(&foreground.text()),
                "the foreground rung posted at a window it had not made key: {}",
                foreground.raw
            );
            assert_eq!(
                foreground.structured()["route"],
                serde_json::json!("synthetic_events"),
                "the menu key-equivalent branch is a PID-routed post, not the HID tap: {}",
                foreground.raw
            );
            assert_ne!(
                foreground.action_effect(),
                Some("suspected_noop"),
                "a chord that reached NSMenu was reported as a no-op: {}",
                foreground.raw
            );
            std::thread::sleep(Duration::from_millis(400));
            let after_foreground = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                after_foreground.contains("menu_action=window_arrange_left#2"),
                "the foreground chord never reached NSMenu:\n{after_foreground}"
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

/// The not-key arm, on a live seam: a second window of the same app is key at
/// launch, so `Window > Arrange > Left` validates false for the harness window
/// exactly the way Notes' Edit > Find items do until their window is key
/// (measured: a background chord there landed 0/12). The chord at the not-key
/// window is dispatched as that menu item — window made key, item pressed,
/// prior key window put back — and the reply says so: `route=menu_command`
/// with foreground delivery and the path, never a background chord, and no
/// foreground rung to escalate to. A same-pid sibling window would have made
/// the chord post itself refuse (`same_pid_keyboard_ambiguity`); the menu
/// command is window-exact, so it is not gated by that.
#[test]
#[ignore]
fn harness_appkit_disabled_until_key_chord_lands_as_its_menu_command() {
    run_case_with_env(
        native_foreground_case(
            "appkit",
            "hotkey_menu_command_not_key",
            Targeting::Ax,
            DriverRoute::MacosAxAction,
        ),
        &[("CUA_APPKIT_SECOND_KEY_WINDOW", "1")],
        |pid, wid, driver| {
            let (second, _) = driver
                .find_window(pid as i64, "Second Key Window")
                .expect("second key window not found");
            assert_ne!(second, wid);
            let before = snapshot_elements(driver, pid, wid);
            assert!(
                before.tree_text().contains("menu_action=none"),
                "fixture did not start with an unfired menu action:\n{}",
                before.tree_text()
            );
            let listed = driver.call(
                "invoke_menu",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": second,
                    "path": ["Window", "Arrange"]
                }),
            );
            assert!(
                listed.structured()["items"]
                    .as_array()
                    .is_some_and(|items| items
                        .iter()
                        .any(|item| item["title"] == "Left" && item["enabled"] == false)),
                "the fixture item must read disabled while the second window is key: {}",
                listed.raw
            );

            let reply = driver.call(
                "hotkey",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "keys": ["cmd", "option", "l"]
                }),
            );
            assert!(!reply.is_error(), "menu route failed: {}", reply.text());
            let structured = reply.structured();
            assert_eq!(
                structured["route"],
                serde_json::json!("menu_command"),
                "a chord dispatched as its menu command must say so: {}",
                reply.raw
            );
            assert_eq!(
                structured["delivery"]["mode"],
                serde_json::json!("foreground"),
                "the window was made key for the dispatch; that is never background: {}",
                reply.raw
            );
            assert_eq!(
                structured["menu_path"],
                serde_json::json!(["Window", "Arrange", "Left"]),
                "{}",
                reply.raw
            );
            assert_ne!(
                reply.action_effect(),
                Some("suspected_noop"),
                "the menu command fired and moved the label: {}",
                reply.raw
            );
            assert_eq!(
                structured["escalation"],
                serde_json::Value::Null,
                "a menu command that landed and held has no rung to escalate to: {}",
                reply.raw
            );
            assert!(
                reply.text().contains("Window > Arrange > Left"),
                "the reply names the path it took: {}",
                reply.text()
            );
            assert!(
                reply.text().contains("was not pid")
                    && reply.text().contains("key window")
                    && reply.text().contains("prior frontmost was restored"),
                "the reply says the window was made key and the prior key window put back: {}",
                reply.text()
            );
            assert!(
                !reply.text().contains("not the frontmost application"),
                "the app was already frontmost; the reply must not claim it was fronted: {}",
                reply.text()
            );
            let after = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                after.contains("menu_action=window_arrange_left#1"),
                "the menu command never reached the item:\n{after}"
            );
            let roster = driver.call(
                "list_windows",
                serde_json::json!({ "pid": pid as i64, "include_accessibility_metadata": true }),
            );
            let main_of = |window_id: u64| {
                roster.structured()["accessibility_windows"]["windows"]
                    .as_array()
                    .and_then(|rows| {
                        rows.iter()
                            .find(|row| row["window_id"].as_u64() == Some(window_id))
                    })
                    .map(|row| row["main"].clone())
            };
            assert_eq!(
                main_of(second),
                Some(serde_json::json!(true)),
                "the second window must be the app's main window again after the dispatch: {}",
                roster.raw
            );
            assert_eq!(
                main_of(wid),
                Some(serde_json::json!(false)),
                "the harness window must have given key back: {}",
                roster.raw
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

/// A field whose `AXValue` catches up with the write over the next second is
/// not a partially typed field. The AX rung used to read the value back once,
/// microseconds after the write returned, and published the prefix it caught
/// as `type_text_incomplete` — measured in Contacts as "delivered 6 of 14"
/// for a phone number the card in fact held in full.
#[test]
#[ignore]
fn harness_appkit_type_text_waits_for_a_lagging_value_readback() {
    run_background_case_with_env(
        "type_text_lagging_readback",
        Targeting::Ax,
        DriverRoute::MacosAxValue,
        &[("CUA_APPKIT_AX_VALUE_LAG_MS", "900")],
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            let idx = element_index_by_id(snap_pre.tree_text(), "txt-input")
                .expect("txt-input element_index not found");
            let text = "lagging-readback-cua";
            let resp = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid, "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id(),
                    "text": text, "delivery_mode": "background"
                }),
            );
            assert!(
                !resp.is_error(),
                "a value the field was still publishing was reported as a failure: {}",
                resp.text()
            );
            assert_eq!(
                resp.structured()["delivery"]["delivered_count"],
                serde_json::json!(text.chars().count()),
                "type_text under-counted a complete insertion: {}",
                resp.raw
            );

            std::thread::sleep(Duration::from_millis(1200));
            let post = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                post.contains(text),
                "the fixture never took the whole string:\n{post}"
            );
        },
    );
}

/// An application answers an AX action on its own main loop, so the effect is
/// not in place when `AXUIElementPerformAction` returns. Contacts' toolbar add
/// button opens its menu ~1.3 s later; a probe that sampled once at ~500 ms
/// called that a no-op and escalated to a pixel rung the app ignores.
#[test]
#[ignore]
fn harness_appkit_press_effect_after_the_first_sample_is_not_a_noop() {
    run_background_case_with_env(
        "press_late_effect",
        Targeting::Ax,
        DriverRoute::MacosAxAction,
        &[("CUA_APPKIT_PRESS_LATENCY_MS", "1200")],
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            assert!(
                snap_pre.tree_text().contains("counter=0"),
                "fixture did not start at zero:\n{}",
                snap_pre.tree_text()
            );
            let idx = element_index_by_id(snap_pre.tree_text(), "btn-increment")
                .expect("btn-increment element_index not found");
            let resp = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid, "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id()
                }),
            );
            assert!(!resp.is_error(), "AppKit click failed: {}", resp.text());
            assert_ne!(
                resp.action_effect(),
                Some("suspected_noop"),
                "a press whose effect landed inside the settle budget was called a no-op: {}",
                resp.raw
            );
            assert!(
                resp.text().contains("Delivered:"),
                "the reply must name the reaction it waited for: {}",
                resp.text()
            );
            let post = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                post.contains("counter=1"),
                "the fixture never published the press:\n{post}"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_scroll_foreground() {
    run_case(
        native_foreground_case(
            "appkit",
            "scroll",
            Targeting::Ax,
            DriverRoute::MacosAxAction,
        ),
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            assert!(pre.tree_text().contains("scroll_offset=0"));
            let index = element_index_by_id(pre.tree_text(), "scroll-tall")
                .or_else(|| element_index_containing(pre.tree_text(), "SCROLL_TOP_MARKER_v1"))
                .unwrap_or_else(|| {
                    panic!("scroll-tall element_index not found:\n{}", pre.tree_text())
                });
            let response = driver.call(
                "scroll",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_index": index,
                    "snapshot_id": pre.snapshot_id(),
                    "direction": "down",
                    "amount": 5,
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit foreground scroll failed: {}; raw={}",
                response.text(),
                response.raw
            );
            std::thread::sleep(Duration::from_millis(300));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                !post.tree_text().contains("scroll_offset=0"),
                "AppKit foreground scroll did not move the NSScrollView; response={}; raw={}",
                response.text(),
                response.raw
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_scroll_background() {
    run_background_case("scroll", DriverRoute::MacosAxAction, |pid, wid, driver| {
        let pre = snapshot_elements(driver, pid, wid);
        assert!(pre.tree_text().contains("scroll_offset=0"));
        let index = element_index_by_id(pre.tree_text(), "scroll-tall")
            .or_else(|| element_index_containing(pre.tree_text(), "SCROLL_TOP_MARKER_v1"))
            .unwrap_or_else(|| panic!("scroll-tall element_index not found:\n{}", pre.tree_text()));
        let response = driver.call(
            "scroll",
            serde_json::json!({
                "pid": pid as i64,
                "window_id": wid,
                "element_index": index,
                "snapshot_id": pre.snapshot_id(),
                "direction": "down",
                "amount": 5,
                "delivery_mode": "background"
            }),
        );
        assert!(
            !response.is_error(),
            "AppKit background scroll failed: {}; raw={}",
            response.text(),
            response.raw
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !snapshot_elements(driver, pid, wid)
                .tree_text()
                .contains("scroll_offset=0"),
            "AppKit background AX scroll did not move the NSScrollView"
        );
    });
}

/// counter: click the increment button via element_index, verify the
/// counter label flips from 0 to 1.
#[test]
#[ignore]
fn harness_appkit_counter() {
    run_background_case(
        "left_click",
        DriverRoute::MacosAxAction,
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            assert!(
                !looks_empty(snap_pre.tree_text()),
                "required AppKit AX tree is empty"
            );
            let pre_text = snap_pre.tree_text().to_owned();
            assert!(
                pre_text.contains("counter=0"),
                "counter not 0 pre-click; snapshot:\n{pre_text}"
            );

            let idx = element_index_by_id(snap_pre.tree_text(), "btn-increment")
                .expect("btn-increment element_index not found");

            let click_resp = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id(),
                    "action": "press",
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !click_resp.is_error(),
                "AppKit counter click failed: {}",
                click_resp.text()
            );
            println!("click resp: {}", click_resp.text());

            // Let the AppKit run-loop process the press and refresh the label.
            std::thread::sleep(Duration::from_millis(200));

            let snap_post = snapshot_elements(driver, pid, wid);
            let post_text = snap_post.tree_text().to_owned();
            assert!(
                post_text.contains("counter=1"),
                "counter did not advance to 1 after press; post snapshot:\n{post_text}"
            );
        },
    );
}

/// Resolve the native AppKit button from a screenshot-space PX target, then
/// deliver through the background-safe AX hit-test bridge while another app
/// remains fully foreground.
#[test]
#[ignore]
fn harness_appkit_counter_px_background() {
    run_background_case_targeting(
        "left_click",
        Targeting::Px,
        DriverRoute::MacosAxAction,
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-increment");
            let response = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit PX background click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("counter=1"),
                "AppKit PX background click did not advance counter"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_right_click_px_foreground() {
    run_case(
        native_foreground_case(
            "appkit",
            "right_click",
            Targeting::Px,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let response = driver.call(
                "right_click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit right click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            assert!(
                snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("last_action=right_click"),
                "AppKit right-click handler did not fire"
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_right_click_px_background() {
    run_background_case_targeting(
        "right_click",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let response = driver.call(
                "right_click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit right click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            assert!(
                snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("last_action=right_click"),
                "AppKit background right-click handler did not fire"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_double_click_px_foreground() {
    run_case(
        native_foreground_case(
            "appkit",
            "double_click",
            Targeting::Px,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let response = driver.call(
                "double_click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit double click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            assert!(
                snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("last_action=double_click"),
                "AppKit double-click handler did not fire"
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_double_click_px_background() {
    run_background_case_targeting(
        "double_click",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let response = driver.call(
                "double_click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit double click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            let receiver_snapshot = snapshot_elements(driver, pid, wid);
            let receiver = receiver_snapshot.tree_text();
            assert!(
                receiver.contains("last_action=double_click") && receiver.contains("clicks=2"),
                "AppKit background double-click receiver did not record exactly two clicks"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_slider_drag_px_foreground() {
    run_case(
        native_foreground_case(
            "appkit",
            "slider_drag",
            Targeting::Px,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            assert!(pre.tree_text().contains("slider_value=0"));
            let (x, y, width, height) = element_pixel_frame(&pre, "sld-value");
            let response = driver.call(
                "drag",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "from_x": x + width * 0.05,
                    "from_y": y + height / 2.0,
                    "to_x": x + width * 0.90,
                    "to_y": y + height / 2.0,
                    "duration_ms": 500,
                    "steps": 30,
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit slider drag failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(300));
            assert!(
                !snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("slider_value=0"),
                "AppKit foreground drag did not move the slider"
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_slider_drag_px_background() {
    let case = native_background_case(
        "appkit",
        "slider_drag",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
    )
    .expecting_refusal(vec![RefusalCode::BackgroundUnavailable]);
    run_case(case, |pid, wid, driver| {
        let pre = snapshot_elements(driver, pid, wid);
        assert!(pre.tree_text().contains("slider_value=0"));
        let (x, y, width, height) = element_pixel_frame(&pre, "sld-value");
        let (response, mut passed) = run_with_background_oracles(
            driver,
            TargetWindow {
                pid,
                native_id: wid,
            },
            |driver| {
                driver.call(
                    "drag",
                    serde_json::json!({
                        "pid": pid as i64,
                        "window_id": wid,
                        "from_x": x + width * 0.05,
                        "from_y": y + height / 2.0,
                        "to_x": x + width * 0.90,
                        "to_y": y + height / 2.0,
                        "duration_ms": 500,
                        "steps": 30,
                        "delivery_mode": "background"
                    }),
                )
            },
        )
        .unwrap_or_else(|error| panic!("background desktop contract failed: {error}"));
        assert!(
            response.is_error(),
            "AppKit background drag unexpectedly reported delivery: {}",
            response.text()
        );
        assert_eq!(
            response.structured()["code"].as_str(),
            Some("background_unavailable"),
            "AppKit background drag returned the wrong refusal: {}",
            response.text()
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            snapshot_elements(driver, pid, wid)
                .tree_text()
                .contains("slider_value=0"),
            "refused AppKit background drag changed the slider"
        );
        passed.push(OracleKind::FixtureState);
        Observation::refused(
            RefusalCode::BackgroundUnavailable,
            passed,
            response.text(),
            Evidence::default(),
        )
    });
}

/// A press that opened a menu is delivery.
///
/// The menu an `AXPopUpButton` opens is an accessory window of the process,
/// outside the target window's AX subtree: the button keeps its role, title,
/// value, focus, selection, enablement and frame, the application's focused
/// element does not move, and the window's own subtree digest is unchanged.
/// Every signal the probe had said "nothing reacted", so the driver reported
/// `suspected_noop` and sent the caller to a pixel rung while the menu stood
/// open on screen (calendar-recur omp-1 L66 / omp-2 L60 against the L69/L63
/// screenshots). The reply has to name the reaction, and the observation has
/// to contain the menu.
///
/// The desktop contract here is the sentinel's, not the target's: measured on
/// this fixture, AppKit orders a popup button's window to the front of its
/// layer when its menu opens — with the process still not frontmost — so a
/// window fully covered before the press is visible after it. That is the
/// application's own behaviour on the action, not a leak of the delivery; the
/// process is not activated, the foreground window keeps focus and the cursor
/// does not move, and those are what this case holds.
#[test]
#[ignore]
fn harness_appkit_press_that_opens_a_menu_is_delivery_with_the_menu_in_the_tree() {
    let mut case = native_background_case(
        "appkit",
        "press_opens_menu",
        Targeting::Ax,
        DriverRoute::MacosAxAction,
    );
    case.oracles.retain(|oracle| *oracle != OracleKind::ZOrder);
    run_case_with_env(
        case,
        &[("CUA_APPKIT_MENU_POPOVER", "1")],
        |pid, wid, driver| {
            let target = TargetWindow {
                pid,
                native_id: wid,
            };
            let sentinel = ForegroundSentinel::launch(driver);
            sentinel
                .assert_background_posture(target)
                .unwrap_or_else(|error| panic!("background posture: {error}"));
            driver.start_behavior_recording();
            sentinel
                .prepare_background_observation(driver, target)
                .unwrap_or_else(|error| panic!("background observation: {error}"));
            let (_, passed) = sentinel
                .observe_desktop(|| {
                    let pre = snapshot_elements(driver, pid, wid);
                    let idx =
                        element_index_by_id(pre.tree_text(), "pop-menu").unwrap_or_else(|| {
                            panic!("pop-menu element_index not found:\n{}", pre.tree_text())
                        });
                    // The menu bar's own `AXMenu`s are always in the observation; the
                    // popup's menu is told apart by the items only it carries.
                    assert!(
                        !pre.tree_text().contains("15 minutes before"),
                        "the popup's menu was already open before the press:\n{}",
                        pre.tree_text()
                    );
                    let response = driver.call(
                        "click",
                        serde_json::json!({
                            "pid": pid as i64, "window_id": wid, "element_index": idx,
                            "snapshot_id": pre.snapshot_id()
                        }),
                    );
                    assert!(
                        !response.is_error(),
                        "AppKit click failed: {}",
                        response.text()
                    );
                    assert_ne!(
                        response.action_effect(),
                        Some("suspected_noop"),
                        "a press that opened a menu was reported as a no-op: {}",
                        response.raw
                    );
                    assert!(
                        response.text().contains("Delivered:"),
                        "the reply must name the reaction it observed: {}",
                        response.text()
                    );

                    let post = snapshot_elements(driver, pid, wid).tree_text().to_owned();
                    assert!(
                post.contains("AXMenuItem \"15 minutes before\""),
                "the open menu and its items are missing from the window's observation:\n{post}"
            );
                })
                .unwrap_or_else(|error| panic!("desktop contract failed: {error}"));
            Observation::delivered_with_fixture_state(passed)
        },
    );
}

/// A capture reports the rect its pixels cover.
///
/// ScreenCaptureKit's desktop-independent window filter renders the popovers
/// and menus an application hangs over a window into that window's capture.
/// Sizing the output from the window's own frame made the delivered image a
/// squeezed union that still measured as a clean 1x capture of the window,
/// and the frame was labelled "1 px = 1 window point" while the window was
/// drawn at ~0.77x inside it (calendar-recur omp-1 L69/L78 against the window
/// list at L81). Every coordinate read off such an image lands somewhere else.
#[test]
#[ignore]
fn harness_appkit_capture_with_an_open_popover_reports_the_rect_it_covers() {
    run_background_case_with_env(
        "capture_with_popover",
        Targeting::Ax,
        DriverRoute::MacosAxAction,
        &[("CUA_APPKIT_MENU_POPOVER", "1")],
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let idx = element_index_by_id(pre.tree_text(), "btn-popover").unwrap_or_else(|| {
                panic!("btn-popover element_index not found:\n{}", pre.tree_text())
            });
            let response = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid, "element_index": idx,
                    "snapshot_id": pre.snapshot_id()
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit popover click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(600));

            let shot = driver.call(
                "get_window_state",
                serde_json::json!({ "pid": pid as i64, "window_id": wid }),
            );
            assert!(!shot.is_error(), "window capture failed: {}", shot.text());
            let state = shot.structured();
            let window = state["window_bounds"].clone();
            let content = state["screenshot_content_bounds"].clone();
            assert!(
                content.is_object(),
                "the capture did not say what rect its pixels cover: {}",
                shot.raw
            );
            let number = |value: &serde_json::Value, key: &str| -> f64 {
                value[key]
                    .as_f64()
                    .unwrap_or_else(|| panic!("{key} missing from {value}"))
            };
            let (wx, wy) = (number(&window, "x"), number(&window, "y"));
            let (ww, wh) = (number(&window, "width"), number(&window, "height"));
            let (cx, cy) = (number(&content, "x"), number(&content, "y"));
            let (cw, ch) = (number(&content, "width"), number(&content, "height"));
            assert!(
                cx <= wx + 1.0 && cy <= wy + 1.0 && cy + ch >= wy + wh - 1.0,
                "the captured rect does not contain the window: {content} vs {window}"
            );
            assert!(
                cx + cw > wx + ww + 1.0,
                "the popover hangs past the window's right edge, so the captured rect has to \
                 reach past it too: {content} vs {window}"
            );

            let sw = state["screenshot_width"]
                .as_f64()
                .expect("screenshot_width");
            let sh = state["screenshot_height"]
                .as_f64()
                .expect("screenshot_height");
            let scale_x = sw / cw;
            let scale_y = sh / ch;
            assert!(
                (scale_x - scale_y).abs() <= 0.03,
                "the capture's pixels per point differ by axis ({scale_x} vs {scale_y}), so no \
                 single scale describes the image"
            );
            // The claim every coordinate rests on: the window's own right
            // edge maps strictly inside the image, because the image covers
            // more than the window. Under the old label it sat on the edge.
            let right = (wx + ww - cx) * scale_x;
            assert!(
                right < sw - 1.0,
                "the window's right edge maps to {right} in a {sw}x{sh} image that also holds \
                 the popover"
            );

            // And a control the tree names maps onto the picture through the
            // same two fields.
            let button = element_by_id(&shot, "btn-popover");
            let frame = button["frame"].clone();
            let bx = (number(&frame, "x") + number(&frame, "w") / 2.0 - cx) * scale_x;
            let by = (number(&frame, "y") + number(&frame, "h") / 2.0 - cy) * scale_y;
            assert!(
                bx > 0.0 && bx < sw && by > 0.0 && by < sh,
                "the control's own frame maps to ({bx}, {by}), outside the {sw}x{sh} image the \
                 reply says covers {content}"
            );
        },
    );
}

/// Nothing hanging over the window: the capture covers the window's own
/// frame, which is what every pixel action already rests on. The delivered
/// image may be smaller than the raw capture (the session's long-edge cap),
/// so the claim is about the rect, and about one scale describing both axes
/// — never about `screenshot_scale`, which names the raw backing scale.
#[test]
#[ignore]
fn harness_appkit_a_plain_window_capture_stays_point_for_point() {
    run_case(
        native_readonly_case(
            "appkit",
            "capture_without_popover",
            Targeting::Px,
            DriverRoute::WindowState,
            vec![OracleKind::Pixels],
        ),
        |pid, wid, driver| {
            let shot = driver.call(
                "get_window_state",
                serde_json::json!({ "pid": pid as i64, "window_id": wid }),
            );
            assert!(!shot.is_error(), "window capture failed: {}", shot.text());
            let state = shot.structured();
            let window = state["window_bounds"].clone();
            let content = state["screenshot_content_bounds"].clone();
            for key in ["x", "y", "width", "height"] {
                assert_eq!(
                    content[key], window[key],
                    "a lone window's capture must cover exactly that window: {content} vs {window}"
                );
            }
            let sw = state["screenshot_width"]
                .as_f64()
                .expect("screenshot_width");
            let sh = state["screenshot_height"]
                .as_f64()
                .expect("screenshot_height");
            let scale = state["screenshot_scale"]
                .as_f64()
                .expect("screenshot_scale");
            let width = window["width"].as_f64().expect("window width");
            let height = window["height"].as_f64().expect("window height");
            let (scale_x, scale_y) = (sw / width, sh / height);
            assert!(
                (scale_x - scale_y).abs() <= 0.03,
                "the capture's pixels per point differ by axis ({scale_x} vs {scale_y}), so no \
                 single scale describes the image"
            );
            assert!(
                scale_x <= scale + 0.01,
                "a lone window's capture was upscaled past its {scale}x backing: {sw} px for \
                 {width} pt"
            );
            Observation::delivered(vec![OracleKind::Pixels], Evidence::default())
        },
    );
}

// ── child-window ownership (child_editor scenario) ───────────────────────────
//
// The shape, measured in Finder's inline rename editor: the application edits
// in a separate borderless WindowServer window drawn inside the window being
// edited, accessibility publishes that window as the text field itself (role
// `AXTextField`, mapped by `_AXUIElementGetWindow`, no `AXWindow` /
// `AXTopLevelUIElement` attribute, `AXParent` = the application), and while it
// is up the application answers no `AXFocusedWindow`. Such a surface cannot be
// addressed, activated or made key on its own, so it is the requested window's
// own surface — and making the requested window key is what dismisses it.
//
// The editor is not in the main window's AX subtree, so the fixture publishes
// its state into the main window: `child_editor=open id=<n>` / `closed`,
// `child_value=`, `child_committed=`.

/// The refusal code a reply carries, under either shape the tools publish it
/// in (`code` at the top level, or nested under `refusal`).
fn refusal_code(reply: &ToolResponse) -> Option<&str> {
    let structured = reply.structured();
    structured["code"]
        .as_str()
        .or_else(|| structured["refusal"]["code"].as_str())
}

/// The line the fixture publishes for the editor's own window, so a test can
/// prove the same surface survived rather than a re-created one.
fn child_editor_state(tree: &str) -> String {
    tree.lines()
        .find(|line| line.contains("child_editor="))
        .map(|line| {
            let start = line.find("child_editor=").expect("child_editor= marker");
            let rest = &line[start..];
            rest.split(|c: char| c == '|' || c == '<')
                .next()
                .unwrap_or(rest)
                .trim()
                .to_owned()
        })
        .unwrap_or_else(|| panic!("the fixture published no child_editor= state:\n{tree}"))
}

/// Window-scoped background keys, with the application editing in a child
/// surface of the requested window. The surface is a second same-pid layer-0
/// window that the application also lists in `AXWindows`, so the pre-fix gate
/// counted it as a competing keyboard destination and refused both rungs with
/// `same_pid_keyboard_ambiguity` — refusing to type into the very field the
/// keyboard was already in.
#[test]
#[ignore]
fn harness_appkit_child_editor_takes_window_scoped_background_keys() {
    run_background_case_with_env(
        "child_editor_background_keys",
        Targeting::Ax,
        DriverRoute::MacosCgEventPid,
        &[("CUA_APPKIT_CHILD_EDITOR", "1")],
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let state = child_editor_state(before.tree_text());
            assert!(
                state.starts_with("child_editor=open"),
                "the fixture must launch with its child editor open: {state}"
            );
            assert!(
                has_id(before.tree_text(), "txt-child-editor"),
                "the fixture's child window must be published as a text field in the \
                 application's window list, not as a window — the shape under test is \
                 gone otherwise:\n{}",
                before.tree_text()
            );

            // No element_index: the window-scoped form, which is the one the
            // process-scoped keyboard gate decides.
            let typed = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "text": "editor-cua",
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !typed.is_error(),
                "a background type at a window whose own child surface holds the keyboard \
                 was refused: {} / {}",
                refusal_code(&typed).unwrap_or("no code"),
                typed.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            let mid = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                mid.contains("editor-cua"),
                "the keystrokes did not reach the child editor:\n{mid}"
            );

            let pressed = driver.call(
                "press_key",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "key": "Return",
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !pressed.is_error(),
                "a background Return at the same window was refused: {} / {}",
                refusal_code(&pressed).unwrap_or("no code"),
                pressed.text()
            );
            std::thread::sleep(Duration::from_millis(300));
            let post = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                post.contains("child_committed=editor-cua"),
                "Return did not commit the child editor's text:\n{post}"
            );
        },
    );
}

/// The foreground chord rung must not front the parent over the surface it is
/// aiming at. `with_menu_key_activation` makes the requested window the
/// application's key window, and that is exactly what dismisses an inline
/// editor: measured in Finder, one foreground `cmd+a` at the parent ended the
/// rename outright.
#[test]
#[ignore]
fn harness_appkit_foreground_chord_does_not_dismiss_the_child_editor() {
    run_case_with_env(
        native_foreground_case(
            "appkit",
            "child_editor_foreground_chord",
            Targeting::Ax,
            DriverRoute::MacosCgEventHid,
        ),
        &[("CUA_APPKIT_CHILD_EDITOR", "1")],
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let open = child_editor_state(before.tree_text());
            assert!(
                open.starts_with("child_editor=open"),
                "the fixture must launch with its child editor open: {open}"
            );

            let chord = driver.call(
                "hotkey",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "keys": ["cmd", "a"],
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !chord.is_error(),
                "foreground chord failed: {}",
                chord.text()
            );
            std::thread::sleep(Duration::from_millis(300));
            let after = snapshot_elements(driver, pid, wid);
            assert_eq!(
                child_editor_state(after.tree_text()),
                open,
                "the foreground chord dismissed (or re-created) the application's own \
                 editing surface: {}",
                chord.text()
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

/// An element in a surface the requested window owns is in that window. The
/// editor is a root-level row of the requested window's own snapshot (a
/// non-window top-level child of the application), and its `AXParent` chain
/// reaches the application without passing a window, so the pre-fix ancestry
/// verdict was `Unproven` and the write was refused
/// `element_outside_target_window` — against the one field the prompt tells a
/// caller to address.
#[test]
#[ignore]
fn harness_appkit_set_value_on_the_child_editor_is_inside_its_window() {
    run_background_case_with_env(
        "child_editor_set_value",
        Targeting::Ax,
        DriverRoute::MacosAxValue,
        &[("CUA_APPKIT_CHILD_EDITOR", "1")],
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let editor = element_by_id(&before, "txt-child-editor");
            let index = editor["element_index"]
                .as_u64()
                .expect("child editor element_index");

            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_index": index,
                    "snapshot_id": before.snapshot_id(),
                    "value": "written-cua"
                }),
            );
            assert_ne!(
                refusal_code(&set),
                Some("element_outside_target_window"),
                "the window's own editing surface was refused as outside it: {}",
                set.raw
            );
            assert!(
                !set.is_error(),
                "set_value on the child editor failed: {}",
                set.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            let post = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                post.contains("written-cua"),
                "the write did not land in the child editor:\n{post}"
            );
        },
    );
}

/// The contrast, and the guard the two-window rule exists for: an attached
/// sheet keeps its window role (`AXSheet`), becomes its application's key
/// window and can be addressed on its own, so it is a keyboard destination in
/// its own right. A process-scoped key stays refused while one is up, with the
/// same code and the same sentence as before.
#[test]
#[ignore]
fn harness_appkit_an_attached_sheet_still_competes_for_the_keyboard() {
    run_case_with_env(
        native_readonly_case(
            "appkit",
            "child_editor_sheet_competes",
            Targeting::Ax,
            DriverRoute::AxRead,
            vec![OracleKind::AxState],
        ),
        &[("CUA_APPKIT_CHILD_EDITOR", "sheet")],
        |pid, wid, driver| {
            let (sheet, _) = driver
                .find_window(pid as i64, "Child Editor Sheet")
                .expect("the fixture's attached sheet was not found");
            assert_ne!(sheet, wid);

            let refused = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "text": "sheet-cua",
                    "delivery_mode": "background"
                }),
            );
            assert!(
                refused.is_error(),
                "a process-scoped key was accepted while a sheet held the keyboard: {}",
                refused.text()
            );
            assert_eq!(
                refusal_code(&refused),
                Some("same_pid_keyboard_ambiguity"),
                "wrong refusal for a sibling that can be the key window: {}",
                refused.raw
            );
            assert!(
                refused
                    .text()
                    .contains("other eligible top-level window(s)"),
                "the refusal sentence for a competing destination changed: {}",
                refused.text()
            );
            Observation::delivered(vec![OracleKind::AxState], Evidence::default())
        },
    );
}
