use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_contract::{
    ScrollBy, ScrollDelivery, ScrollDirection, ScrollInput, ScrollOutcomeKind, ScrollWheelUnit,
};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_children, copy_element_attr, copy_string_attr, element_at_screen_position,
    element_screen_rect, kAXErrorSuccess, perform_action, AXUIElementRef,
};
use crate::focus_guard;
use crate::frame_sampler::{FrameSampler, Settle};
use crate::input::raised_pointer::{self, RaisedPointerError};
use crate::scroll_motion::{
    self, ChunkMeasure, ChunkRun, ChunkStep, Classified, LoopSummary, OutcomeKind,
    ScrollCalibrations, ScrollUnit, POINTS_AMOUNT_MAX,
};
use crate::window_change_detector::WindowChangeDetector;
use crate::windows::WindowBounds;

use super::ToolState;

/// Per-notch step of the desktop-scope wheel (`scope:"desktop"`).
const DESKTOP_STEP_LINE_PX: i32 = 120;
const DESKTOP_STEP_PAGE_PX: i32 = 600;

/// `scroll_wheel_at_xy` turns each 120 of a tick's delta into one line.
const LINE_TICK_DELTA: i32 = 120;

/// Where the wheel goes: screen point, window-local point, and the point in
/// the window's screenshot pixels the reply names (`None` when an element's
/// point could not be mapped into them).
#[derive(Debug, Clone, Copy, PartialEq)]
struct WheelPoint {
    screen: (f64, f64),
    local: (f64, f64),
    report: Option<(f64, f64)>,
}

/// No element and no `x`/`y`: the centre of the window.
fn window_centre(bounds: &WindowBounds) -> WheelPoint {
    let local = (bounds.width / 2.0, bounds.height / 2.0);
    WheelPoint {
        screen: (bounds.x + local.0, bounds.y + local.1),
        local,
        report: None,
    }
}

fn after_exact_target_gate<T>(
    gate: Result<(), ToolResult>,
    action: impl FnOnce() -> T,
) -> Result<T, ToolResult> {
    gate?;
    Ok(action())
}

pub struct ScrollTool {
    state: Arc<ToolState>,
}

impl ScrollTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "scroll".into(),
        description: "Scroll a window at a point: the centre of `element_index`/`element_token` \
            (revealed first), window-local screenshot `x, y`, or — with neither — the centre of \
            the window.\n\n\
            • delivery_mode:\"foreground\" — pointer-routed, for any app: the driver fronts the \
            window, refuses with `target_covered` before any input if another window still \
            covers the point, parks the real pointer there, posts pixel wheel events through \
            the HID tap in measured chunks (at most 400 pt and 0.6 of the scroll area), then \
            puts the pointer and the previously frontmost app back. If the user moves the \
            pointer or brings another app to the front during the scroll, it stops sending at \
            once and restores nothing. Reaches views that scroll only under the real pointer \
            (pixel-only surfaces, nested web scrollers). motion:\"stroke\" sends the whole \
            distance as one continuous wheel stream instead (48 px every 40 ms, sized by the \
            window's measured points per pixel, stopped early when tracked travel reaches the \
            distance, at most 9600 pt) and settles once: no correcting chunk, so the reply \
            states the error; a landing the view settled back to (a snap point or its end) is \
            reported as moved but not confirmed.\n\
            • delivery_mode:\"background\" (default) — line wheel events posted to the pid at \
            the point: no activation, no pointer move. An AppKit text area addressed by \
            element scrolls through its scroll bar instead.\n\n\
            Distance: by='points' → `amount` points; by='line' → `amount` × 40 pt (background: \
            `amount` line ticks); by='page' → `amount` × 0.8 × the visible height of the scroll \
            area under the point, or its width when scrolling sideways (background: 5 lines per \
            page).\n\n\
            Every wheel reply is measured from window frames — moved N pt, at end, no motion, \
            changed in place, or unmeasured — and the structured `scroll` object carries the \
            numbers. The AppKit text-area scroll-bar route (background, element target) is \
            not measured and carries no `scroll` object."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            // `pid` conditionally required (validated in code), not pinned in the
            // schema — keeps the contract consistent across platforms.
            "required": ["direction"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid": { "type": "integer" },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "description": "Scroll direction: the way the view moves through its content (down reveals what is below)."
                },
                "by": {
                    "type": "string",
                    "enum": ["line", "page", "points"],
                    "description": "Distance unit. Default: line. `points` is for window scrolls; desktop scope takes line or page."
                },
                "amount": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": POINTS_AMOUNT_MAX,
                    "description": "How many units. line and page clamp to 50; points accepts up to 5000 and has no default. Default for a window scroll: 3 lines, or 1 page. Desktop scope defaults to 3 of either."
                },
                "window_id": { "type": "integer" },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "x": { "type": "number", "description": "Window-local screenshot X (top-left origin of the PNG from get_window_state). With `y`, scrolls at this point — use for a scrollable surface that isn't in the AX tree. Requires window_id to anchor the window→screen conversion." },
                "y": { "type": "number", "description": "Window-local screenshot Y. See `x`." },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "Use desktop with x,y and no pid/window_id for native get_desktop_state screenshot coordinates." },
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema(),
                "motion": {
                    "type": "string",
                    "enum": ["stepped", "stroke"],
                    "default": "stepped",
                    "description": "Foreground only. stepped (default): measured chunks, each settled and corrected. stroke: one continuous wheel stream for the whole distance, one settle at the end, no correction."
                },
                "detect_window_change": { "type": "boolean", "description": "Default true: after the action the driver polls WindowServer for up to one second so the reply can name a window the action opened. Pass false when you enumerate windows yourself — the poll is then skipped (roughly a second off this call) and the reply carries no opened-window evidence." },
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: true,
    })
}

fn parse_direction(raw: &str) -> Option<ScrollDirection> {
    match raw {
        "up" => Some(ScrollDirection::Up),
        "down" => Some(ScrollDirection::Down),
        "left" => Some(ScrollDirection::Left),
        "right" => Some(ScrollDirection::Right),
        _ => None,
    }
}

/// The `motion` argument: `Ok(true)` for a stroke. A stroke is a pointer
/// gesture on a window: refused for background delivery and for the
/// desktop scope, as is anything but the two names.
fn parse_motion(raw: Option<&Value>, foreground: bool, desktop: bool) -> Result<bool, String> {
    match raw {
        None | Some(Value::Null) => Ok(false),
        Some(Value::String(motion)) if motion == "stepped" => Ok(false),
        Some(Value::String(motion)) if motion == "stroke" => {
            if desktop {
                Err("motion:\"stroke\" scrolls a window: pass pid and window_id with \
                     delivery_mode:\"foreground\""
                    .to_owned())
            } else if !foreground {
                Err("motion:\"stroke\" needs delivery_mode:\"foreground\"".to_owned())
            } else {
                Ok(true)
            }
        }
        Some(other) => Err(format!("motion must be \"stepped\" or \"stroke\" (got {other})")),
    }
}

