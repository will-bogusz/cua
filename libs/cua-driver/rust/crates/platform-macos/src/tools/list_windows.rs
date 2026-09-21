use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;

pub struct ListWindowsTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "list_windows".into(),
        description: "List all layer-0 top-level windows currently known to WindowServer. \
            Includes off-screen windows (minimized, on another Space, hidden-launched). \
            Use this to find a window_id before calling get_window_state.\n\n\
            Per-record fields: window_id, pid, app_name, title, bounds \
            (x/y/width/height, top-left origin), z_index (integer or null; higher values are \
            closer to the front; null means stacking order is unavailable and callers must not \
            infer one), is_on_screen, space_ids, current_space_id (the active Space on that \
            window's display), and on_current_space. The top-level current_space_id is \
            WindowServer's main/global active Space and can differ from a record's \
            current_space_id when displays use independent Spaces. To select a frontmost candidate, take the \
            maximum integer z_index; if every value is null, use an explicit fallback instead of \
            relying on array order. With pid and include_accessibility_metadata, also return \
            accessibility_windows: the application's AXWindows mapped to exact window IDs, \
            with complete=false when enumeration or any mapping is unavailable. This metadata \
            does not activate the app, exclude minimized windows, or remove WindowServer rows.\n\n\
            A record may also carry kind, which is present only on windows that are not ordinary \
            application windows. \"system_overlay\" is the small system window-sharing indicator \
            macOS places inside an application while another process holds a screen-capture \
            lease on it: it stays in the list so callers can observe that the application is \
            being captured, but it is not part of the application's own UI. \"desktop\" is a \
            display's desktop surface — the window Finder draws the desktop icons on, owned by \
            Finder, sized to its display and behind every application window (it is not a \
            layer-0 window; it is listed because get_window_state can read it: the desktop's \
            icons come back as that window's tree). \"app-modal\" is a window the application \
            reports modal (AXModal): while it is up no other window of that process accepts \
            input, so act on it — or dismiss it — before addressing anything else of that pid. \
            Modality is an accessibility fact, so this kind requires \
            include_accessibility_metadata and an explicit pid. Callers enumerating an \
            application's own windows will usually want to filter out system_overlay and \
            desktop rows; an app-modal row is the application's own window and the only one \
            worth addressing while it is up. Ordinary windows omit the key entirely.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "Optional pid filter. When set, only this pid's windows are returned."
                },
                "on_screen_only": {
                    "type": "boolean",
                    "description": "When true, drop windows not on the current Space. Default false."
                },
                "include_accessibility_metadata": {
                    "type": "boolean",
                    "description": "macOS only. With an explicit pid, include the application's accessibility window identities without walking their contents. Default false."
                }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ListWindowsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid_filter = match args.opt_i64("pid") {
            Some(value) => match i32::try_from(value) {
                Ok(pid) if pid > 0 => Some(pid),
                _ => return ToolResult::error("pid must be a positive 32-bit integer"),
            },
            None => None,
        };
        let on_screen_only = args.bool_or("on_screen_only", false);
        let include_accessibility = args.bool_or("include_accessibility_metadata", false);
        if include_accessibility && !pid_filter.is_some_and(|pid| pid > 0) {
            return ToolResult::error("include_accessibility_metadata requires a positive pid");
        }

        let enumeration = crate::windows::listable_windows_with_space_snapshot(on_screen_only);
        let current_space_id = enumeration.current_space_id;
        let mut windows = enumeration.windows;
        let system_overlays = crate::window_kind::system_overlay_window_ids(&windows);

        if let Some(pid) = pid_filter {
            windows.retain(|w| w.pid == pid);
        }

        let roster = include_accessibility.then(|| accessibility_roster(pid_filter.unwrap()));

        let windows_json: Vec<Value> = windows
            .iter()
            .map(|w| {
                let kind = if system_overlays.contains(&w.window_id) {
                    Some(crate::window_kind::SYSTEM_OVERLAY_KIND)
                } else if crate::windows::is_desktop_surface(w) {
                    Some(crate::window_kind::DESKTOP_KIND)
                } else if roster
                    .as_ref()
                    .is_some_and(|roster| roster.is_app_modal(w.window_id))
                {
                    Some(crate::window_kind::APP_MODAL_KIND)
                } else {
                    None
                };
                let ax_backed = row_ax_backed(kind, roster.as_ref(), w.window_id);
                window_record_json(w, kind, ax_backed)
            })
            .collect();

        let mut data = serde_json::json!({
            "windows": windows_json,
            "current_space_id": current_space_id
        });
        if let Some(roster) = roster {
            data["accessibility_windows"] = roster.json;
        }
        ToolResult::text(format!("Found {} window(s).", windows_json.len())).with_structured(data)
    }
}

