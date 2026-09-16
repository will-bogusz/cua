use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::path::PathBuf;

pub struct LaunchAppTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "launch_app".into(),
        description:
            "Request a non-activating launch for a new macOS app instance, or reuse an already-running instance without changing its visibility.\n\n\
             Provide either `bundle_id` (e.g. `com.apple.calculator`) \
             or `name` (e.g. \"Calculator\" or an exact absolute .app bundle path). If both are given, bundle_id wins. An explicit path is verified against the running bundle; \
             a different running copy returns APP_PATH_CONFLICT without dispatch.\n\n\
             Optional `urls` are handed to the app as open targets — for Finder, pass a folder \
             path to open a backgrounded Finder window there.\n\n\
             Browser DevTools setup belongs to `browser_prepare`, which can prove that a \
             separate isolated profile is driver-owned before enabling CDP.\n\n\
             Optional `webkit_inspector_port`: opens a WebKit inspector server on the specified \
             port (sets WEBKIT_INSPECTOR_SERVER=127.0.0.1:N + TAURI_WEBVIEW_AUTOMATION=1). \
             Use this for Tauri/WebKit-based apps.\n\n\
             Optional `creates_new_application_instance`: when true, forces a new app instance \
             even if one is already running. Use this to request a specific installation when another \
             copy is running. App-specific singleton behavior may still prevent an independent instance; \
             inspect the returned identity and readiness.\n\n\
             Optional `additional_arguments`: extra argv strings appended after --args.\n\n\
             Returns the launched app's pid, bundle_id, name, and a `windows` array \
             (same shape as `list_windows`) so callers can skip an extra round-trip before \
             `get_window_state(pid, window_id)`. `launch_state` distinguishes whether the \
             request was sent, the process is running, and a window is ready. Visibility metadata \
             reports the requested policy and observed active/onscreen state; apps may override \
             launch policy. The driver never restores a previous foreground app or suppresses \
             intentional user activation after launch."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "bundle_id": {
                    "type": "string",
                    "description": "App bundle identifier, e.g. com.apple.calculator. Takes precedence over name; use an explicit name path without bundle_id to select an installation."
                },
                "name": {
                    "type": "string",
                    "description": "App display name or exact absolute .app bundle path (also accepts ~/). Used only when bundle_id is absent."
                },
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file paths or URLs to open with the app (e.g. a folder path for Finder)."
                },
                "webkit_inspector_port": {
                    "type": "integer",
                    "description": "Open a WebKit inspector server on this port (sets WEBKIT_INSPECTOR_SERVER env var)."
                },
                "creates_new_application_instance": {
                    "type": "boolean",
                    "description": "Request a new app instance, including an exact installation when another copy is running. Inspect the returned identity and readiness; application singleton behavior may prevent independent instances."
                },
                "additional_arguments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments appended after --args when launching."
                }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: true,
    })
}