#[async_trait]
impl Tool for ScrollTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let desktop = args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none();
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let foreground = delivery_mode.is_foreground();
        let stroke = match parse_motion(args.get("motion"), foreground, desktop) {
            Ok(stroke) => stroke,
            Err(message) => return ToolResult::error(message),
        };
        if desktop {
            return desktop_scroll(&args).await;
        }
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        if !foreground && crate::browser::ElectronJs::is_electron(pid) {
            return ToolResult::error(
                "Background scroll is unavailable for Electron/Chromium windows on macOS."
                    .to_owned(),
            )
            .with_structured(serde_json::json!({ "code": "background_unavailable" }));
        }
        let direction = match args.require_str("direction") {
            Ok(v) => match parse_direction(&v) {
                Some(direction) => direction,
                None => {
                    return ToolResult::error(format!(
                        "direction must be up, down, left or right (got {v:?})"
                    ))
                }
            },
            Err(e) => return e,
        };
        let by = args.str_or("by", "line");
        let Some(unit) = ScrollUnit::parse(&by) else {
            return ToolResult::error(format!("by must be line, page or points (got {by:?})"));
        };
        if unit == ScrollUnit::Points && args.get("amount").is_none() {
            return ToolResult::error(
                "by:\"points\" needs an explicit amount (1-5000 points)".to_owned(),
            );
        }
        let amount = unit.clamp_amount(args.u64_or("amount", unit.default_amount()));
        // Surface 6: element_token / element_index precedence.
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id");
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match self.state.element_cache.resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "scroll",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (_, window_id, pre_focus_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let pre_focus_ptr: Option<usize> = pre_focus_guard.as_ref().map(|g| g.as_ptr());

        let mut _mutation_lease: Option<super::BackgroundMutationLease> = None;

        // AppKit exposes vertical scroll-bar buttons beneath the text area's
        // AXScrollArea parent. Pressing those controls is a true
        // background-safe scroll: no activation, z-order change, or cursor
        // move. Foreground delivery is the pointer route for every window.
        let vertical = matches!(direction, ScrollDirection::Up | ScrollDirection::Down);
        if vertical && !foreground && unit != ScrollUnit::Points {
            if let (Some(element_guard), Some(wid)) = (pre_focus_guard.clone(), window_id) {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    pre_focus_ptr,
                    cua_driver_core::background_input::BackgroundAction::AxSemantic,
                )
                .await
                {
                    Ok(lease) => _mutation_lease = Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
                let ax_result = cua_driver_core::operation::spawn_blocking(move || unsafe {
                    scroll_native_text_area(
                        element_guard.as_ptr() as AXUIElementRef,
                        direction,
                        unit,
                        amount as usize,
                    )
                })
                .await;
                match ax_result {
                    Ok(true) => {
                        return ToolResult::text(format!(
                            "✅ Scrolled native macOS control {} by {} × {amount} through AX.",
                            direction.as_str(),
                            unit.as_str()
                        ))
                        .with_structured(serde_json::json!({
                            "path": "ax",
                            "verified": false,
                            "effect": "unverifiable"
                        }));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return ToolResult::error(format!("Native AX scroll task failed: {error}"));
                    }
                }
            }
        }

        let wid = match window_id {
            Some(wid) => wid,
            None => match crate::windows::resolve_main_window_id(pid) {
                Ok(wid) => wid,
                Err(error) => return ToolResult::error(format!("scroll: {error}")),
            },
        };
        let ratio = self.state.resize_registry.ratio(pid, Some(wid));
        let x_arg = args
            .opt_f64("x")
            .or_else(|| args.opt_i64("x").map(|v| v as f64));
        let y_arg = args
            .opt_f64("y")
            .or_else(|| args.opt_i64("y").map(|v| v as f64));

        let point: WheelPoint = if let Some(element_ptr) = pre_focus_ptr {
            // Revealing an element is itself an AX mutation. Prove that the
            // cached element still belongs to the exact requested window before
            // AXScrollToVisible, then keep the lease for the stricter pointer
            // revalidation below.
            let semantic_gate = if !foreground {
                if let Some(lease) = _mutation_lease.as_ref() {
                    lease
                        .gate_again(
                            wid,
                            Some(element_ptr),
                            cua_driver_core::background_input::BackgroundAction::AxSemantic,
                        )
                        .await
                } else {
                    match super::gate_background_window_action(
                        pid,
                        wid,
                        Some(element_ptr),
                        cua_driver_core::background_input::BackgroundAction::AxSemantic,
                    )
                    .await
                    {
                        Ok(lease) => {
                            _mutation_lease = Some(lease);
                            Ok(())
                        }
                        Err(refusal) => Err(refusal),
                    }
                }
            } else {
                Ok(())
            };
            // Element path: wheel at the element's screen-space center. Both AX
            // coordinates and window bounds are logical top-left points, so no
            // Retina scaling is needed here.
            let target_guard = pre_focus_guard.clone();
            let target_task = cua_driver_core::operation::spawn_blocking(move || {
                let _target_guard = target_guard;
                // Web content can be present in AX while its frame is below
                // the outer page viewport. Ask the accessibility hierarchy to
                // reveal the target before taking the screen-space center;
                // otherwise the wheel is posted outside the rendered window
                // and nested overflow regions never receive it.
                after_exact_target_gate(semantic_gate, || unsafe {
                    crate::ax::bindings::perform_action(
                        element_ptr as AXUIElementRef,
                        "AXScrollToVisible",
                    )
                })?;
                std::thread::sleep(std::time::Duration::from_millis(40));
                // Wheel at the centre of the part of the element that is on
                // screen: a text view inside a short scroller reports a frame
                // far taller than what it shows.
                let Some(rect) = (unsafe { element_screen_rect(element_ptr as AXUIElementRef) })
                else {
                    return Ok(None);
                };
                let Some(bounds) = crate::windows::window_bounds_by_id(wid) else {
                    return Ok(None);
                };
                let area = unsafe {
                    core_foundation::base::CFRetain(element_ptr as CFTypeRef);
                    scroll_area_from(element_ptr as AXUIElementRef)
                };
                let Some((cx, cy)) = visible_centre(rect, area, &bounds) else {
                    return Ok(None);
                };
                let report = super::px_frame::resolve_window_px_frame(wid)
                    .ok()
                    .map(|frame| screenshot_point(&frame, (cx, cy), ratio));
                Ok(Some(WheelPoint {
                    screen: (cx, cy),
                    local: (cx - bounds.x, cy - bounds.y),
                    report,
                }))
            });
            match target_task.await {
                Ok(Ok(Some(point))) => point,
                Ok(Ok(None)) | Err(_) => {
                    return ToolResult::error(
                        "scroll: the element has no on-screen position in its window; \
                         re-observe the window."
                            .to_owned(),
                    )
                }
                Ok(Err(refusal)) => return refusal,
            }
        } else if let (Some(x), Some(y)) = (x_arg, y_arg) {
            // Targeted x,y are window-local screenshot pixels and REQUIRE a
            // window_id to anchor the window→screen conversion (schema contract).
            if window_id.is_none() {
                return ToolResult::error(
                    "window_id is required when scrolling by window-local x,y pixels.".to_string(),
                );
            }
            // Mirror the click pixel path — undo any session downscale, then
            // translate through the shared window frame (which refuses a
            // window with no live frame rather than scrolling at
            // screen-absolute coords).
            let (cx, cy) = match ratio {
                Some(ratio) => (x * ratio, y * ratio),
                None => (x, y),
            };
            match super::px_frame::resolve_or_refuse(wid).await {
                Ok(frame) => {
                    let (sx, sy, lx, ly) = frame.to_screen(cx, cy);
                    WheelPoint {
                        screen: (sx, sy),
                        local: (lx, ly),
                        report: Some((x, y)),
                    }
                }
                Err(refusal) => return refusal,
            }
        } else {
            let Some(bounds) = crate::windows::window_bounds_by_id(wid) else {
                return ToolResult::error(format!(
                    "scroll: window {wid} is not on screen; re-observe the window."
                ));
            };
            let mut point = window_centre(&bounds);
            match super::px_frame::resolve_or_refuse(wid).await {
                Ok(frame) => point.report = Some(screenshot_point(&frame, point.screen, ratio)),
                Err(refusal) => return refusal,
            }
            point
        };

        if !foreground {
            let (lx, ly) = point.local;
            if let Some(bounds) = crate::windows::window_bounds_by_id(wid) {
                if lx < 0.0 || ly < 0.0 || lx > bounds.width || ly > bounds.height {
                    return ToolResult::error(format!(
                        "scroll: window-local point ({lx:.1}, {ly:.1}) pt lies outside \
                         window {wid}'s {:.0}×{:.0} pt frame; background delivery \
                         refused",
                        bounds.width, bounds.height
                    ));
                }
            }
            // The semantic reveal gate does not authorize pointer delivery.
            // Revalidate the stricter route immediately before dispatch.
            if let Some(lease) = _mutation_lease.as_ref() {
                if let Err(refusal_result) = lease
                    .gate_again(
                        wid,
                        pre_focus_ptr,
                        cua_driver_core::background_input::BackgroundAction::WindowPointer,
                    )
                    .await
                {
                    return refusal_result;
                }
            } else {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    pre_focus_ptr,
                    cua_driver_core::background_input::BackgroundAction::WindowPointer,
                )
                .await
                {
                    Ok(lease) => _mutation_lease = Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            }
        }

        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
        // Pin + glide the agent-cursor overlay to the target for visibility
        // (overlay only — does NOT move the hardware cursor). Mirrors click.
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::PinAbove(wid as u64),
        );
        crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), point.screen.0, point.screen.1)
            .await;
        self.state
            .cursor_registry
            .update_position(&cursor_key, point.screen.0, point.screen.1);

        let request = Request {
            pid,
            wid,
            point,
            direction,
            unit,
            amount,
        };
        if foreground {
            self.foreground(request, stroke, &args).await
        } else {
            background(request, &args).await
        }
    }
}

/// A resolved window scroll.
#[derive(Debug, Clone, Copy)]
struct Request {
    pid: i32,
    wid: u32,
    point: WheelPoint,
    direction: ScrollDirection,
    unit: ScrollUnit,
    amount: u32,
}

impl Request {
    /// The visible scroll area under the point in window-local points, and
    /// its visible height (the window's when accessibility exposes none).
    fn area(&self) -> (Option<[f64; 4]>, f64) {
        let Some(bounds) = crate::windows::window_bounds_by_id(self.wid) else {
            return (None, 0.0);
        };
        let area = scroll_area_at(self.pid, self.point.screen.0, self.point.screen.1);
        visible_area(area, &bounds)
    }
}

/// Clip a scroll area's screen frame to the window it sits in, as window-local
/// points. A scroll area's frame can extend past the window (a document
/// view, a resized window), and a page is what the user can see.
fn visible_area(area: Option<[f64; 4]>, window: &WindowBounds) -> (Option<[f64; 4]>, f64) {
    let clipped = area.and_then(|[x, y, w, h]| {
        let x0 = x.max(window.x);
        let y0 = y.max(window.y);
        let x1 = (x + w).min(window.x + window.width);
        let y1 = (y + h).min(window.y + window.height);
        (x1 - x0 >= 1.0 && y1 - y0 >= 1.0).then(|| [x0 - window.x, y0 - window.y, x1 - x0, y1 - y0])
    });
    let visible = clipped.map_or(window.height, |[_, _, _, h]| h);
    (clipped, visible)
}

