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

/// Timing and health of the sample stream; registration lives behind its own
/// lock so posting input never waits on it.
struct State {
    stop: bool,
    last_event: Instant,
    last_change: Instant,
    /// Successful frames captured since the last event.
    frames_since_event: usize,
    /// Successful frames captured since the sampler started.
    frames: usize,
    /// When the latest successful frame arrived.
    last_success: Instant,
    /// A capture failed after the first frame: the stream has a gap, so
    /// nothing after it can be judged settled.
    failure: Option<String>,
}

/// The settle decision over a snapshot of [`State`]; `None` = keep waiting.
/// Quiet is only evidence while frames keep arriving: settling needs a
/// successful frame after the last event and a latest frame within the quiet
/// window, and a stream that stalled without failing is unmeasured.
fn settle_decision(state: &State, now: Instant) -> Option<Settle> {
    if let Some(failure) = &state.failure {
        return Some(Settle::Unmeasured(format!("capture failed during the scroll: {failure}")));
    }
    let since_event = now.duration_since(state.last_event);
    let fresh = state.frames_since_event > 0
        && state.last_success > state.last_event
        && now.duration_since(state.last_success) <= SETTLE_QUIET;
    if fresh
        && since_event >= SETTLE_AFTER_EVENT
        && now.duration_since(state.last_change) >= SETTLE_QUIET
    {
        return Some(Settle::Settled);
    }
    if since_event >= SETTLE_CAP {
        return Some(if fresh {
            Settle::Capped
        } else if state.frames_since_event > 0 {
            Settle::Unmeasured("frame capture stalled during the scroll".into())
        } else {
            Settle::Unmeasured("no frame arrived after the scroll".into())
        });
    }
    None
}