const AX_ROSTER_MAX_WINDOWS: usize = 128;
const AX_ROSTER_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);
const AX_WINDOW_MESSAGING_TIMEOUT: f32 = 0.25;

struct AccessibilityRoster {
    complete: bool,
    window_ids: std::collections::HashSet<u32>,
    /// Windows the application reports modal. A modal window blocks every
    /// other window of its process, so a roster that does not say so invites
    /// a caller to address one that cannot answer.
    modal_window_ids: std::collections::HashSet<u32>,
    json: Value,
}

impl AccessibilityRoster {
    fn unavailable(pid: i32, error: String) -> Self {
        Self {
            complete: false,
            window_ids: std::collections::HashSet::new(),
            modal_window_ids: std::collections::HashSet::new(),
            json: serde_json::json!({
                "pid": pid,
                "complete": false,
                "windows": [],
                "error": error,
            }),
        }
    }

    fn is_app_modal(&self, window_id: u32) -> bool {
        self.modal_window_ids.contains(&window_id)
    }

    fn ax_backed(&self, window_id: u32) -> Option<bool> {
        if self.window_ids.contains(&window_id) {
            Some(true)
        } else {
            self.complete.then_some(false)
        }
    }
}

/// Whether an `AXWindows` entry this roster could not describe leaves the
/// enumeration unable to claim it saw every window of the process.
///
/// An entry that maps no CGWindowID and is not an `AXWindow` is an
/// application-level surface, not a window this pass failed to identify: a
/// display's desktop surface is listed under `AXWindows` and never as an
/// `AXWindow`, so it has no window id by construction
/// (`ax::window_scope::decide_desktop_surface_scope`). Counting it as a miss
/// made every roster of a pid that owns one permanently incomplete, and an
/// incomplete mapping is the one thing a caller may not narrow an acquisition
/// with. Every other shape is a real window this roster cannot describe: an
/// `AXWindow` whose id would not map, or a non-window element that does map
/// one (an application's inline-rename field).
fn entry_leaves_roster_incomplete(mapped: Option<u32>, role: Option<&str>) -> bool {
    !matches!((mapped, role), (None, Some(role)) if role != "AXWindow")
}

/// `ax_backed` for one WindowServer row.
///
/// A desktop surface is never claimed by an `AXWindow`, yet `get_window_state`
/// reads it through the desktop-surface scope — it is the one surface of that
/// pid the driver can read. Reporting it as accessibility-backed keeps a
/// caller that skips rows no accessibility window claims from skipping the
/// only readable one.
fn row_ax_backed(
    kind: Option<&str>,
    roster: Option<&AccessibilityRoster>,
    window_id: u32,
) -> Option<bool> {
    let roster = roster?;
    if kind == Some(crate::window_kind::DESKTOP_KIND) {
        return Some(true);
    }
    roster.ax_backed(window_id)
}

