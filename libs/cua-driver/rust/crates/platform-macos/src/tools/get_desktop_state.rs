//! `get_desktop_state` — full-display vision screenshot (macOS).
//!
//! Vision-only desktop capture: grabs the ENTIRE main display at native
//! pixel size (no downscale) so screen-absolute pixel picks land exactly,
//! then reports the true screen size + backing scale. No AX walk, no
//! pid/window_id. This is the capture surface for actions with a primary-display
//! desktop target and screen-absolute coordinates.
//!
//! Mirrors `get_window_state.rs`'s vision ToolResult shape: an `image_png`
//! content part (or a written-out file path), a text summary line, and a
//! `structuredContent` object.

use async_trait::async_trait;
use cua_driver_contract::GetDesktopStateInput;
use cua_driver_core::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef},
    tool_args::parse_typed_input,
};
use serde_json::Value;

use super::get_screen_size::{main_screen_geometry, MainScreenGeometry};

pub struct GetDesktopStateTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "get_desktop_state".into(),
        description: "Capture the full display in true screen pixels with no downscale. \
            Use its native-size PNG as the coordinate source for actions whose target is \
            {kind:\"desktop\",display_id:\"primary\"}. Returns the true screen size and \
            backing scale factor, display_identity {uuid,native_id}, and screen_origin {x,y} \
            in global logical points. Rejects a changed primary display or inconsistent native \
            image dimensions. Vision-only: no AX tree walk."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "screenshot_out_file": { "type": "string", "description": "Write PNG here instead of base64." }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: false,
        open_world: false,
    })
}

#[async_trait]
impl Tool for GetDesktopStateTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let input = match parse_typed_input::<GetDesktopStateInput>("get_desktop_state", args) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let screenshot_out_file = input.screenshot_out_file.map(|s| {
            // Expand ~ prefix (mirrors get_window_state).
            if let Some(relative) = s.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/{relative}")
            } else {
                s
            }
        });

        // Capture and bind identity in the same blocking operation. No file or
        // image escapes until the primary display and native dimensions agree.
        let out_file = screenshot_out_file.clone();
        let res = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(Option<String>, Option<String>, MainScreenGeometry)> {
                use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
                let (png, screen) = capture_with_geometry(
                    main_screen_geometry,
                    crate::capture::screenshot_display_bytes,
                )?;
                if let Some(ref path) = out_file {
                    std::fs::write(path, &png)?;
                    Ok((None, Some(path.clone()), screen))
                } else {
                    Ok((Some(BASE64.encode(&png)), None, screen))
                }
            },
        )
        .await;
        let (b64_opt, file_path, screen) = match res {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return ToolResult::error(format!("Desktop screenshot failed: {e}")),
            Err(e) => return ToolResult::error(format!("Desktop screenshot task error: {e}")),
        };
        let (screen_width, screen_height, scale_factor) =
            (screen.width, screen.height, screen.scale_factor);
        let (screenshot_width, screenshot_height) = (screen.pixel_width, screen.pixel_height);

        let mut content: Vec<Content> = Vec::new();
        if let Some(b64) = b64_opt {
            content.push(Content::image_png(b64));
        }
        let summary = format!(
            "desktop screenshot {screenshot_width}x{screenshot_height} px \
             (screen {screen_width}x{screen_height} pts @ {scale_factor}x)"
        );
        content.push(Content::text(summary));

        let mut structured = serde_json::json!({
            "platform": "macos",
            "display": "primary",
            "screenshot_width": screenshot_width,
            "screenshot_height": screenshot_height,
            "screen_width": screen_width,
            "screen_height": screen_height,
            "scale_factor": scale_factor,
            "display_identity": screen.identity,
            "screen_origin": screen.origin,
            "screenshot_mime_type": "image/png",
        });
        if let Some(ref fp) = file_path {
            structured["screenshot_file_path"] = serde_json::json!(fp);
        }

        ToolResult {
            content,
            is_error: None,
            structured_content: Some(structured),
            action_record: None,
        }
    }
}