/// A unit step from `(x, y)` toward the window's centre, so the pointer
/// primer approaches the point from inside the window.
fn toward_interior((x, y): (f64, f64), window: &WindowBounds) -> (f64, f64) {
    let (cx, cy) = (window.x + window.width / 2.0, window.y + window.height / 2.0);
    let sign = |from: f64, to: f64| if to >= from { 1.0 } else { -1.0 };
    (sign(x, cx), sign(y, cy))
}

/// The points-per-pixel a scroll starts from, and whether it was measured
/// (not the 1.0 default). A stroke reads its own calibration, else the
/// stepped one; a stepped scroll reads only its own.
fn starting_calibration(
    stepped: &ScrollCalibrations,
    strokes: &ScrollCalibrations,
    stroke: bool,
    pid: i32,
    wid: u32,
) -> (f64, bool) {
    let stepped_k = stepped.get(pid, wid);
    let k = if stroke {
        strokes.get(pid, wid).or(stepped_k)
    } else {
        stepped_k
    };
    (k.unwrap_or(1.0), k.is_some())
}

/// Keep a measured calibration where only its own mode reads it first: a
/// stroke's never resizes a stepped scroll's chunks.
fn record_calibration(
    stepped: &ScrollCalibrations,
    strokes: &ScrollCalibrations,
    stroke: bool,
    pid: i32,
    wid: u32,
    k: f64,
) {
    if stroke {
        strokes.record(pid, wid, k);
    } else {
        stepped.record(pid, wid, k);
    }
}

impl ScrollTool {
    async fn foreground(&self, request: Request, stroke: bool, args: &Value) -> ToolResult {
        let Request { pid, wid, .. } = request;
        let (k0, calibrated) = starting_calibration(
            &self.state.scroll_calibrations,
            &self.state.stroke_calibrations,
            stroke,
            pid,
            wid,
        );
        // The envelope owns activation and restoration; any suppressor would
        // fight it, or undo the user's own switch mid-gesture.
        let snapshot = WindowChangeDetector::snapshot_without_suppression(apps::frontmost_pid());
        let run = cua_driver_core::operation::spawn_blocking(move || {
            let (area, visible_height) = request.area();
            // The extent along the scroll: a page and the chunk cap are both
            // sized from it, so a sideways page is 0.8 of the visible width.
            let band = match request.direction {
                ScrollDirection::Up | ScrollDirection::Down => visible_height,
                ScrollDirection::Left | ScrollDirection::Right => area
                    .map(|[_, _, w, _]| w)
                    .or_else(|| crate::windows::window_bounds_by_id(wid).map(|b| b.width))
                    .unwrap_or(0.0),
            };
            let requested = scroll_motion::requested_distance(request.unit, request.amount, band)
                .round()
                .max(1.0);
            let max_chunk = scroll_motion::max_chunk_pt(band);
            raised_pointer::with_raised_pointer(
                pid,
                wid,
                request.point.screen.0,
                request.point.screen.1,
                |envelope| {
                    if stroke {
                        pointer_stroke(request, area, requested, k0, calibrated, envelope)
                    } else {
                        pointer_scroll(request, area, requested, k0, max_chunk, envelope)
                    }
                },
            )
            .map(|run| (run, requested))
        })
        .await;
        let changes = super::finish_window_observation(snapshot, args).await;
        let (run, requested) = match run {
            Ok(Ok(run)) => run,
            Ok(Err(RaisedPointerError::Covered(covered))) => {
                return covered_refusal(&request, &covered);
            }
            Ok(Err(RaisedPointerError::Failed(error))) => {
                return ToolResult::error(format!("scroll failed: {error}"));
            }
            Err(error) => return ToolResult::error(format!("scroll task failed: {error}")),
        };
        if run.summary.total_px == 0 {
            let reason = run
                .stopped
                .unwrap_or_else(|| "no wheel event could be posted".to_owned());
            return not_sent_refusal(&request, &reason);
        }
        if let Some(k) = run.summary.calibration {
            record_calibration(
                &self.state.scroll_calibrations,
                &self.state.stroke_calibrations,
                stroke,
                pid,
                wid,
                k,
            );
        }
        let report = Report {
            delivery: ScrollDelivery::Foreground,
            direction: request.direction,
            point: request.point.report,
            requested_pt: Some(requested as u32),
            wheel_unit: ScrollWheelUnit::Pixel,
            events: run.summary.events,
            total: run.summary.total_px,
            chunks: run.summary.chunks,
            verdict: run.verdict,
            stopped: run.stopped,
            from_accessibility: run.from_accessibility,
            stroke: run.stroke,
        };
        report.result(&changes)
    }
}

struct PointerRun {
    summary: LoopSummary,
    verdict: Verdict,
    /// Why the gesture stopped before its distance, when something other
    /// than the view did.
    stopped: Option<String>,
    /// The distance comes from the accessibility scroll position because the
    /// window's pixels did not show the move.
    from_accessibility: bool,
    /// Set for a `motion:"stroke"` scroll.
    stroke: Option<StrokeShape>,
}

/// What a stroke's frames showed beyond the landing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct StrokeShape {
    /// See [`scroll_motion::StrokeVerdict::snapped_from`].
    snapped_from: Option<i32>,
    /// See [`scroll_motion::StrokeVerdict::reshaped_header`].
    reshaped_header: bool,
}

/// Inside the raised envelope: prime the pointer, then scroll `requested_pt`
/// in measured chunks. Destination and ownership are checked before each
/// chunk, and ownership again before every wheel event.
fn pointer_scroll(
    request: Request,
    area: Option<[f64; 4]>,
    requested_pt: f64,
    k0: f64,
    max_chunk_pt: f64,
    envelope: &raised_pointer::Envelope,
) -> anyhow::Result<PointerRun> {
    let (x, y) = request.point.screen;
    let mut stopped: Option<String> = None;
    // Prime before the sampler's baseline, so the redraw a pointer arriving
    // causes (hover styling) settles before the first frame the gesture is
    // measured against.
    let primed = match envelope.check() {
        Ok(()) => {
            crate::input::mouse::prime_pointer_at(x, y, toward_interior((x, y), envelope.bounds()))
        }
        Err(reason) => {
            stopped = Some(reason);
            Ok(())
        }
    };
    primed?;
    let sampler = FrameSampler::start(request.wid, area, request.point.local);
    let mut unmeasured = sampler.as_ref().err().cloned();
    let position = || scroll_position(request.pid, x, y);
    let start_position = position();
    let mut from_accessibility = false;
    let summary = scroll_motion::drive_chunks(requested_pt, k0, max_chunk_pt, |px| {
        let halted = |stop: &mut Option<String>, reason: String| {
            *stop = Some(reason);
            Ok(ChunkRun {
                step: ChunkStep::Unmeasured,
                posted_px: 0,
                posted_events: 0,
                stop: true,
            })
        };
        if let Some(reason) = stopped.clone() {
            return halted(&mut stopped, reason);
        }
        if let Err(reason) = envelope.check() {
            return halted(&mut stopped, reason);
        }
        let events = scroll_motion::wheel_events(px, request.direction);
        if let Ok(sampler) = &sampler {
            sampler.begin_chunk();
        }
        let chunk_position = position();
        let mut posted = 0usize;
        let mut takeover: Option<String> = None;
        let burst = crate::input::mouse::pixel_wheel_burst(
            x,
            y,
            &events,
            scroll_motion::WHEEL_EVENT_INTERVAL,
            || match envelope.takeover() {
                Some(reason) => {
                    takeover = Some(reason);
                    false
                }
                None => true,
            },
            || {
                if let Ok(sampler) = &sampler {
                    sampler.mark_event();
                }
            },
            &mut posted,
        );
        let posted_px: u32 = events[..posted]
            .iter()
            .map(|(w1, w2)| w1.unsigned_abs() + w2.unsigned_abs())
            .sum();
        let mut stop = takeover.is_some();
        if let Some(reason) = takeover {
            stopped = Some(reason);
        }
        if let Err(error) = burst {
            stopped = Some(format!("input stopped: {error}"));
            unmeasured = Some(format!("input stopped mid-chunk: {error}"));
            stop = true;
        }
        let step = match &sampler {
            Err(_) => ChunkStep::Unmeasured,
            Ok(_) if unmeasured.is_some() => ChunkStep::Unmeasured,
            Ok(_) if posted == 0 => ChunkStep::Unmeasured,
            Ok(sampler) => match sampler.wait_settled() {
                Settle::Unmeasured(reason) => {
                    unmeasured = Some(reason);
                    ChunkStep::Unmeasured
                }
                settle => {
                    let mut classified =
                        scroll_motion::classify(&sampler.chunk_motion(), request.direction);
                    if let Some(moved) = position_moved(request.direction, chunk_position, position())
                        .filter(|_| classified.kind == OutcomeKind::NoMotion)
                    {
                        classified = moved;
                        from_accessibility = true;
                    }
                    ChunkStep::Measured(ChunkMeasure {
                        classified,
                        settled: settle == Settle::Settled,
                    })
                }
            },
        };
        Ok(ChunkRun {
            step,
            posted_px,
            posted_events: posted as u32,
            stop,
        })
    })?;
    if stopped.is_none() {
        stopped = summary.short.map(str::to_owned);
    }
    let verdict = match (sampler, unmeasured) {
        (Ok(sampler), None) => {
            let mut classified = scroll_motion::classify(&sampler.finish(), request.direction);
            match position_moved(request.direction, start_position, position()) {
                Some(moved) if classified.kind == OutcomeKind::NoMotion => {
                    classified = moved;
                    from_accessibility = true;
                }
                _ if classified.kind != OutcomeKind::NoMotion => from_accessibility = false,
                _ => {}
            }
            if classified.kind == OutcomeKind::Moved
                && summary.stalled
                && f64::from(classified.along) < 0.8 * requested_pt
            {
                classified.kind = OutcomeKind::AtEnd;
            }
            Verdict::Measured(classified)
        }
        (_, reason) => Verdict::Unmeasured(reason.unwrap_or_else(|| "no frame was captured".into())),
    };
    Ok(PointerRun {
        summary,
        verdict,
        stopped,
        from_accessibility,
        stroke: None,
    })
}