struct Shared {
    state: Mutex<State>,
    tracker: Mutex<MotionTracker>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn tracker(&self) -> MutexGuard<'_, MotionTracker> {
        self.tracker.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

/// A background thread sampling one window while a scroll runs.
pub struct FrameSampler {
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// Frames sampled before the first event, to tell whether the content was
/// already moving.
const BASELINE_FRAMES: usize = 2;
/// The view must hold still this long before the first event...
const BASELINE_QUIET: Duration = Duration::from_millis(150);
/// ...or the gesture starts anyway after this, with a moving baseline.
const BASELINE_QUIET_CAP: Duration = Duration::from_millis(700);

impl FrameSampler {
    /// Capture the first frame now, sample [`BASELINE_FRAMES`] more before
    /// returning (so content already moving is known before any input), and
    /// keep sampling until [`Self::finish`]. `area` is the region to register
    /// on in window points (`[x, y, w, h]`).
    pub fn start(window_id: u32, area: Option<[f64; 4]>) -> Result<Self, String> {
        let first = capture_gray(window_id)?;
        let region = Region::within(first.width, first.height, area);
        let now = Instant::now();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                stop: false,
                last_event: now,
                last_change: now,
                frames_since_event: 0,
                frames: 0,
                last_success: now,
                failure: None,
            }),
            tracker: Mutex::new(MotionTracker::new(first, region)),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("cua-scroll-sampler".into())
            .spawn(move || sample(window_id, &worker_shared))
            .map_err(|error| format!("sampler thread failed to start: {error}"))?;
        let sampler = Self {
            shared,
            worker: Some(worker),
        };
        // Wait for the view to hold still before the first event: a window
        // just raised redraws as it becomes key, and those frames would read
        // as motion. Content still changing at the deadline is recorded as a
        // moving baseline.
        let deadline = Instant::now() + BASELINE_QUIET_CAP;
        let mut state = sampler.shared.lock();
        let moving = loop {
            if state.failure.is_some() {
                break true;
            }
            let now = Instant::now();
            if state.frames >= BASELINE_FRAMES
                && now.duration_since(state.last_change) >= BASELINE_QUIET
            {
                break false;
            }
            if now >= deadline {
                break true;
            }
            state = sampler
                .shared
                .wake
                .wait_timeout(state, Duration::from_millis(20))
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poison| poison.into_inner().0);
        };
        let failure = state.failure.clone();
        drop(state);
        if let Some(failure) = failure {
            return Err(failure);
        }
        sampler.shared.tracker().begin_gesture(moving);
        Ok(sampler)
    }

    /// Record that an input event was just posted.
    pub fn mark_event(&self) {
        let mut state = self.shared.lock();
        state.last_event = Instant::now();
        state.frames_since_event = 0;
    }

    /// Start measuring a new chunk from the latest frame.
    pub fn begin_chunk(&self) {
        self.shared.tracker().begin_chunk();
    }

    /// Block until the view settles after the last [`Self::mark_event`]: a
    /// fresh successful frame after it, then quiet. A capture failure at any
    /// point after the first frame, or a cancelled operation, is unmeasured.
    pub fn wait_settled(&self) -> Settle {
        let mut state = self.shared.lock();
        loop {
            if cua_driver_core::operation::check().is_err() {
                return Settle::Unmeasured("the operation was cancelled".into());
            }
            if let Some(settle) = settle_decision(&state, Instant::now()) {
                return settle;
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
        self.shared.tracker().chunk_motion()
    }

    /// Stop sampling and return the motion over the whole call.
    pub fn finish(mut self) -> Motion {
        self.stop();
        self.shared.tracker().call_motion()
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
        match capture_gray(window_id) {
            Ok(frame) => {
                let changed = shared.tracker().push(frame);
                let mut state = shared.lock();
                if changed {
                    state.last_change = Instant::now();
                }
                state.frames_since_event += 1;
                state.frames += 1;
                state.last_success = Instant::now();
            }
            Err(error) => {
                shared.lock().failure.get_or_insert(error);
            }
        }
        shared.wake.notify_all();
        if let Some(rest) = FRAME_INTERVAL.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(frames_since_event: usize, event_ms: u64, change_ms: u64) -> (State, Instant) {
        let now = Instant::now();
        let state = State {
            stop: false,
            last_event: now - Duration::from_millis(event_ms),
            last_change: now - Duration::from_millis(change_ms),
            frames_since_event,
            frames: 5,
            last_success: now - Duration::from_millis(5),
            failure: None,
        };
        (state, now)
    }

    /// Skeptic R4: frames that stopped arriving after the event used to read
    /// as a quiet, settled view (a measured no-motion or move).
    #[test]
    fn a_capture_failure_after_the_baseline_is_unmeasured_not_settled() {
        let (mut failed, now) = state(3, 900, 900);
        failed.failure = Some("window 7 returned no image".into());
        assert!(matches!(settle_decision(&failed, now), Some(Settle::Unmeasured(_))));
    }

    #[test]
    fn settling_needs_a_fresh_frame_after_the_event_then_quiet() {
        let (stale, now) = state(0, 400, 900);
        assert_eq!(settle_decision(&stale, now), None, "no frame since the event yet");
        let (fresh, now) = state(2, 400, 350);
        assert_eq!(settle_decision(&fresh, now), Some(Settle::Settled));
        let (busy, now) = state(9, 400, 10);
        assert_eq!(settle_decision(&busy, now), None);
        let (capped, now) = state(9, 1300, 10);
        assert_eq!(settle_decision(&capped, now), Some(Settle::Capped));
    }

    /// Audit R4: one frame after the event, then a capture that stalls
    /// without failing, read as a quiet, settled view.
    #[test]
    fn a_stalled_capture_is_unmeasured_not_settled() {
        let (mut stalled, now) = state(1, 1300, 1250);
        stalled.last_success = now - Duration::from_millis(1250);
        assert!(matches!(settle_decision(&stalled, now), Some(Settle::Unmeasured(_))));
        let (mut early, now) = state(1, 600, 590);
        early.last_success = now - Duration::from_millis(590);
        assert_eq!(settle_decision(&early, now), None, "stale quiet is not settled");
    }
}