fn accessibility_roster(pid: i32) -> AccessibilityRoster {
    use crate::ax::bindings::*;
    use core_foundation::base::{CFRelease, CFTypeRef};

    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return AccessibilityRoster::unavailable(
                pid,
                "No application accessibility element".to_owned(),
            );
        }
        AXUIElementSetMessagingTimeout(app, AX_WINDOW_MESSAGING_TIMEOUT);
        let copied = copy_ax_windows_checked(app);
        CFRelease(app as CFTypeRef);
        let windows = match copied {
            Ok(windows) => windows,
            Err(error) => {
                return AccessibilityRoster::unavailable(
                    pid,
                    format!("AXWindows unavailable ({error})"),
                )
            }
        };
        let mut complete = windows.len() <= AX_ROSTER_MAX_WINDOWS;
        let deadline = std::time::Instant::now() + AX_ROSTER_BUDGET;
        let mut window_ids = std::collections::HashSet::new();
        let mut modal_window_ids = std::collections::HashSet::new();
        let mut records = Vec::new();
        for (index, window) in windows.into_iter().enumerate() {
            if index < AX_ROSTER_MAX_WINDOWS && std::time::Instant::now() < deadline {
                AXUIElementSetMessagingTimeout(window, AX_WINDOW_MESSAGING_TIMEOUT);
                match (ax_get_window_id(window), copy_string_attr(window, "AXRole")) {
                    (Some(window_id), Some(role)) if role == "AXWindow" => {
                        window_ids.insert(window_id);
                        // The application's own report that this window blocks
                        // every other window of its process. Unreadable is
                        // unknown, never "not modal".
                        let modal = copy_bool_attr(window, "AXModal");
                        if modal == Some(true) {
                            modal_window_ids.insert(window_id);
                        }
                        records.push(serde_json::json!({
                            "window_id": window_id,
                            "role": role,
                            "subrole": copy_string_attr(window, "AXSubrole"),
                            "minimized": copy_bool_attr(window, "AXMinimized"),
                            "main": copy_bool_attr(window, "AXMain"),
                            "modal": modal,
                        }))
                    }
                    (mapped, role) => {
                        if entry_leaves_roster_incomplete(mapped, role.as_deref()) {
                            complete = false;
                        }
                    }
                }
            } else {
                complete = false;
            }
            CFRelease(window as CFTypeRef);
        }
        AccessibilityRoster {
            complete,
            window_ids,
            modal_window_ids,
            json: serde_json::json!({"pid": pid, "complete": complete, "windows": records}),
        }
    }
}