/// Events between destination checks inside a stroke (about 400 ms).
const STROKE_CHECK_EVERY: usize = 10;

/// Inside the raised envelope: prime the pointer, then post the whole
/// `requested_pt` (at most [`scroll_motion::STROKE_MAX_PT`], at `k0` points
/// per pixel) as one stream of wheel events
/// [`scroll_motion::STROKE_EVENT_INTERVAL`] apart, stopping early once the
/// travel tracked frame to frame reaches the distance; then settle once and
/// measure the landing. Ownership is checked before every wheel event and
/// the destination every [`STROKE_CHECK_EVERY`]. `calibrated`: `k0` was
/// measured, not the 1.0 default.
fn pointer_stroke(
    request: Request,
    area: Option<[f64; 4]>,
    requested_pt: f64,
    k0: f64,
    calibrated: bool,
    envelope: &raised_pointer::Envelope,
) -> anyhow::Result<PointerRun> {
    let limited = requested_pt > scroll_motion::STROKE_MAX_PT;
    let stroke_pt = requested_pt.min(scroll_motion::STROKE_MAX_PT);
    let (x, y) = request.point.screen;
    let mut stopped: Option<String> = None;
    match envelope.check() {
        Ok(()) => {
            crate::input::mouse::prime_pointer_at(x, y, toward_interior((x, y), envelope.bounds()))?
        }
        Err(reason) => stopped = Some(reason),
    }
    let sampler = FrameSampler::start(request.wid, area, request.point.local);
    let mut unmeasured = sampler.as_ref().err().cloned();
    let position = || scroll_position(request.pid, x, y);
    let start_position = position();
    let events = scroll_motion::stroke_events(stroke_pt, k0, request.direction);
    let mut posted = 0usize;
    if stopped.is_none() {
        let mut asked = 0usize;
        let burst = crate::input::mouse::pixel_wheel_burst(
            x,
            y,
            &events,
            scroll_motion::STROKE_EVENT_INTERVAL,
            || {
                if let Some(reason) = envelope.takeover() {
                    stopped = Some(reason);
                    return false;
                }
                if asked > 0 && asked % STROKE_CHECK_EVERY == 0 {
                    if let Err(reason) = envelope.check() {
                        stopped = Some(reason);
                        return false;
                    }
                }
                asked += 1;
                !matches!(&sampler, Ok(s) if f64::from(s.tracked_along(request.direction)) >= stroke_pt)
            },
            || {
                if let Ok(sampler) = &sampler {
                    sampler.mark_event();
                }
            },
            &mut posted,
        );
        if let Err(error) = burst {
            stopped = Some(format!("input stopped: {error}"));
            unmeasured = Some(format!("input stopped mid-stroke: {error}"));
        }
    }
    let posted_px: u32 = events[..posted]
        .iter()
        .map(|(w1, w2)| w1.unsigned_abs() + w2.unsigned_abs())
        .sum();
    let expected_pt = f64::from(posted_px) * k0;
    let mut from_accessibility = false;
    let mut shape = StrokeShape::default();
    let mut calibration = None;
    let verdict = match (sampler, unmeasured) {
        (Ok(_), None) if posted == 0 => {
            Verdict::Unmeasured("no wheel event was posted".into())
        }
        (Ok(sampler), None) => match sampler.wait_settled() {
            Settle::Unmeasured(reason) => Verdict::Unmeasured(reason),
            _ => {
                let stroke = scroll_motion::classify_stroke(
                    &sampler.finish(),
                    request.direction,
                    expected_pt,
                    calibrated,
                );
                let mut classified = stroke.classified;
                shape = StrokeShape {
                    snapped_from: stroke.snapped_from,
                    reshaped_header: stroke.reshaped_header,
                };
                if let Some(moved) = position_moved(request.direction, start_position, position())
                    .filter(|_| classified.kind == OutcomeKind::NoMotion)
                {
                    classified = moved;
                    from_accessibility = true;
                }
                if stopped.is_none() && !from_accessibility {
                    calibration = scroll_motion::stroke_calibration(&classified, posted_px, k0);
                }
                Verdict::Measured(classified)
            }
        },
        (_, reason) => Verdict::Unmeasured(reason.unwrap_or_else(|| "no frame was captured".into())),
    };
    if stopped.is_none() && limited {
        stopped = Some("the stroke limit was reached".to_owned());
    }
    Ok(PointerRun {
        summary: LoopSummary {
            chunks: u32::from(posted > 0),
            total_px: posted_px,
            events: posted as u32,
            calibration,
            stalled: false,
            unmeasured: matches!(verdict, Verdict::Unmeasured(_)),
            halted: stopped.is_some(),
            short: None,
        },
        verdict,
        stopped,
        from_accessibility,
        stroke: Some(shape),
    })
}

/// The on-screen origin of the document inside the scroll area under a
/// point: the child of the nearest `AXScrollArea` on the hit-tested path.
/// It moves exactly as far as the content scrolls. `None` when accessibility
/// exposes no scroll area there (a pixel-only surface).
fn scroll_position(pid: i32, x: f64, y: f64) -> Option<(f64, f64)> {
    let _budget = crate::ax::budget::WalkBudget::new(std::time::Duration::from_millis(300));
    unsafe {
        let mut element = element_at_screen_position(pid, x, y)?;
        for _ in 0..32 {
            let Some(parent) = copy_element_attr(element, "AXParent") else {
                CFRelease(element as CFTypeRef);
                return None;
            };
            if copy_string_attr(parent, "AXRole").as_deref() == Some("AXScrollArea") {
                let rect = element_screen_rect(element);
                CFRelease(element as CFTypeRef);
                CFRelease(parent as CFTypeRef);
                return rect.map(|[x, y, _, _]| (x, y));
            }
            CFRelease(element as CFTypeRef);
            element = parent;
        }
        CFRelease(element as CFTypeRef);
        None
    }
}

/// A move the accessibility scroll position shows (at least 1 pt the way
/// asked) when the window's pixels did not: some windows do not paint the
/// scrolled view into what captures see.
fn position_moved(
    direction: ScrollDirection,
    before: Option<(f64, f64)>,
    after: Option<(f64, f64)>,
) -> Option<Classified> {
    let ((bx, by), (ax, ay)) = (before?, after?);
    let along = match direction {
        ScrollDirection::Down => by - ay,
        ScrollDirection::Up => ay - by,
        ScrollDirection::Right => bx - ax,
        ScrollDirection::Left => ax - bx,
    };
    (along >= 1.0).then(|| Classified {
        kind: OutcomeKind::Moved,
        along: along.round() as i32,
        across: 0,
        confidence: None,
        bounced: false,
        ambiguous: false,
        uncalibrated: true,
    })
}

