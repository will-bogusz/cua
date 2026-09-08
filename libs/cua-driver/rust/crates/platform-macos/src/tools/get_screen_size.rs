use async_trait::async_trait;
use cua_driver_contract::{DisplayIdentityOutput, GetScreenSizeInput, ScreenOriginOutput};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_input,
};
use serde_json::Value;

pub struct GetScreenSizeTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "get_screen_size".into(),
        description: "Return the logical size of the main display in points plus its backing \
            scale factor, display_identity {uuid,native_id}, and screen_origin {x,y} in \
            global logical points. The identity describes the current source; desktop action \
            targets still use the primary selector and native screenshot pixels. \
            Requires no TCC permissions."
            .into(),
        input_schema: serde_json::json!({"type":"object","properties":{
            "session": cua_driver_core::tool_schema::session_schema()
        },"additionalProperties":false}),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for GetScreenSizeTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Err(result) = parse_typed_input::<GetScreenSizeInput>("get_screen_size", args) {
            return result;
        }
        match main_screen_geometry() {
            Some(screen) => ToolResult::text(format!(
                "Main display: {}x{} points @ {}x",
                screen.width, screen.height, screen.scale_factor
            ))
            .with_structured(screen.size_json()),
            None => ToolResult::error(
                "Primary display identity or geometry is unavailable or changed while reading.",
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MainScreenGeometry {
    pub identity: DisplayIdentityOutput,
    pub origin: ScreenOriginOutput,
    pub width: i64,
    pub height: i64,
    pub scale_factor: f64,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl MainScreenGeometry {
    pub(crate) fn logical_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= f64::from(self.pixel_width)
            || y >= f64::from(self.pixel_height)
        {
            return None;
        }
        Some((
            self.origin.x + x / self.scale_factor,
            self.origin.y + y / self.scale_factor,
        ))
    }

    fn size_json(&self) -> Value {
        serde_json::json!({
            "width": self.width, "height": self.height, "scale_factor": self.scale_factor,
            "display_identity": self.identity, "screen_origin": self.origin,
        })
    }
}

/// Two matching reads avoid combining an old primary identity with a new mode.
/// These are content-free CoreGraphics/ColorSync queries, safe off the main thread.
pub(crate) fn main_screen_geometry() -> Option<MainScreenGeometry> {
    stable_geometry_with(read_main_screen_geometry)
}

fn stable_geometry_with(
    mut read: impl FnMut() -> Option<MainScreenGeometry>,
) -> Option<MainScreenGeometry> {
    let first = read()?;
    let second = read()?;
    (first == second).then_some(first)
}

fn read_main_screen_geometry() -> Option<MainScreenGeometry> {
    let display_id = unsafe { core_graphics::display::CGMainDisplayID() };
    display_geometry(display_id)
}

pub(crate) fn display_geometry(display_id: u32) -> Option<MainScreenGeometry> {
    use core_graphics::display::{CGDisplay, CGDisplayBounds};
    if display_id == 0 {
        return None;
    }
    let bounds = unsafe { CGDisplayBounds(display_id) };
    let mode = CGDisplay::new(display_id).display_mode()?;
    geometry_from_observation(
        display_id,
        display_uuid(display_id)?,
        [
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        ],
        (
            mode.width(),
            mode.height(),
            mode.pixel_width(),
            mode.pixel_height(),
        ),
    )
}

fn geometry_from_observation(
    native_id: u32,
    uuid: String,
    bounds: [f64; 4],
    mode: (u64, u64, u64, u64),
) -> Option<MainScreenGeometry> {
    let [x, y, width, height] = bounds;
    let (point_w, point_h, pixel_w, pixel_h) = mode;
    if native_id == 0
        || uuid.trim().is_empty()
        || bounds.iter().any(|v| !v.is_finite())
        || width <= 0.0
        || height <= 0.0
        || width.fract() != 0.0
        || height.fract() != 0.0
        || point_w == 0
        || point_h == 0
        || pixel_w == 0
        || pixel_h == 0
    {
        return None;
    }
    let scale_factor = pixel_w as f64 / point_w as f64;
    let vertical_scale = pixel_h as f64 / point_h as f64;
    if !scale_factor.is_finite() || (scale_factor - vertical_scale).abs() > 1e-9 {
        return None;
    }
    let native_width = width * scale_factor;
    let native_height = height * scale_factor;
    if [native_width, native_height].iter().any(|v| {
        !v.is_finite() || *v < 1.0 || *v > u32::MAX as f64 || (*v - v.round()).abs() > 1e-6
    }) {
        return None;
    }
    Some(MainScreenGeometry {
        identity: DisplayIdentityOutput {
            uuid,
            native_id: u64::from(native_id),
        },
        origin: ScreenOriginOutput { x, y },
        width: width as i64,
        height: height as i64,
        scale_factor,
        pixel_width: native_width.round() as u32,
        pixel_height: native_height.round() as u32,
    })
}

#[link(name = "ColorSync", kind = "framework")]
extern "C" {
    fn CGDisplayCreateUUIDFromDisplayID(display_id: u32) -> core_foundation::uuid::CFUUIDRef;
}

fn display_uuid(display_id: u32) -> Option<String> {
    use core_foundation::{
        base::{kCFAllocatorDefault, TCFType},
        string::CFString,
        uuid::{CFUUIDCreateString, CFUUID},
    };
    unsafe {
        let raw = CGDisplayCreateUUIDFromDisplayID(display_id);
        if raw.is_null() {
            return None;
        }
        let uuid = CFUUID::wrap_under_create_rule(raw);
        let raw_string = CFUUIDCreateString(kCFAllocatorDefault, uuid.as_concrete_TypeRef());
        if raw_string.is_null() {
            return None;
        }
        Some(CFString::wrap_under_create_rule(raw_string).to_string())
    }
}

/// Return the current display mode's backing-pixel to point ratio.
pub(crate) fn get_backing_scale(display_id: u32) -> f64 {
    use core_graphics::display::CGDisplay;

    let Some(mode) = CGDisplay::new(display_id).display_mode() else {
        return 1.0;
    };
    backing_scale_from_widths(mode.width(), mode.pixel_width())
}

fn backing_scale_from_widths(point_width: u64, pixel_width: u64) -> f64 {
    if point_width == 0 || pixel_width == 0 {
        return 1.0;
    }
    let ratio = pixel_width as f64 / point_width as f64;
    // Round to nearest 0.5 to avoid floating point noise.
    (ratio * 2.0).round() / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::display::CGDisplay;

    fn screen() -> MainScreenGeometry {
        geometry_from_observation(
            7,
            "display-a".into(),
            [0.0, 0.0, 1920.0, 1080.0],
            (1920, 1080, 3840, 2160),
        )
        .unwrap()
    }

    #[test]
    fn primary_geometry_preserves_observed_identity_origin_and_exact_scale() {
        let screen = geometry_from_observation(
            7,
            "display-a".into(),
            [-1920.0, 120.0, 1920.0, 1080.0],
            (1920, 1080, 3072, 1728),
        )
        .unwrap();
        assert_eq!(
            screen.scale_factor, 1.6,
            "do not round a measured scale to a guessed half-step"
        );
        assert_eq!((screen.pixel_width, screen.pixel_height), (3072, 1728));
        let json = screen.size_json();
        assert_eq!(
            json["display_identity"],
            serde_json::json!({"uuid":"display-a","native_id":7})
        );
        assert_eq!(
            json["screen_origin"],
            serde_json::json!({"x":-1920.0,"y":120.0})
        );
        assert_eq!(screen.logical_point(160.0, 320.0), Some((-1820.0, 320.0)));
    }

    #[test]
    fn primary_geometry_rejects_missing_or_inconsistent_observations() {
        for (id, uuid, bounds, mode) in [
            (
                0,
                "display-a",
                [0.0, 0.0, 1920.0, 1080.0],
                (1920, 1080, 3840, 2160),
            ),
            (7, "", [0.0, 0.0, 1920.0, 1080.0], (1920, 1080, 3840, 2160)),
            (
                7,
                "display-a",
                [f64::NAN, 0.0, 1920.0, 1080.0],
                (1920, 1080, 3840, 2160),
            ),
            (
                7,
                "display-a",
                [0.0, 0.0, 0.0, 1080.0],
                (1920, 1080, 3840, 2160),
            ),
            (
                7,
                "display-a",
                [0.0, 0.0, 1920.0, 1080.0],
                (0, 1080, 3840, 2160),
            ),
            (
                7,
                "display-a",
                [0.0, 0.0, 1920.0, 1080.0],
                (1920, 1080, 3840, 1080),
            ),
        ] {
            assert!(geometry_from_observation(id, uuid.into(), bounds, mode).is_none());
        }
    }

    #[test]
    fn primary_geometry_refuses_equal_sized_source_replacement_during_read() {
        let original = screen();
        assert_eq!(
            stable_geometry_with(|| Some(original.clone())),
            Some(original.clone())
        );
        for variant in 0..4 {
            let mut changed = original.clone();
            match variant {
                0 => changed.identity.uuid = "display-b".into(),
                1 => changed.identity.native_id = 8,
                2 => changed.origin.x = 10.0,
                _ => changed.scale_factor = 1.0,
            }
            let mut reads = [Some(original.clone()), Some(changed)].into_iter();
            assert!(stable_geometry_with(|| reads.next().flatten()).is_none());
        }
    }

    #[test]
    fn desktop_pixel_actions_have_finite_primary_display_bounds() {
        let screen = screen();
        assert_eq!(screen.logical_point(1920.0, 1080.0), Some((960.0, 540.0)));
        for (x, y) in [
            (-1.0, 0.0),
            (3840.0, 0.0),
            (0.0, 2160.0),
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(screen.logical_point(x, y).is_none());
        }
    }

    #[test]
    #[ignore = "requires an attached macOS display; run explicitly on a native Retina host"]
    fn main_display_scale_matches_current_mode() {
        let display = CGDisplay::main();
        let mode = display
            .display_mode()
            .expect("main display should expose its current mode");
        let point_width = mode.width();
        let pixel_width = mode.pixel_width();
        assert!(point_width > 0, "display mode should have a point width");
        assert!(pixel_width > 0, "display mode should have a pixel width");
        assert!(
            pixel_width > point_width,
            "native Retina validation requires more backing pixels than points; got {pixel_width}px for {point_width}pt"
        );

        let expected = ((pixel_width as f64 / point_width as f64) * 2.0).round() / 2.0;
        let actual = get_backing_scale(display.id);

        assert_eq!(
            actual, expected,
            "backing scale should use the current mode's pixel and point widths"
        );
    }

    #[test]
    fn display_mode_widths_preserve_rounding_and_fallbacks() {
        assert_eq!(backing_scale_from_widths(1920, 3840), 2.0);
        assert_eq!(backing_scale_from_widths(1920, 2880), 1.5);
        assert_eq!(backing_scale_from_widths(0, 3840), 1.0);
        assert_eq!(backing_scale_from_widths(1920, 0), 1.0);
    }
}