pub(super) fn window_record_json(
    w: &crate::windows::WindowInfo,
    kind: Option<&str>,
    ax_backed: Option<bool>,
) -> Value {
    let mut record = serde_json::json!({
        "window_id": w.window_id,
        "pid": w.pid,
        "app_name": w.app_name,
        "title": w.title,
        "bounds": {
            "x": w.bounds.x,
            "y": w.bounds.y,
            "width": w.bounds.width,
            "height": w.bounds.height
        },
        "layer": w.layer,
        "z_index": w.z_index,
        "is_on_screen": w.is_on_screen,
        "current_space_id": w.current_space_id,
        "on_current_space": w.on_current_space,
        "space_ids": w.space_ids,
    });
    if let Some(kind) = kind {
        record["kind"] = Value::String(kind.to_owned());
    }
    if let Some(ax_backed) = ax_backed {
        record["ax_backed"] = Value::Bool(ax_backed);
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accessibility_metadata_requires_an_exact_valid_process() {
        for args in [
            serde_json::json!({"include_accessibility_metadata": true}),
            serde_json::json!({"pid": 0, "include_accessibility_metadata": true}),
            serde_json::json!({"pid": 4294967297_i64, "include_accessibility_metadata": true}),
        ] {
            let result = ListWindowsTool.invoke(args).await;
            assert_eq!(result.is_error, Some(true));
        }
    }

    fn sample_window() -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id: 42,
            pid: 123,
            app_name: "Example".into(),
            title: "Document".into(),
            bounds: crate::windows::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            layer: 0,
            z_index: 7,
            is_on_screen: true,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        }
    }

    #[test]
    fn window_record_includes_observed_z_index() {
        let window = sample_window();

        assert_eq!(
            window_record_json(&window, None, None)["z_index"],
            serde_json::json!(7)
        );
        assert_eq!(
            window_record_json(&window, None, None)["current_space_id"],
            serde_json::json!(1)
        );
        assert_eq!(
            window_record_json(&window, None, None)["on_current_space"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn only_a_classified_window_carries_a_kind_key() {
        let window = sample_window();

        let classified =
            window_record_json(&window, Some(crate::window_kind::SYSTEM_OVERLAY_KIND), None);
        assert_eq!(classified["kind"], serde_json::json!("system_overlay"));

        let unclassified = window_record_json(&window, None, None);
        assert_eq!(unclassified.get("kind"), None);
    }

    fn roster(complete: bool, window_ids: &[u32]) -> AccessibilityRoster {
        modal_roster(complete, window_ids, &[])
    }

    fn modal_roster(complete: bool, window_ids: &[u32], modal: &[u32]) -> AccessibilityRoster {
        AccessibilityRoster {
            complete,
            window_ids: window_ids.iter().copied().collect(),
            modal_window_ids: modal.iter().copied().collect(),
            json: serde_json::json!({}),
        }
    }

    /// A modal window blocks every other window of its process, so the roster
    /// says which one it is. Nothing else the roster knows changes a row's
    /// kind into a claim about modality.
    #[test]
    fn only_the_window_the_app_calls_modal_is_labelled_app_modal() {
        let roster = modal_roster(true, &[42, 43], &[43]);
        assert!(roster.is_app_modal(43));
        assert!(!roster.is_app_modal(42), "an ordinary window is not modal");
        assert!(!roster.is_app_modal(58), "an unlisted window is not modal");

        let window = sample_window();
        assert_eq!(
            window_record_json(&window, Some(crate::window_kind::APP_MODAL_KIND), None)["kind"],
            serde_json::json!("app-modal")
        );
    }

    #[test]
    fn a_row_an_accessibility_window_claims_is_ax_backed() {
        let complete = roster(true, &[42, 43]);
        assert_eq!(complete.ax_backed(42), Some(true));
        assert_eq!(complete.ax_backed(58), Some(false));

        let window = sample_window();
        assert_eq!(
            window_record_json(&window, None, complete.ax_backed(window.window_id))["ax_backed"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn an_incomplete_roster_never_claims_a_row_is_cg_only() {
        let partial = roster(false, &[43]);
        assert_eq!(partial.ax_backed(43), Some(true));
        assert_eq!(partial.ax_backed(42), None);

        let window = sample_window();
        assert_eq!(
            window_record_json(&window, None, partial.ax_backed(window.window_id)).get("ax_backed"),
            None
        );
    }

    /// An application-level surface in `AXWindows` — one with no window id to
    /// map, which is what a display's desktop surface is — is not a window
    /// this enumeration missed. Counting it as one made every roster of the
    /// pid that owns it incomplete, and only a complete mapping may narrow an
    /// acquisition.
    #[test]
    fn an_unmappable_non_window_surface_does_not_make_the_roster_incomplete() {
        assert!(!entry_leaves_roster_incomplete(None, Some("AXList")));
        assert!(!entry_leaves_roster_incomplete(None, Some("AXGroup")));

        assert!(
            entry_leaves_roster_incomplete(None, Some("AXWindow")),
            "a window whose id would not map is a window this roster cannot describe"
        );
        assert!(
            entry_leaves_roster_incomplete(Some(51), Some("AXTextField")),
            "an element that maps its own window id is a surface a caller can address"
        );
        assert!(
            entry_leaves_roster_incomplete(None, None),
            "an unreadable role says nothing, and nothing is not proof"
        );
    }

    /// The desktop surface is the one row whose accessibility backing cannot
    /// come from the AXWindows mapping: no AXWindow claims it, yet
    /// `get_window_state` reads it. A caller skipping rows nothing claims must
    /// not skip it.
    #[test]
    fn the_desktop_surface_is_reported_accessibility_backed() {
        let roster = roster(true, &[42]);
        let desktop = crate::window_kind::DESKTOP_KIND;
        assert_eq!(row_ax_backed(Some(desktop), Some(&roster), 99), Some(true));
        assert_eq!(
            row_ax_backed(None, Some(&roster), 99),
            Some(false),
            "an ordinary row no accessibility window claims is still unclaimed"
        );
        assert_eq!(
            row_ax_backed(Some(desktop), None, 99),
            None,
            "without accessibility metadata the roster claims nothing"
        );
    }

    #[test]
    fn a_roster_without_accessibility_metadata_leaves_every_row_unclaimed() {
        let window = sample_window();
        assert_eq!(
            window_record_json(&window, None, None).get("ax_backed"),
            None
        );
    }
}