#[async_trait]
impl Tool for LaunchAppTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let bundle_id = args.opt_str("bundle_id");
        let name = args.opt_str("name");
        let mut response_bundle_id = bundle_id.clone();
        let mut application_ref = bundle_id.clone();
        let response_requested_name = name.clone();
        let urls: Vec<String> = args
            .str_array("urls")
            .into_iter()
            .map(normalize_launch_url)
            .collect();
        if args.get("cdp_debugging_port").is_some() {
            return ToolResult::error(
                "cdp_debugging_port moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }
        let webkit_inspector_port = args.opt_u64("webkit_inspector_port").map(|v| v as u16);
        let creates_new_instance = args.bool_or("creates_new_application_instance", false);
        let additional_arguments: Vec<String> = args.str_array("additional_arguments");
        if additional_arguments
            .iter()
            .any(|argument| argument == super::check_permissions::PERMISSIONS_HOST_REQUEST_ARG)
        {
            return protected_host_launch_refusal();
        }
        if additional_arguments
            .iter()
            .any(|argument| contains_remote_debugging_flag(argument))
        {
            return ToolResult::error(
                "Chromium remote-debugging flags moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }

        if bundle_id.is_none() && name.is_none() {
            return ToolResult::error(
                "Provide either bundle_id or name to identify the app to launch.",
            );
        }
        if bundle_id.as_deref().is_some_and(is_cua_driver_bundle_id) {
            return protected_host_launch_refusal();
        }
        if let Some(ref bid) = bundle_id {
            if crate::apps::resolve_bundle_id_to_locator(bid).is_none() {
                return structured_launch_error(
                    "APP_NOT_INSTALLED",
                    format!("No installed macOS app found for bundle_id '{bid}'."),
                    serde_json::json!({ "bundle_id": bid }),
                );
            }
        } else if let Some(ref n) = name {
            let Some(locator) = crate::apps::locate_by_name(n) else {
                return structured_launch_error(
                    "APP_NOT_INSTALLED",
                    format!("No installed macOS app found for name '{n}'."),
                    serde_json::json!({ "name": n }),
                );
            };
            let (resolved_ref, resolved_bundle_id) = locator.app_ref_and_bundle_id();
            application_ref = Some(resolved_ref);
            response_bundle_id = resolved_bundle_id.clone();
            if resolved_bundle_id
                .as_deref()
                .is_some_and(is_cua_driver_bundle_id)
            {
                return protected_host_launch_refusal();
            }
        }
        if let Some(err) = preflight_file_urls(&urls) {
            return err;
        }

        // Build env dict for webkit inspector.
        let mut env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Some(port) = webkit_inspector_port {
            env.insert(
                "WEBKIT_INSPECTOR_SERVER".to_string(),
                format!("127.0.0.1:{port}"),
            );
            env.insert("TAURI_WEBVIEW_AUTOMATION".to_string(), "1".to_string());
        }

        let port_summary = {
            let mut s = String::new();
            if let Some(port) = webkit_inspector_port {
                s.push_str(&format!("\nWebKit inspector available on port {port}."));
            }
            s
        };

        // A background request must not restore an old foreground app after
        // the user switches tasks. New instances request non-activating launch; plain
        // acquisition of a running instance performs no reopen/hide/activation.
        let existing = response_bundle_id
            .as_deref()
            .map(crate::apps::running_apps_for_bundle)
            .unwrap_or_default();
        let exact_path = if bundle_id.is_none()
            && name
                .as_deref()
                .is_some_and(|name| name.starts_with('/') || name.starts_with("~/"))
        {
            match application_ref
                .as_deref()
                .map(std::fs::canonicalize)
                .transpose()
            {
                Ok(path) => path,
                Err(error) => {
                    return structured_launch_error(
                        "APP_PATH_UNAVAILABLE",
                        error.to_string(),
                        serde_json::json!({"path":application_ref,"launch_state":launch_state(false,false,false)}),
                    )
                }
            }
        } else {
            None
        };
        let existing_pids = if creates_new_instance {
            Vec::new()
        } else {
            match matching_running_instances(&existing, exact_path.as_deref()) {
                Ok(pids) => pids,
                Err(reason) => {
                    return structured_launch_error(
                        "APP_PATH_CONFLICT",
                        reason.into(),
                        serde_json::json!({"path":exact_path,"running_instances":existing,"launch_state":launch_state(false,true,false)}),
                    )
                }
            }
        };
        let has_handoff = !urls.is_empty() || !additional_arguments.is_empty() || !env.is_empty();
        // NSWorkspace URL delivery is not PID-addressed. Do not send a file to
        // one of multiple same-bundle instances on the strength of path selection.
        if has_handoff && !creates_new_instance && existing.len() > 1 {
            return structured_launch_error("AMBIGUOUS_APP_HANDOFF",
                "Multiple running copies make URL/argument delivery ambiguous; use the intended window or an explicit new instance.".into(),
                serde_json::json!({"running_instances":existing,"launch_state":launch_state(false,true,false)}));
        }
        let reuse_pid = match launch_plan(&existing_pids, creates_new_instance, has_handoff) {
            Ok(plan) => plan,
            Err(reason) => {
                return structured_launch_error(
                    "AMBIGUOUS_RUNNING_APP",
                    reason.into(),
                    serde_json::json!({"pids":existing_pids,"bundle_id":response_bundle_id}),
                )
            }
        };
        let launch_requested = reuse_pid.is_none();
        let launch_bundle_id = response_bundle_id.clone();
        let launch_result = tokio::task::spawn_blocking(move || {
            let pid = if let Some(pid) = reuse_pid {
                pid
            } else {
                let config = crate::apps::nsworkspace::OpenConfig {
                    // Hidden AppKit panels can accept AX selection but never enable
                    // their commit button. Keep normal window lifecycle without activation.
                    hides: false,
                    arguments: additional_arguments,
                    environment: env,
                    creates_new_instance,
                    apple_event_bundle_id: if urls.is_empty() {
                        launch_bundle_id
                    } else {
                        None
                    },
                };
                let app_ref = application_ref
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("Missing resolved application"))?;
                let running = if urls.is_empty() {
                    crate::apps::nsworkspace::open_application(app_ref, &config)
                } else {
                    crate::apps::nsworkspace::open_urls_with_application(&urls, app_ref, &config)
                }?;
                unsafe { running.processIdentifier() }
            };
            let windows = resolve_windows_for_pid(pid);
            let app_info = crate::apps::running_app_by_pid(pid);
            Ok::<_, anyhow::Error>((pid, app_info, windows))
        })
        .await;

        match launch_result {
            Ok(Ok((pid, app_info, windows))) => {
                if !returned_identity_matches(
                    app_info.as_ref(),
                    response_bundle_id.as_deref(),
                    exact_path.as_deref(),
                ) {
                    return structured_launch_error("LAUNCH_TARGET_CHANGED",
                        format!("App identity for PID {pid} does not confirm the requested bundle/path; no further control was performed."),
                        serde_json::json!({"pid":pid,"requested_bundle_id":response_bundle_id,"requested_path":exact_path,
                            "observed":app_info,"launch_state":launch_state(launch_requested,app_info.is_some(),!windows.is_empty())}));
                }

                let (app_name, bid) = response_identity(
                    app_info.as_ref(),
                    response_bundle_id.as_deref(),
                    response_requested_name.as_deref(),
                );

                let mut summary = if launch_requested {
                    format!(
                        "Requested non-activating launch of {app_name} (pid {pid}).{port_summary}"
                    )
                } else {
                    format!("Using running {app_name} (pid {pid}); visibility unchanged.")
                };

                if !windows.is_empty() {
                    summary.push_str("\n\nWindows:");
                    for w in &windows {
                        let title = if w.title.is_empty() {
                            "(no title)".to_owned()
                        } else {
                            format!("\"{}\"", w.title)
                        };
                        summary.push_str(&format!("\n- {title} [window_id: {}]", w.window_id));
                    }
                    summary.push_str(&format!(
                        "\n→ Call get_window_state(pid: {pid}, window_id) to inspect."
                    ));
                }

                let windows_json: Vec<Value> = windows
                    .iter()
                    .map(|w| super::list_windows::window_record_json(w, None, None))
                    .collect();

                let structured = serde_json::json!({
                    "pid": pid,
                    "bundle_id": bid,
                    "name": app_name,
                    "launch_path": app_info.as_ref().and_then(|app| app.launch_path.as_deref()),
                    "windows": windows_json,
                    "launch_state": launch_state(launch_requested, true, !windows.is_empty()),
                    "visibility": {
                        "requested": if launch_requested {"background"} else {"preserve"},
                        "observed_active": app_info.as_ref().map(|app| app.active),
                        "observed_onscreen_window_ids": windows.iter().filter(|w|w.is_on_screen).map(|w|w.window_id).collect::<Vec<_>>(),
                    },
                });
                ToolResult::text(summary).with_structured(structured)
            }
            Ok(Err(e)) => structured_launch_failure(&e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

fn matching_running_instances(
    existing: &[crate::apps::AppInfo],
    requested_path: Option<&std::path::Path>,
) -> Result<Vec<i32>, &'static str> {
    let Some(path) = requested_path else {
        return Ok(existing.iter().map(|app| app.pid).collect());
    };
    let matches: Vec<i32> = existing
        .iter()
        .filter(|app| running_path_matches(app, path))
        .map(|app| app.pid)
        .collect();
    if !existing.is_empty() && matches.is_empty() {
        return Err("A different or unverified copy of this bundle is running. The exact requested installation was not reused or launched; explicitly request a new instance to launch that copy.");
    }
    Ok(matches)
}

fn running_path_matches(app: &crate::apps::AppInfo, path: &std::path::Path) -> bool {
    app.launch_path
        .as_deref()
        .and_then(|value| std::fs::canonicalize(value).ok())
        .as_deref()
        == Some(path)
}

fn returned_identity_matches(
    app: Option<&crate::apps::AppInfo>,
    bundle_id: Option<&str>,
    path: Option<&std::path::Path>,
) -> bool {
    app.is_some_and(|app| {
        app.running
            && app.pid > 0
            && bundle_id.is_none_or(|bundle| app.bundle_id.as_deref() == Some(bundle))
            && path.is_none_or(|path| running_path_matches(app, path))
    })
}

fn launch_plan(
    existing_pids: &[i32],
    new_instance: bool,
    handoff: bool,
) -> Result<Option<i32>, &'static str> {
    if new_instance || existing_pids.is_empty() {
        return Ok(None);
    }
    if existing_pids.len() != 1 {
        return Err("Multiple running instances; select an exact window/PID or explicitly request a new instance.");
    }
    Ok(if handoff {
        None
    } else {
        Some(existing_pids[0])
    })
}

fn contains_remote_debugging_flag(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("--remote-debugging-port") || lower.contains("--remote-debugging-pipe")
}

fn is_cua_driver_bundle_id(bundle_id: &str) -> bool {
    matches!(bundle_id, "com.trycua.driver" | "com.trycua.driver.local")
}

fn protected_host_launch_refusal() -> ToolResult {
    structured_launch_error(
        "PROTECTED_HOST_ENTRYPOINT",
        "launch_app cannot launch Cua Driver's protected host; operating-system permission UI must originate outside the agent tool stream".to_owned(),
        serde_json::json!({}),
    )
}

// ── Blocking helpers ──────────────────────────────────────────────────────────

/// Bound readiness waiting to five seconds; launching can finish before the UI exists.
fn resolve_windows_for_pid(pid: i32) -> Vec<crate::windows::WindowInfo> {
    for attempt in 0..50 {
        let found: Vec<_> = crate::windows::all_windows()
            .into_iter()
            .filter(|w| w.pid == pid && w.layer == 0)
            .filter(|w| w.bounds.width > 1.0 && w.bounds.height > 1.0)
            .collect();
        if !found.is_empty() {
            return found;
        }
        if attempt < 49 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    vec![]
}

fn structured_launch_error(code: &str, message: String, details: serde_json::Value) -> ToolResult {
    let mut payload = serde_json::json!({
        "error": code,
    });

    match details {
        serde_json::Value::Object(details) => {
            if let serde_json::Value::Object(payload) = &mut payload {
                payload.extend(details);
            }
        }
        details => {
            if let serde_json::Value::Object(payload) = &mut payload {
                payload.insert("details".to_string(), details);
            }
        }
    }

    ToolResult::error(message).with_structured(payload)
}

fn launch_state(requested: bool, process_running: bool, window_ready: bool) -> serde_json::Value {
    serde_json::json!({
        "requested": requested,
        "process_running": process_running,
        "window_ready": window_ready,
    })
}

fn response_identity(
    app_info: Option<&crate::apps::AppInfo>,
    requested_bundle_id: Option<&str>,
    requested_name: Option<&str>,
) -> (String, String) {
    let bundle_id = app_info
        .and_then(|app| app.bundle_id.as_deref())
        .filter(|value| !value.is_empty())
        .or(requested_bundle_id)
        .unwrap_or("?")
        .to_owned();

    let name = app_info
        .map(|app| app.name.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| requested_app_name(requested_name, requested_bundle_id));

    (name, bundle_id)
}

fn requested_app_name(requested_name: Option<&str>, requested_bundle_id: Option<&str>) -> String {
    if let Some(name) = requested_name.filter(|name| Some(*name) != requested_bundle_id) {
        let file_name = std::path::Path::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(name);
        return file_name
            .strip_suffix(".app")
            .unwrap_or(file_name)
            .to_owned();
    }

    requested_bundle_id
        .and_then(|bundle_id| bundle_id.rsplit('.').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("?")
        .to_owned()
}

fn structured_launch_failure(error: &anyhow::Error) -> ToolResult {
    use crate::apps::nsworkspace::LaunchError;

    let (code, requested) = if let Some(launch_error) = error.downcast_ref::<LaunchError>() {
        match launch_error {
            LaunchError::Cocoa(_) => ("NSWORKSPACE_LAUNCH_FAILED", true),
            LaunchError::NoApp => ("LAUNCH_RESULT_MISSING", true),
            LaunchError::Timeout => ("LAUNCH_CALLBACK_TIMEOUT", true),
            LaunchError::BadUrl(_) => ("APP_URL_INVALID", false),
        }
    } else {
        ("LAUNCH_FAILED", false)
    };

    structured_launch_error(
        code,
        format!("Launch failed: {error:#}"),
        serde_json::json!({
            "launch_state": launch_state(requested, false, false),
        }),
    )
}

fn preflight_file_urls(urls: &[String]) -> Option<ToolResult> {
    for raw in urls {
        let Some(path) = local_file_target(raw) else {
            continue;
        };
        if !path.exists() {
            return Some(structured_launch_error(
                "FILE_NOT_FOUND",
                format!(
                    "Local launch_app url target does not exist: {}",
                    path.display()
                ),
                serde_json::json!({
                    "url": raw,
                    "path": path.display().to_string(),
                }),
            ));
        }
    }
    None
}

fn normalize_launch_url(raw: String) -> String {
    local_file_target(&raw)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(raw)
}

fn local_file_target(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return Some(PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("file://") {
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = percent_decode_path(path);
        return Some(expand_tilde(&decoded));
    }
    let looks_like_url = raw.contains(':') && !raw.starts_with('/') && !raw.starts_with('~');
    if looks_like_url {
        return None;
    }
    Some(expand_tilde(raw))
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn percent_decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                decoded.push((high << 4) | low);
                i += 3;
                continue;
            }
        }

        decoded.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn launch_plan_preserves_existing_visibility_and_refuses_ambiguous_instances() {
        assert_eq!(super::launch_plan(&[], false, false), Ok(None));
        assert_eq!(super::launch_plan(&[101], false, false), Ok(Some(101)));
        assert_eq!(super::launch_plan(&[101], false, true), Ok(None));
        assert!(super::launch_plan(&[101, 102], false, false).is_err());
        assert_eq!(super::launch_plan(&[101, 102], true, false), Ok(None));
    }

    #[test]
    fn explicit_installation_selects_its_process_and_rejects_other_or_unknown_paths() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("First.app");
        let second = root.path().join("Second.app");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let alias = root.path().join("Alias.app");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let canonical = std::fs::canonicalize(&first).unwrap();
        let app = crate::apps::AppInfo {
            name: "Fixture".into(),
            pid: 101,
            bundle_id: Some("test.fixture".into()),
            running: true,
            active: false,
            launch_path: Some(alias.to_string_lossy().into_owned()),
            kind: None,
            last_used: None,
        };
        let other = crate::apps::AppInfo {
            pid: 102,
            launch_path: Some(second.to_string_lossy().into_owned()),
            ..app.clone()
        };
        let unknown = crate::apps::AppInfo {
            pid: 103,
            launch_path: None,
            ..app.clone()
        };
        assert_eq!(
            super::matching_running_instances(&[app.clone(), other.clone()], Some(&canonical)),
            Ok(vec![101])
        );
        assert!(
            super::matching_running_instances(std::slice::from_ref(&other), Some(&canonical))
                .is_err()
        );
        assert!(super::matching_running_instances(&[unknown], Some(&canonical)).is_err());
        assert_eq!(
            super::matching_running_instances(&[app.clone(), other.clone()], None),
            Ok(vec![101, 102])
        );
        assert!(super::returned_identity_matches(
            Some(&app),
            Some("test.fixture"),
            Some(&canonical)
        ));
        assert!(!super::returned_identity_matches(
            Some(&other),
            Some("test.fixture"),
            Some(&canonical)
        ));
        assert!(!super::returned_identity_matches(
            Some(&app),
            Some("other.bundle"),
            Some(&canonical)
        ));
        assert!(!super::returned_identity_matches(
            None,
            Some("test.fixture"),
            Some(&canonical)
        ));
        std::fs::remove_dir(&first).unwrap();
        assert!(super::matching_running_instances(&[app], Some(&canonical)).is_err());
    }

    use super::{
        contains_remote_debugging_flag, is_cua_driver_bundle_id, local_file_target,
        normalize_launch_url, preflight_file_urls, response_identity, structured_launch_failure,
        LaunchAppTool,
    };
    use cua_driver_core::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn local_file_target_treats_plain_paths_as_files() {
        assert_eq!(
            local_file_target("/tmp/does-not-exist.md"),
            Some(PathBuf::from("/tmp/does-not-exist.md"))
        );
        assert_eq!(
            local_file_target("relative/path.md"),
            Some(PathBuf::from("relative/path.md"))
        );
    }

    #[test]
    fn launch_url_normalization_expands_home_relative_paths() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set for macOS"));

        assert_eq!(
            PathBuf::from(normalize_launch_url("~/Desktop/BenchInbox".to_owned())),
            home.join("Desktop/BenchInbox")
        );
        assert_eq!(
            normalize_launch_url("https://example.com".to_owned()),
            "https://example.com"
        );
    }

    #[test]
    fn local_file_target_skips_remote_and_custom_schemes() {
        assert_eq!(local_file_target("https://example.com"), None);
        assert_eq!(local_file_target("about:blank"), None);
        assert_eq!(local_file_target("myapp://open/item"), None);
    }

    #[test]
    fn preflight_file_urls_returns_structured_file_not_found() {
        let missing = "/tmp/cua-driver-definitely-missing-file-for-test.md".to_string();
        let result = preflight_file_urls(&[missing]).expect("missing file should error");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured error");
        assert_eq!(structured["error"], "FILE_NOT_FOUND");
        assert_eq!(
            structured["path"],
            "/tmp/cua-driver-definitely-missing-file-for-test.md"
        );
        assert!(structured.get("details").is_none());
    }

    #[test]
    fn local_file_target_percent_decodes_file_urls_before_path_checks() {
        assert_eq!(
            local_file_target("file:///tmp/My%20Doc.txt"),
            Some(PathBuf::from("/tmp/My Doc.txt"))
        );
        assert_eq!(
            local_file_target("file://localhost/tmp/%E2%9C%93.txt"),
            Some(PathBuf::from("/tmp/✓.txt"))
        );
    }

    #[test]
    fn rejects_all_chromium_remote_debugging_spellings() {
        assert!(contains_remote_debugging_flag("--remote-debugging-port=0"));
        assert!(contains_remote_debugging_flag("--REMOTE-DEBUGGING-PIPE"));
        assert!(!contains_remote_debugging_flag(
            "--user-data-dir=/tmp/profile"
        ));
    }

    #[test]
    fn recognizes_release_and_local_protected_host_bundle_ids() {
        assert!(is_cua_driver_bundle_id("com.trycua.driver"));
        assert!(is_cua_driver_bundle_id("com.trycua.driver.local"));
        assert!(!is_cua_driver_bundle_id("com.trycua.harness.tauri"));
    }

    #[test]
    fn launch_timeout_reports_requested_without_process_or_window() {
        let error = anyhow::Error::new(crate::apps::nsworkspace::LaunchError::Timeout)
            .context("Failed to launch com.example.App");
        let result = structured_launch_failure(&error);
        let structured = result.structured_content.expect("structured error");

        assert_eq!(result.is_error, Some(true));
        assert_eq!(structured["error"], "LAUNCH_CALLBACK_TIMEOUT");
        assert_eq!(structured["launch_state"]["requested"], true);
        assert_eq!(structured["launch_state"]["process_running"], false);
        assert_eq!(structured["launch_state"]["window_ready"], false);
    }

    #[test]
    fn invalid_url_reports_request_was_not_sent() {
        let error = anyhow::Error::new(crate::apps::nsworkspace::LaunchError::BadUrl(
            "bad url".to_owned(),
        ))
        .context("Failed to launch com.example.App");
        let result = structured_launch_failure(&error);
        let structured = result.structured_content.expect("structured error");

        assert_eq!(structured["error"], "APP_URL_INVALID");
        assert_eq!(structured["launch_state"]["requested"], false);
        assert_eq!(structured["launch_state"]["process_running"], false);
        assert_eq!(structured["launch_state"]["window_ready"], false);
    }

    #[test]
    fn process_only_response_falls_back_to_requested_identity() {
        assert_eq!(
            response_identity(None, Some("com.apple.Safari"), None),
            ("Safari".to_owned(), "com.apple.Safari".to_owned())
        );
        assert_eq!(
            response_identity(
                None,
                Some("com.example.Editor"),
                Some("/Applications/Example Editor.app"),
            ),
            ("Example Editor".to_owned(), "com.example.Editor".to_owned())
        );
    }

    #[tokio::test]
    async fn explicit_app_path_preserves_protected_host_refusal() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Protected Host.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.trycua.driver</string></dict></plist>"#).unwrap();
        let result = LaunchAppTool
            .invoke(json!({ "name": app.to_str().unwrap() }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "PROTECTED_HOST_ENTRYPOINT"
        );
    }

    #[tokio::test]
    async fn launch_app_cannot_reach_private_permission_host_entrypoint() {
        let result = LaunchAppTool
            .invoke(json!({
                "bundle_id": "com.example.not-installed",
                "additional_arguments": [
                    "__permissions-host-request",
                    "--result-file",
                    "/tmp/cua-driver-permissions-forged.json"
                ]
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "PROTECTED_HOST_ENTRYPOINT"
        );

        let result = LaunchAppTool
            .invoke(json!({ "bundle_id": "com.trycua.driver" }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "PROTECTED_HOST_ENTRYPOINT"
        );
    }
}
