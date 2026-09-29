//! Window frames sampled while a scroll runs, tracked as they arrive.
//!
//! Frames come from `CGWindowListCreateImage` at nominal resolution drawn into
//! an 8-bit gray bitmap, so one pixel is one window point. That legacy API is
//! used here for measurement only — the frames never leave the driver and
//! nothing is addressed through their coordinates (screenshots keep the
//! ScreenCaptureKit path) — because it answers synchronously for one window,
//! covered or not, fast enough to sample a gesture (≥ 25 fps measured on
//! macOS 26.1 for a phone-sized window).
//!
//! The sampler thread registers each frame against the previous one as it
//! arrives ([`MotionTracker`]), so only the first, previous and chunk-start
//! frames are held, and decides when the view has settled after the last
//! posted event.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use core_graphics::base::kCGImageAlphaNone;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window::{
    create_image, kCGWindowImageBoundsIgnoreFraming, kCGWindowImageNominalResolution,
    kCGWindowListOptionIncludingWindow,
};

use crate::scroll_motion::{GrayFrame, Motion, MotionTracker, Region};

/// Target sampling period (~30 fps).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// The view is not judged settled sooner than this after the last event.
const SETTLE_AFTER_EVENT: Duration = Duration::from_millis(250);
/// ...and not before it has held still this long.
const SETTLE_QUIET: Duration = Duration::from_millis(300);
/// Hard cap after the last event, for a view that never stops changing.
const SETTLE_CAP: Duration = Duration::from_millis(1200);

/// One gray frame of `window_id`, one pixel per point.
pub fn capture_gray(window_id: u32) -> Result<GrayFrame, String> {
    // SAFETY: `CGRectNull` is an immutable CoreGraphics constant.
    let bounds = unsafe { core_graphics::display::CGRectNull };
    let image = create_image(
        bounds,
        kCGWindowListOptionIncludingWindow,
        window_id,
        kCGWindowImageBoundsIgnoreFraming | kCGWindowImageNominalResolution,
    )
    .ok_or_else(|| format!("window {window_id} returned no image"))?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(format!("window {window_id} returned an empty image"));
    }
    let mut context = CGContext::create_bitmap_context(
        None,
        width,
        height,
        8,
        width,
        &CGColorSpace::create_device_gray(),
        kCGImageAlphaNone,
    );
    context.draw_image(
        CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(width as f64, height as f64),
        ),
        &image,
    );
    let row_bytes = context.bytes_per_row();
    let data = context.data();
    let mut pixels = Vec::with_capacity(width * height);
    for row in data.chunks(row_bytes).take(height) {
        pixels.extend_from_slice(&row[..width]);
    }
    Ok(GrayFrame::new(width, height, pixels))
}

/// How a wait for the view to settle ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settle {
    /// The frames held still after the last event.
    Settled,
    /// The hard cap expired with the view still changing.
    Capped,
    /// No frame could be captured after the last event.
    Unmeasured(String),
}

struct State {
    tracker: MotionTracker,
    stop: bool,
    last_event: Instant,
    last_change: Instant,
    frames_since_event: usize,
    error: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

/// A background thread sampling one window while a scroll runs.
pub struct FrameSampler {
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl FrameSampler {
    /// Capture the first frame now and keep sampling until [`Self::finish`].
    /// `area` is the scroll area in window points (`[x, y, w, h]`), when
    /// accessibility exposes one.
    pub fn start(window_id: u32, area: Option<[f64; 4]>) -> Result<Self, String> {
        let first = capture_gray(window_id)?;
        let region = Region::within(first.width, first.height, area);
        let now = Instant::now();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                tracker: MotionTracker::new(first, region),
                stop: false,
                last_event: now,
                last_change: now,
                frames_since_event: 0,
                error: None,
            }),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("cua-scroll-sampler".into())
            .spawn(move || sample(window_id, &worker_shared))
            .map_err(|error| format!("sampler thread failed to start: {error}"))?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Record that an input event was just posted.
    pub fn mark_event(&self) {
        let mut state = self.shared.lock();
        state.last_event = Instant::now();
        state.frames_since_event = 0;
    }

    /// Start measuring a new chunk from the latest frame.
    pub fn begin_chunk(&self) {
        self.shared.lock().tracker.begin_chunk();
    }

    /// Block until the view settles after the last [`Self::mark_event`].
    pub fn wait_settled(&self) -> Settle {
        let mut state = self.shared.lock();
        loop {
            let now = Instant::now();
            let since_event = now.duration_since(state.last_event);
            if state.frames_since_event > 0
                && since_event >= SETTLE_AFTER_EVENT
                && now.duration_since(state.last_change) >= SETTLE_QUIET
            {
                return Settle::Settled;
            }
            if since_event >= SETTLE_CAP {
                return if state.frames_since_event > 0 {
                    Settle::Capped
                } else {
                    Settle::Unmeasured(
                        state
                            .error
                            .clone()
                            .unwrap_or_else(|| "no frame arrived after the scroll".into()),
                    )
                };
            }
            state = self
                .shared
                .wake
                .wait_timeout(state, Duration::from_millis(20))
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poison| poison.into_inner().0);
        }
    }

    /// Motion since the last [`Self::begin_chunk`].
    pub fn chunk_motion(&self) -> Motion {
        self.shared.lock().tracker.chunk_motion()
    }

    /// Stop sampling and return the motion over the whole call.
    pub fn finish(mut self) -> Motion {
        self.stop();
        self.shared.lock().tracker.call_motion()
    }

    fn stop(&mut self) {
        self.shared.lock().stop = true;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for FrameSampler {
    fn drop(&mut self) {
        self.stop();
    }
}

fn sample(window_id: u32, shared: &Shared) {
    loop {
        let started = Instant::now();
        if shared.lock().stop {
            return;
        }
        let frame = capture_gray(window_id);
        {
            let mut state = shared.lock();
            match frame {
                Ok(frame) => {
                    if state.tracker.push(frame) {
                        state.last_change = Instant::now();
                    }
                    state.frames_since_event += 1;
                }
                Err(error) => state.error = Some(error),
            }
        }
        shared.wake.notify_all();
        if let Some(rest) = FRAME_INTERVAL.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}