async fn background(request: Request, args: &Value) -> ToolResult {
    let Request {
        pid,
        wid,
        point,
        direction,
        unit,
        amount,
    } = request;
    let (ticks, lines) = scroll_motion::background_ticks(unit, amount);
    let delta = LINE_TICK_DELTA * lines as i32;
    let (delta_y, delta_x) = match direction {
        ScrollDirection::Down => (-delta, 0),
        ScrollDirection::Up => (delta, 0),
        ScrollDirection::Right => (0, -delta),
        ScrollDirection::Left => (0, delta),
    };
    let prior_front = apps::frontmost_pid();
    let snapshot = WindowChangeDetector::snapshot(prior_front);
    let result = focus_guard::with_focus_suppressed(
        Some(pid),
        prior_front,
        "scroll.CGScrollWheel",
        || async move {
            cua_driver_core::operation::spawn_blocking(move || -> anyhow::Result<Verdict> {
                let (area, _) = request.area();
                let sampler = FrameSampler::start(wid, area, point.local);
                crate::input::mouse::scroll_wheel_at_xy(
                    pid,
                    point.screen.0,
                    point.screen.1,
                    Some(point.local),
                    Some(wid),
                    delta_y,
                    delta_x,
                    ticks as usize,
                )?;
                let sampler = match sampler {
                    Ok(sampler) => sampler,
                    Err(reason) => return Ok(Verdict::Unmeasured(reason)),
                };
                sampler.mark_event();
                Ok(match sampler.wait_settled() {
                    Settle::Unmeasured(reason) => Verdict::Unmeasured(reason),
                    Settle::Settled | Settle::Capped => {
                        Verdict::Measured(scroll_motion::classify(&sampler.finish(), direction))
                    }
                })
            })
            .await
        },
    )
    .await;
    let changes = super::finish_window_observation(snapshot, args).await;
    match result {
        Ok(Ok(verdict)) => Report {
            delivery: ScrollDelivery::Background,
            direction,
            point: point.report,
            requested_pt: None,
            wheel_unit: ScrollWheelUnit::Line,
            events: ticks,
            total: ticks * lines,
            chunks: 1,
            verdict,
            stopped: None,
            from_accessibility: false,
            stroke: None,
        }
        .result(&changes),
        Ok(Err(e)) => ToolResult::error(format!("Wheel scroll failed: {e}")),
        Err(e) => ToolResult::error(format!("Task error: {e}")),
    }
}

async fn desktop_scroll(args: &Value) -> ToolResult {
    let input = match parse_typed_projection::<ScrollInput>("scroll", args) {
        Ok(input) => input,
        Err(result) => return result,
    };
    let (x, y) = (input.x, input.y);
    let direction = input.direction.as_str();
    let by = input.by.unwrap_or(ScrollBy::Line).as_str();
    let amount = ScrollUnit::Line.clamp_amount(input.amount.unwrap_or(3)) as usize;
    let step = if input.by == Some(ScrollBy::Page) {
        DESKTOP_STEP_PAGE_PX
    } else {
        DESKTOP_STEP_LINE_PX
    };
    let (delta_y, delta_x) = match input.direction {
        ScrollDirection::Down => (-step, 0),
        ScrollDirection::Up => (step, 0),
        ScrollDirection::Right => (0, -step),
        ScrollDirection::Left => (0, step),
    };
    let (x, y) = match super::desktop_screenshot_point(x, y).await {
        Ok(point) => point,
        Err(error) => return error,
    };
    let result = cua_driver_core::operation::spawn_blocking(move || {
        crate::input::mouse::scroll_wheel_desktop(x, y, delta_y, delta_x, amount)
    })
    .await;
    match result {
        Ok(Ok(())) => ToolResult::text(format!(
            "Scrolled desktop {direction} by {by} × {amount} at ({x:.1}, {y:.1})."
        ))
        .with_structured(serde_json::json!({
            "scope": "desktop",
            "path": "hid",
            "effect": "unverifiable"
        })),
        Ok(Err(error)) => ToolResult::error(format!("desktop scroll failed: {error}")),
        Err(error) => ToolResult::error(format!("desktop scroll task failed: {error}")),
    }
}

/// A screen point in the window's screenshot pixels (the `x`/`y` space).
fn screenshot_point(
    frame: &super::px_frame::WindowPxFrame,
    (sx, sy): (f64, f64),
    ratio: Option<f64>,
) -> (f64, f64) {
    let ratio = ratio.unwrap_or(1.0);
    (
        (sx - frame.content.x) * frame.scale / ratio,
        (sy - frame.content.y) * frame.scale / ratio,
    )
}

/// The innermost `AXScrollArea` containing a screen point, as `[x, y, w, h]`
/// screen points. `None` for a window whose accessibility tree has no scroll
/// area there (a pixel-only surface).
fn scroll_area_at(pid: i32, x: f64, y: f64) -> Option<[f64; 4]> {
    let _budget = crate::ax::budget::WalkBudget::new(std::time::Duration::from_millis(500));
    unsafe { scroll_area_from(element_at_screen_position(pid, x, y)?) }
}

/// The frame of `element` itself or its nearest `AXScrollArea` ancestor.
/// Takes ownership of `element` (one reference) and releases it.
unsafe fn scroll_area_from(mut element: AXUIElementRef) -> Option<[f64; 4]> {
    for _ in 0..32 {
        if copy_string_attr(element, "AXRole").as_deref() == Some("AXScrollArea") {
            let rect = element_screen_rect(element);
            CFRelease(element as CFTypeRef);
            return rect;
        }
        let parent = copy_element_attr(element, "AXParent");
        CFRelease(element as CFTypeRef);
        element = parent?;
    }
    CFRelease(element as CFTypeRef);
    None
}

/// The centre of the part of `element` (screen `[x, y, w, h]`) that is
/// visible: inside its enclosing scroll area and the window. `None` when
/// nothing of it is visible.
fn visible_centre(
    element: [f64; 4],
    area: Option<[f64; 4]>,
    window: &WindowBounds,
) -> Option<(f64, f64)> {
    let mut rect = [
        element[0],
        element[1],
        element[0] + element[2],
        element[1] + element[3],
    ];
    let mut clip = |[x, y, w, h]: [f64; 4]| {
        rect = [rect[0].max(x), rect[1].max(y), rect[2].min(x + w), rect[3].min(y + h)];
    };
    if let Some(area) = area {
        clip(area);
    }
    clip([window.x, window.y, window.width, window.height]);
    (rect[2] - rect[0] >= 1.0 && rect[3] - rect[1] >= 1.0)
        .then(|| ((rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0))
}

fn not_sent_refusal(request: &Request, reason: &str) -> ToolResult {
    let (x, y) = request.point.report.unwrap_or(request.point.local);
    ToolResult::error(format!(
        "scroll not sent at ({x:.0}, {y:.0}): {reason}; no wheel event was posted"
    ))
    .with_structured(serde_json::json!({
        "code": "scroll_not_sent",
        "window_id": request.wid,
        "point": { "x": x, "y": y },
        "reason": reason,
    }))
}

fn covered_refusal(request: &Request, covered: &raised_pointer::Covered) -> ToolResult {
    let (x, y) = request.point.report.unwrap_or(request.point.local);
    let wid = request.wid;
    let (message, covered_by) = match &covered.owner {
        Some((owner, owner_pid)) => (
            format!(
                "scroll refused: window {wid} stays covered by {owner} (pid {owner_pid}) at \
                 ({x:.0}, {y:.0}); no input was sent"
            ),
            serde_json::json!({ "app_name": owner, "pid": owner_pid }),
        ),
        None => (
            format!(
                "scroll refused: window {wid} is not on screen under ({x:.0}, {y:.0}); no input \
                 was sent"
            ),
            Value::Null,
        ),
    };
    ToolResult::error(message).with_structured(serde_json::json!({
        "code": "target_covered",
        "window_id": wid,
        "point": { "x": x, "y": y },
        "covered_by": covered_by,
    }))
}

/// What the frames said.
#[derive(Debug, Clone, PartialEq)]
enum Verdict {
    Measured(Classified),
    Unmeasured(String),
}

/// Everything a scroll reply states.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    delivery: ScrollDelivery,
    direction: ScrollDirection,
    point: Option<(f64, f64)>,
    requested_pt: Option<u32>,
    wheel_unit: ScrollWheelUnit,
    events: u32,
    total: u32,
    chunks: u32,
    verdict: Verdict,
    /// Why the gesture stopped before its distance (the user took over, the
    /// window was covered or moved, input failed); the measured result is
    /// then partial.
    stopped: Option<String>,
    /// The distance was read from the accessibility scroll position; the
    /// window's pixels did not show the move, so it is not frame evidence.
    from_accessibility: bool,
    /// Set for a `motion:"stroke"` scroll.
    stroke: Option<StrokeShape>,
}

const NO_MOTION: &str = "no displacement observed — the view may be at its end, or nothing \
     under this point scrolls with the wheel";
const BACKGROUND_RETRY: &str = "if it is not at its end, retry with \
     delivery_mode:\"foreground\" (background wheels do not reach views that only scroll under \
     the real pointer)";

fn opposite(direction: ScrollDirection) -> ScrollDirection {
    match direction {
        ScrollDirection::Up => ScrollDirection::Down,
        ScrollDirection::Down => ScrollDirection::Up,
        ScrollDirection::Left => ScrollDirection::Right,
        ScrollDirection::Right => ScrollDirection::Left,
    }
}

impl Report {
    fn outcome(&self) -> ScrollOutcomeKind {
        match &self.verdict {
            Verdict::Unmeasured(_) => ScrollOutcomeKind::Unmeasured,
            Verdict::Measured(c) => match c.kind {
                OutcomeKind::Moved => ScrollOutcomeKind::Moved,
                OutcomeKind::AtEnd => ScrollOutcomeKind::AtEnd,
                OutcomeKind::NoMotion => ScrollOutcomeKind::NoMotion,
                OutcomeKind::ChangedInPlace => ScrollOutcomeKind::ChangedInPlace,
            },
        }
    }