/// Keep this boundary injectable: equal image dimensions cannot prove that the
/// primary display is still the same physical/native source.
fn capture_with_geometry(
    mut geometry: impl FnMut() -> Option<MainScreenGeometry>,
    capture: impl FnOnce() -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<(Vec<u8>, MainScreenGeometry)> {
    let before = geometry().ok_or_else(|| {
        anyhow::anyhow!("primary display identity/geometry unavailable before capture")
    })?;
    let png = capture()?;
    let after = geometry().ok_or_else(|| {
        anyhow::anyhow!("primary display identity/geometry unavailable after capture")
    })?;
    if before != after {
        anyhow::bail!("primary display identity or geometry changed during capture; observe again");
    }
    let actual = crate::capture::png_dimensions(&png)?;
    let expected = (before.pixel_width, before.pixel_height);
    if actual != expected {
        anyhow::bail!("primary display PNG dimensions {actual:?} do not match observed native dimensions {expected:?}");
    }
    Ok((png, before))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> MainScreenGeometry {
        MainScreenGeometry {
            identity: cua_driver_contract::DisplayIdentityOutput {
                uuid: "display-a".into(),
                native_id: 7,
            },
            origin: cua_driver_contract::ScreenOriginOutput { x: 0.0, y: 0.0 },
            width: 2,
            height: 1,
            scale_factor: 2.0,
            pixel_width: 4,
            pixel_height: 2,
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        cua_driver_core::image_utils::encode_rgba_to_png(
            &vec![255; (width * height * 4) as usize],
            width,
            height,
        )
        .unwrap()
    }

    #[test]
    fn desktop_capture_returns_only_a_matching_source_and_native_size() {
        let expected = screen();
        let bytes = png(4, 2);
        let (actual, geometry) =
            capture_with_geometry(|| Some(expected.clone()), || Ok(bytes.clone())).unwrap();
        assert_eq!(actual, bytes);
        assert_eq!(geometry, expected);
        assert!(capture_with_geometry(|| Some(expected.clone()), || Ok(png(2, 1))).is_err());
    }

    #[test]
    fn desktop_capture_rejects_same_sized_primary_swap_or_disconnection() {
        for variant in 0..4 {
            let original = screen();
            let mut changed = original.clone();
            match variant {
                0 => changed.identity.uuid = "display-b".into(),
                1 => changed.identity.native_id = 8,
                2 => changed.origin.y = 100.0,
                _ => (),
            }
            let after = (variant != 3).then_some(changed);
            let mut reads = [Some(original), after].into_iter();
            assert!(capture_with_geometry(|| reads.next().flatten(), || Ok(png(4, 2))).is_err());
        }
    }

    #[test]
    fn desktop_capture_does_not_start_without_primary_identity() {
        assert!(capture_with_geometry(|| None, || panic!("capture must not run")).is_err());
    }

    #[test]
    fn schema_has_no_pid_or_window_id_and_is_read_only() {
        let d = def();
        assert!(d.read_only, "get_desktop_state must be read_only");
        assert!(!d.destructive);
        assert!(!d.idempotent);
        assert!(!d.open_world);

        let props = d.input_schema["properties"].as_object().unwrap();
        assert!(!props.contains_key("pid"), "must not accept pid");
        assert!(
            !props.contains_key("window_id"),
            "must not accept window_id"
        );
        assert!(
            !props.contains_key("capture_mode"),
            "must not accept capture_mode"
        );
        assert!(props.contains_key("session"));
        assert!(props.contains_key("screenshot_out_file"));
        assert_eq!(
            d.input_schema["additionalProperties"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn description_mentions_full_and_screen_or_display() {
        let desc = def().description.to_lowercase();
        assert!(desc.contains("full"), "description must mention 'full'");
        assert!(
            desc.contains("screen") || desc.contains("display"),
            "description must mention 'screen' or 'display'"
        );
    }
}