    /// The scroll reached its postcondition: the content moved the way asked.
    /// A stroke whose landing rests on a snap reading or on tracked travel
    /// alone is never confirmed.
    fn confirmed(&self) -> bool {
        if self.from_accessibility {
            return false;
        }
        if self
            .stroke
            .is_some_and(|shape| shape.snapped_from.is_some() || shape.reshaped_header)
        {
            return false;
        }
        match &self.verdict {
            Verdict::Measured(c) => match c.kind {
                OutcomeKind::Moved => c.along > 0,
                OutcomeKind::AtEnd => c.along >= 0,
                _ => false,
            },
            Verdict::Unmeasured(_) => false,
        }
    }

    fn at(&self) -> String {
        match self.point {
            Some((x, y)) => format!("({x:.0}, {y:.0})"),
            None => "the element".to_owned(),
        }
    }

    fn wheel(&self) -> String {
        match self.delivery {
            ScrollDelivery::Foreground if self.stroke.is_some() => {
                format!("foreground pointer stroke, {} px", self.total)
            }
            ScrollDelivery::Foreground => format!(
                "foreground pointer wheel, {} chunk{}, {} px",
                self.chunks,
                if self.chunks == 1 { "" } else { "s" },
                self.total
            ),
            ScrollDelivery::Background => format!(
                "background line wheel, {} tick{} = {} line{}",
                self.events,
                if self.events == 1 { "" } else { "s" },
                self.total,
                if self.total == 1 { "" } else { "s" }
            ),
        }
    }

    fn reason(&self) -> Option<String> {
        let base = match (&self.verdict, self.delivery) {
            (Verdict::Unmeasured(reason), _) => Some(format!("capture unavailable: {reason}")),
            (Verdict::Measured(c), ScrollDelivery::Background) if c.kind == OutcomeKind::NoMotion => {
                Some(format!("{NO_MOTION}; {BACKGROUND_RETRY}"))
            }
            (Verdict::Measured(c), ScrollDelivery::Foreground) if c.kind == OutcomeKind::NoMotion => {
                Some(NO_MOTION.to_owned())
            }
            _ if self.from_accessibility => Some(
                "measured from the accessibility scroll position: the window's pixels did not show the move"
                    .to_owned(),
            ),
            _ => self.shape_reason(),
        };
        match (base, &self.stopped) {
            (Some(base), Some(stop)) => Some(format!("{base}; stopped early: {stop}")),
            (None, Some(stop)) => Some(format!("stopped early: {stop}")),
            (base, None) => base,
        }
    }

    /// What a stroke's frames showed beyond its landing, for `reason`.
    fn shape_reason(&self) -> Option<String> {
        let shape = self.stroke?;
        let Verdict::Measured(c) = &self.verdict else {
            return None;
        };
        if let Some(peak) = shape.snapped_from {
            return Some(format!(
                "the view followed the stroke {peak} pt, then settled back to {} pt: a snap \
                 point or the view's end",
                c.along
            ));
        }
        shape.reshaped_header.then(|| {
            "the distance is the travel tracked frame to frame, short by any frame pair that \
             failed to register: the first and last frames do not register as one shift (part \
             of the view, such as a large title, changed shape)"
                .to_owned()
        })
    }

    fn text(&self) -> String {
        let stop = self
            .stopped
            .as_ref()
            .map(|stop| format!(" Stopped early: {stop}."))
            .unwrap_or_default();
        let witness = if self.from_accessibility {
            " Measured from the accessibility scroll position: the window's pixels did not show \
             the move."
        } else {
            ""
        };
        let shape = match self.stroke {
            Some(StrokeShape { snapped_from: Some(peak), .. }) if self.snapped_back().is_none() => {
                format!(
                    " The view followed the stroke {peak} pt, then settled back here: a snap \
                     point or the view's end."
                )
            }
            Some(StrokeShape { reshaped_header: true, .. }) => {
                " Distance tracked frame to frame (short by any frame pair that failed to \
                 register): part of the view (a large title) changed shape while it moved."
                    .to_owned()
            }
            _ => String::new(),
        };
        format!("{}{shape}{witness}{stop}", self.verdict_text())
    }

    /// A stroke the view followed and then settled back from to where it
    /// started: `(peak, landing)`.
    fn snapped_back(&self) -> Option<(i32, i32)> {
        match (&self.verdict, self.stroke?.snapped_from) {
            (Verdict::Measured(c), Some(peak))
                if c.kind == OutcomeKind::Moved && c.along <= 0 =>
            {
                Some((peak, c.along))
            }
            _ => None,
        }
    }

    fn verdict_text(&self) -> String {
        let direction = self.direction.as_str();
        let at = self.at();
        let wheel = self.wheel();
        let requested = self
            .requested_pt
            .map(|pt| format!("requested {pt}; "))
            .unwrap_or_default();
        if let Some((peak, landing)) = self.snapped_back() {
            return format!(
                "? Snapped back at {at}: the view followed the stroke {peak} pt {direction}, \
                 then settled back where it started, a snap point or the view's end ({landing} \
                 pt; {requested}{wheel})"
            );
        }
        match &self.verdict {
            Verdict::Unmeasured(reason) => {
                let asked = self
                    .requested_pt
                    .map(|pt| format!(" (requested {pt} pt)"))
                    .unwrap_or_default();
                format!(
                    "? Unmeasured: scrolled {direction}{asked} at {at}, movement not measured \
                     ({wheel}; capture unavailable: {reason})"
                )
            }
            Verdict::Measured(c) => match c.kind {
                OutcomeKind::Moved if c.along > 0 => format!(
                    "{} Scrolled {direction} {} pt at {at} ({requested}{wheel})",
                    if self.stroke.is_some() && !self.confirmed() { "?" } else { "✓" },
                    c.along
                ),
                OutcomeKind::Moved | OutcomeKind::AtEnd if c.along < 0 => format!(
                    "? Moved the other way: the view scrolled {} {} pt at {at} \
                     ({requested}{wheel})",
                    opposite(self.direction).as_str(),
                    -c.along
                ),
                OutcomeKind::Moved | OutcomeKind::AtEnd => {
                    let of = self
                        .requested_pt
                        .map(|pt| format!(" of {pt}"))
                        .unwrap_or_default();
                    let how = if c.bounced { "bounced" } else { "stopped" };
                    format!(
                        "✓ At end: scrolled {direction} {}{of} pt at {at}, then the view \
                         {how} ({wheel})",
                        c.along
                    )
                }
                OutcomeKind::NoMotion => match self.delivery {
                    ScrollDelivery::Background => format!(
                        "✗ No motion at {at}: {NO_MOTION} ({wheel}); {BACKGROUND_RETRY}."
                    ),
                    ScrollDelivery::Foreground => {
                        format!("✗ No motion at {at}: {NO_MOTION} ({requested}{wheel}).")
                    }
                },
                OutcomeKind::ChangedInPlace => format!(
                    "? Changed in place at {at}: pixels changed but nothing shifted (a pager, \
                     sheet or navigation) ({wheel}). Re-observe before the next coordinate \
                     action."
                ),
            },
        }
    }

    fn structured(&self) -> Value {
        let (moved, across, confidence) = match &self.verdict {
            Verdict::Measured(c) => (Some(c.along), Some(c.across), c.confidence),
            Verdict::Unmeasured(_) => (None, None, None),
        };
        let outcome = self.outcome();
        let effect = if self.confirmed() {
            "confirmed"
        } else if outcome == ScrollOutcomeKind::NoMotion {
            "suspected_noop"
        } else {
            "unverifiable"
        };
        let foreground = self.delivery == ScrollDelivery::Foreground;
        let mut scroll = serde_json::json!({
            "delivery": if foreground { "foreground" } else { "background" },
            "direction": self.direction.as_str(),
            "point": self.point.map(|(x, y)| serde_json::json!({ "x": x, "y": y })),
            "requested_pt": self.requested_pt,
            "wheel": {
                "unit": if self.wheel_unit == ScrollWheelUnit::Pixel { "pixel" } else { "line" },
                "events": self.events,
                "total": self.total,
            },
            "chunks": self.chunks,
            "outcome": serde_json::to_value(outcome).unwrap_or(Value::Null),
            "moved_pt": moved,
            "across_pt": across,
            "confidence": confidence.map(|c| (c * 100.0).round() / 100.0),
        });
        if let Some(reason) = self.reason() {
            scroll["reason"] = Value::String(reason);
        }
        let mut structured = serde_json::json!({
            "path": if foreground { "hid_pointer_wheel" } else { "pid_line_wheel" },
            "delivery_mode": if foreground { "foreground" } else { "background" },
            "verified": self.confirmed(),
            "effect": effect,
            "scroll": scroll,
        });
        if self.confirmed() {
            structured["evidence"] = serde_json::json!([{ "kind": "frame_motion" }]);
        }
        if outcome == ScrollOutcomeKind::NoMotion && !foreground {
            structured["escalation"] =
                serde_json::json!({ "target": "foreground", "reason": "suspected_noop" });
        }
        structured
    }

    fn result(&self, changes: &crate::window_change_detector::Changes) -> ToolResult {
        let mut structured = self.structured();
        changes.publish_gained_windows(&mut structured);
        ToolResult::text(format!("{}{}", self.text(), changes.result_suffix()))
            .with_structured(structured)
    }
}

unsafe fn scroll_native_text_area(
    element: AXUIElementRef,
    direction: ScrollDirection,
    unit: ScrollUnit,
    amount: usize,
) -> bool {
    if copy_string_attr(element, "AXRole").as_deref() != Some("AXTextArea") {
        return false;
    }
    let Some(scroll_area) = copy_element_attr(element, "AXParent") else {
        return false;
    };
    if copy_string_attr(scroll_area, "AXRole").as_deref() != Some("AXScrollArea") {
        CFRelease(scroll_area as CFTypeRef);
        return false;
    }
    let mut buttons = Vec::new();
    collect_ax_buttons(scroll_area, 0, &mut buttons);
    CFRelease(scroll_area as CFTypeRef);
    if buttons.is_empty() {
        return false;
    }

    let reverse = direction == ScrollDirection::Up;
    let base = if unit == ScrollUnit::Page && buttons.len() >= 4 {
        2
    } else {
        0
    };
    let index = base + usize::from(reverse);
    let mut delivered = false;
    if let Some(target) = buttons.get(index).copied() {
        for _ in 0..amount.max(1) {
            if perform_action(target, "AXPress") != kAXErrorSuccess {
                break;
            }
            delivered = true;
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    for button in buttons {
        CFRelease(button as CFTypeRef);
    }
    delivered
}

unsafe fn collect_ax_buttons(
    element: AXUIElementRef,
    depth: usize,
    buttons: &mut Vec<AXUIElementRef>,
) {
    if depth >= 4 || buttons.len() >= 4 {
        return;
    }
    for child in copy_children(element) {
        if buttons.len() >= 4 {
            CFRelease(child as CFTypeRef);
        } else if copy_string_attr(child, "AXRole").as_deref() == Some("AXButton") {
            buttons.push(child);
        } else {
            collect_ax_buttons(child, depth + 1, buttons);
            CFRelease(child as CFTypeRef);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, BackgroundTargetFacts,
        ElementAncestry, ExactWindowTarget, WindowServerOwnership,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn exact_target_refusal_prevents_ax_reveal() {
        let action_ran = AtomicBool::new(false);
        let target = ExactWindowTarget {
            pid: 42,
            window_id: 7,
        };
        let facts = BackgroundTargetFacts {
            window_server: WindowServerOwnership::SamePid,
            ax_window_present: true,
            target_minimized: Some(false),
            app_hidden: Some(false),
            competing_keyboard_destinations: Vec::new(),
            element: ElementAncestry::OutsideTargetWindow {
                pid: Some(42),
                window_id: 8,
            },
        };
        let refusal = match decide_background_input(target, &facts, BackgroundAction::AxSemantic) {
            BackgroundInputDecision::Refuse(refusal) => Err(
                super::super::background_refusal_result(target.pid, target.window_id, &refusal),
            ),
            BackgroundInputDecision::Execute { .. } => panic!("exact-target facts must refuse"),
        };

        let result = after_exact_target_gate(refusal, || {
            action_ran.store(true, Ordering::SeqCst);
        });

        assert!(result.is_err());
        assert!(!action_ran.load(Ordering::SeqCst));
    }

    /// Before: a scroll with no element and no x/y pressed arrow or PageDown
    /// keys at the focused control, which pixel-only surfaces ignore. Now it
    /// is a wheel at the centre of the window, in screen and window points.
    #[test]
    fn an_untargeted_scroll_wheels_at_the_window_centre() {
        let bounds = WindowBounds {
            x: 716.0,
            y: 193.0,
            width: 326.0,
            height: 720.0,
        };
        let point = window_centre(&bounds);
        assert_eq!(point.screen, (879.0, 553.0));
        assert_eq!(point.local, (163.0, 360.0));
    }

    fn classified(kind: OutcomeKind, along: i32, bounced: bool) -> Verdict {
        Verdict::Measured(Classified {
            kind,
            along,
            across: 0,
            confidence: matches!(kind, OutcomeKind::Moved | OutcomeKind::AtEnd).then_some(0.934),
            bounced,
            ambiguous: false,
            uncalibrated: false,
        })
    }

    fn foreground(verdict: Verdict) -> Report {
        Report {
            delivery: ScrollDelivery::Foreground,
            direction: ScrollDirection::Down,
            point: Some((163.0, 400.0)),
            requested_pt: Some(231),
            wheel_unit: ScrollWheelUnit::Pixel,
            events: 10,
            total: 300,
            chunks: 2,
            verdict,
            stopped: None,
            from_accessibility: false,
            stroke: None,
        }
    }

    fn background(verdict: Verdict) -> Report {
        Report {
            delivery: ScrollDelivery::Background,
            direction: ScrollDirection::Down,
            point: Some((163.0, 400.0)),
            requested_pt: None,
            wheel_unit: ScrollWheelUnit::Line,
            events: 3,
            total: 3,
            chunks: 1,
            verdict,
            stopped: None,
            from_accessibility: false,
            stroke: None,
        }
    }

    #[test]
    fn a_measured_move_is_a_confirmed_pointer_wheel_in_points() {
        let report = foreground(classified(OutcomeKind::Moved, 231, false));
        assert_eq!(
            report.text(),
            "✓ Scrolled down 231 pt at (163, 400) (requested 231; foreground pointer wheel, 2 \
             chunks, 300 px)"
        );
        let structured = report.structured();
        assert_eq!(structured["path"], "hid_pointer_wheel");
        assert_eq!(structured["effect"], "confirmed");
        assert_eq!(structured["verified"], true);
        assert_eq!(structured["evidence"], serde_json::json!([{ "kind": "frame_motion" }]));
        let scroll: cua_driver_contract::ScrollOutcome =
            serde_json::from_value(structured["scroll"].clone()).expect("contract shape");
        assert_eq!(scroll.moved_pt, Some(231));
        assert_eq!(scroll.requested_pt, Some(231));
        assert_eq!(scroll.confidence, Some(0.93));
        assert_eq!(scroll.wheel.unit, ScrollWheelUnit::Pixel);
        assert_eq!(scroll.outcome, ScrollOutcomeKind::Moved);
    }

    #[test]
    fn a_bounce_reads_as_at_end_with_the_distance_moved() {
        let report = foreground(classified(OutcomeKind::AtEnd, 58, true));
        assert!(report
            .text()
            .starts_with("✓ At end: scrolled down 58 of 231 pt at (163, 400), then the view bounced"));
        assert_eq!(report.structured()["effect"], "confirmed");
    }

    fn stroke(verdict: Verdict, shape: StrokeShape) -> Report {
        Report {
            chunks: 1,
            stroke: Some(shape),
            ..foreground(verdict)
        }
    }

    /// A snapping pager: the reply states where the view landed, as a move a
    /// caller reads through `moved_pt`, but neither the snap nor a
    /// reshaped header's tracked-only distance is confirmed.
    #[test]
    fn a_snapped_or_reshaped_stroke_is_a_move_to_its_landing_left_unconfirmed() {
        let snapped = StrokeShape { snapped_from: Some(210), reshaped_header: false };
        let report = stroke(classified(OutcomeKind::Moved, 151, false), snapped);
        let text = report.text();
        assert!(text.starts_with("? Scrolled down 151 pt at (163, 400)"), "{text}");
        let structured = report.structured();
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["verified"], false);
        let scroll: cua_driver_contract::ScrollOutcome =
            serde_json::from_value(structured["scroll"].clone()).expect("contract shape");
        assert_eq!((scroll.outcome, scroll.moved_pt, scroll.chunks), (ScrollOutcomeKind::Moved, Some(151), 1));
        let reason = scroll.reason.unwrap_or_default();
        assert!(reason.contains("210 pt") && reason.contains("snap point or the view's end"), "{reason}");

        let reshaped = StrokeShape { snapped_from: None, reshaped_header: true };
        let report = stroke(classified(OutcomeKind::Moved, 151, false), reshaped);
        assert_eq!(report.structured()["effect"], "unverifiable");
        let reason = report.reason().unwrap_or_default();
        assert!(reason.contains("tracked frame to frame"), "{reason}");

        let clean = stroke(classified(OutcomeKind::Moved, 151, false), StrokeShape::default());
        assert_eq!(clean.structured()["effect"], "confirmed");
        assert!(clean.text().starts_with("✓ Scrolled down 151 pt"));
    }

    #[test]
    fn a_stroke_is_refused_off_a_foreground_window_scroll_and_motion_is_checked_first() {
        let stroke = Value::String("stroke".into());
        assert_eq!(parse_motion(Some(&stroke), true, false), Ok(true));
        assert!(parse_motion(Some(&stroke), false, false).is_err(), "background");
        assert!(parse_motion(Some(&stroke), true, true).is_err(), "desktop scope");
        assert_eq!(parse_motion(None, false, true), Ok(false));
        assert_eq!(parse_motion(Some(&Value::String("stepped".into())), false, false), Ok(false));
        assert!(parse_motion(Some(&serde_json::json!(true)), true, false).is_err());
        assert!(parse_motion(Some(&Value::String("fling".into())), true, false).is_err());
    }

    #[test]
    fn a_strokes_calibration_never_sizes_a_stepped_scroll() {
        let (stepped, strokes) = (ScrollCalibrations::default(), ScrollCalibrations::default());
        assert_eq!(starting_calibration(&stepped, &strokes, true, 7, 9), (1.0, false));
        record_calibration(&stepped, &strokes, true, 7, 9, 0.5);
        assert_eq!(starting_calibration(&stepped, &strokes, false, 7, 9), (1.0, false));
        assert_eq!(starting_calibration(&stepped, &strokes, true, 7, 9), (0.5, true));
        record_calibration(&stepped, &strokes, false, 7, 9, 0.8);
        assert_eq!(starting_calibration(&stepped, &strokes, false, 7, 9), (0.8, true));
        assert_eq!(starting_calibration(&stepped, &strokes, true, 7, 9), (0.5, true), "own first");
        assert_eq!(starting_calibration(&stepped, &strokes, true, 7, 10), (1.0, false));
    }

    #[test]
    fn a_stroke_that_snapped_back_is_neither_at_end_nor_confirmed() {
        let shape = StrokeShape { snapped_from: Some(200), reshaped_header: false };
        let report = stroke(classified(OutcomeKind::Moved, 0, false), shape);
        let text = report.text();
        assert!(
            text.starts_with("? Snapped back at (163, 400): the view followed the stroke 200 pt down"),
            "{text}"
        );
        assert!(!text.contains("At end"), "{text}");
        let structured = report.structured();
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["scroll"]["outcome"], "moved");
        assert_eq!(structured["scroll"]["moved_pt"], 0);
    }

    #[test]
    fn background_no_motion_names_the_foreground_retry_and_never_says_pixel() {
        let report = background(classified(OutcomeKind::NoMotion, 0, false));
        let text = report.text();
        assert!(text.starts_with("✗ No motion at (163, 400)"), "{text}");
        assert!(text.contains("the view may be at its end"), "{text}");
        assert!(text.contains("if it is not at its end, retry with delivery_mode:\"foreground\""), "{text}");
        assert!(text.contains("background line wheel, 3 ticks = 3 lines"), "{text}");
        assert!(!text.contains("pixel"), "{text}");
        let structured = report.structured();
        assert_eq!(structured["path"], "pid_line_wheel");
        assert_eq!(structured["effect"], "suspected_noop");
        assert_eq!(structured["escalation"]["target"], "foreground");
        assert_eq!(structured["scroll"]["requested_pt"], Value::Null);
        assert_eq!(structured["scroll"]["wheel"]["unit"], "line");
    }

    #[test]
    fn foreground_replies_never_claim_background_delivery() {
        for verdict in [
            classified(OutcomeKind::Moved, 120, false),
            classified(OutcomeKind::NoMotion, 0, false),
            classified(OutcomeKind::ChangedInPlace, 0, false),
            Verdict::Unmeasured("window 7 returned no image".into()),
        ] {
            let text = foreground(verdict).text();
            assert!(!text.contains("background"), "{text}");
        }
    }

    #[test]
    fn unmeasured_and_in_place_outcomes_stay_unverifiable_with_null_numbers() {
        let unmeasured = foreground(Verdict::Unmeasured("window 7 returned no image".into()));
        assert!(unmeasured.text().starts_with("? Unmeasured: scrolled down (requested 231 pt)"));
        let structured = unmeasured.structured();
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["scroll"]["moved_pt"], Value::Null);
        assert_eq!(structured["scroll"]["confidence"], Value::Null);
        assert_eq!(
            structured["scroll"]["reason"],
            "capture unavailable: window 7 returned no image"
        );

        let pager = foreground(classified(OutcomeKind::ChangedInPlace, 0, false));
        assert!(pager.text().starts_with("? Changed in place at (163, 400)"));
        assert_eq!(pager.structured()["effect"], "unverifiable");
        assert!(pager.structured().get("evidence").is_none());
    }

    #[test]
    fn a_move_against_the_request_is_not_confirmed() {
        let report = foreground(classified(OutcomeKind::Moved, -80, false));
        assert!(report.text().starts_with("? Moved the other way: the view scrolled up 80 pt"));
        assert_eq!(report.structured()["effect"], "unverifiable");
    }

    /// Live on TextEdit (700 pt window): `by:"page"` asked for 1603 pt
    /// because the scroll area's frame ran past the window. A page is 0.8 of
    /// what is visible.
    #[test]
    fn a_page_is_sized_from_the_scroll_area_clipped_to_the_window() {
        let window = WindowBounds {
            x: 100.0,
            y: 50.0,
            width: 600.0,
            height: 700.0,
        };
        let (area, visible) = visible_area(Some([100.0, 78.0, 600.0, 2004.0]), &window);
        assert_eq!(area, Some([0.0, 28.0, 600.0, 672.0]));
        assert_eq!(visible, 672.0);
        assert_eq!(visible_area(None, &window), (None, 700.0));
        assert_eq!(visible_area(Some([900.0, 0.0, 50.0, 50.0]), &window), (None, 700.0));
    }

    /// AppKit harness: a 460×600 text view inside a 120 pt scroller. The
    /// centre of the text view's own frame lies below the scroller, so the
    /// wheel landed on whatever was under it.
    #[test]
    fn an_element_scrolls_at_the_centre_of_its_visible_part() {
        let window = WindowBounds {
            x: 0.0,
            y: 0.0,
            width: 600.0,
            height: 800.0,
        };
        let text_view = [20.0, 100.0, 460.0, 600.0];
        let scroller = [20.0, 100.0, 480.0, 120.0];
        assert_eq!(visible_centre(text_view, Some(scroller), &window), Some((250.0, 160.0)));
        assert_eq!(visible_centre(text_view, None, &window), Some((250.0, 400.0)));
        assert_eq!(visible_centre([700.0, 0.0, 50.0, 50.0], None, &window), None);
    }

    /// AppKit harness (VM and local): the 120 pt scroller moved 72 pt
    /// (`scroll_offset=72`) while the captured pixels of that window never
    /// changed, and the reply said "No motion". The document's AX origin
    /// moving up is the witness then, reported as such and never confirmed.
    #[test]
    fn a_scroll_the_pixels_miss_is_measured_from_the_document_position() {
        let moved = position_moved(ScrollDirection::Down, Some((535.0, 572.0)), Some((535.0, 500.0)))
            .unwrap();
        assert_eq!((moved.kind, moved.along), (OutcomeKind::Moved, 72));
        assert!(position_moved(ScrollDirection::Up, Some((535.0, 572.0)), Some((535.0, 500.0))).is_none());
        assert!(position_moved(ScrollDirection::Down, None, Some((535.0, 500.0))).is_none());

        let mut report = foreground(Verdict::Measured(moved));
        report.from_accessibility = true;
        assert!(report.text().contains("accessibility scroll position"));
        assert_eq!(report.structured()["effect"], "unverifiable");
        assert_eq!(report.structured()["scroll"]["outcome"], "moved");
        assert!(report.structured()["scroll"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("accessibility scroll position")));
    }

    #[test]
    fn the_primer_approaches_an_edge_point_from_inside_the_window() {
        let window = WindowBounds {
            x: 716.0,
            y: 193.0,
            width: 326.0,
            height: 720.0,
        };
        assert_eq!(toward_interior((717.0, 900.0), &window), (1.0, -1.0));
        assert_eq!(toward_interior((1040.0, 200.0), &window), (-1.0, 1.0));
    }

    /// Reviewer M4: a view at its end holds still exactly like one that does
    /// not take wheels, so the reply names both possibilities.
    #[test]
    fn foreground_no_motion_does_not_assert_a_cause() {
        let report = foreground(classified(OutcomeKind::NoMotion, 0, false));
        let text = report.text();
        assert!(text.contains("no displacement observed"), "{text}");
        assert!(text.contains("may be at its end"), "{text}");
        assert!(!text.contains("so nothing under this point"), "{text}");
        assert_eq!(report.structured()["scroll"]["reason"], NO_MOTION);
    }

    #[test]
    fn a_stopped_gesture_reports_its_partial_distance_and_why() {
        let mut report = foreground(classified(OutcomeKind::Moved, 120, false));
        report.stopped = Some("the pointer moved to (10, 10) during the scroll; the user has it".into());
        let text = report.text();
        assert!(text.starts_with("✓ Scrolled down 120 pt"), "{text}");
        assert!(text.ends_with("Stopped early: the pointer moved to (10, 10) during the scroll; the user has it."));
        assert!(report.structured()["scroll"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("stopped early:"));
    }
}
