//! Native ScreenCaptureKit video backend (macOS).
//!
//! Replaces the ffmpeg subprocess pipeline with an in-process SCStream +
//! SCRecordingOutput. The key win is TCC: ScreenCaptureKit runs in the
//! same process as cua-driver, so it inherits the daemon's Screen
//! Recording grant. No per-binary subprocess gotcha, no second prompt,
//! no fast-fail-on-hang heuristic.
//!
//! Requires macOS 15.0+ (SCRecordingOutput introduced in macOS 15). The
//! Swift impl this is modelled on lives at
//! `libs/cua-driver/swift/Sources/CuaDriverCore/Recording/VideoRecorder.swift`,
//! though that version composes SCStream + AVAssetWriter manually so it
//! also runs on macOS 14. We use SCRecordingOutput here because the
//! Rust binding doesn't expose AVAssetWriter and macOS 15 is already
//! widespread enough that requiring it is acceptable for the Rust port.
//!
//! Lifecycle:
//!   1. `start(path)` resolves the main display, builds a 30fps full-display
//!      SCStream config + SCRecordingOutput pointing at the mp4 path,
//!      attaches the recording output, calls `start_capture()`.
//!   2. Caller stays alive while recording.
//!   3. `stop()` calls `stop_capture()` (which finalises the mp4 moov
//!      atom) and returns the elapsed-time metadata.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cua_driver_core::video::{VideoBackend, VideoBackendFactory, VideoMetadata};

use screencapturekit::prelude::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration,
};
use screencapturekit::recording_output::{
    SCRecordingOutput, SCRecordingOutputCodec, SCRecordingOutputConfiguration,
    SCRecordingOutputFileType,
};

pub struct SckitVideoBackendFactory;

impl VideoBackendFactory for SckitVideoBackendFactory {
    fn start(&self, output_path: &Path) -> anyhow::Result<Box<dyn VideoBackend>> {
        SckitVideoBackend::start(output_path).map(|b| Box::new(b) as Box<dyn VideoBackend>)
    }
}

/// Deadline for `SCStream::start_capture`.
///
/// The binding awaits Apple's completion handler on an unbounded condvar
/// (`SyncCompletion::wait`). When the capture stack wedges for a given client
/// process, that handler is never called and the wait never returns: starting
/// a recording hangs the calling thread for the life of the process, with no
/// error and no way to give up. Bound the wait and report a timeout instead.
const CAPTURE_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Run `work` on a thread we are willing to abandon, and give up after
/// `timeout`.
///
/// On timeout the thread is left running; it still owns everything it captured
/// (here the `Arc`-shared stream and recording output), so abandoning it is
/// sound — the native objects are released whenever Apple finally answers.
fn bounded<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    timeout: Duration,
) -> Result<T, Duration> {
    let (done, finished) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = done.send(work());
    });
    finished.recv_timeout(timeout).map_err(|_| timeout)
}

pub struct SckitVideoBackend {
    stream: Arc<SCStream>,
    // SCStream's add_recording_output is non-owning — Apple's API requires
    // the SCRecordingOutput stay alive for the stream's lifetime, so we
    // keep it parked here. Dropping it before stop_capture aborts the
    // encode mid-file. Shared with the bounded start so a timed-out start
    // cannot drop it out from under a native call that is still running.
    _recording: Arc<SCRecordingOutput>,
    output_path: std::path::PathBuf,
    started_at: Instant,
}

impl SckitVideoBackend {
    fn start(output_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!(
                    "failed to create recording output directory {}: {e}",
                    parent.display()
                )
            })?;
        }
        // SCRecordingOutput appends-or-fails on an existing file; match the
        // Swift impl by clearing any stale recording.mp4 from a prior run.
        match std::fs::remove_file(output_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                anyhow::bail!(
                    "failed to remove stale recording file {}: {e}",
                    output_path.display()
                );
            }
        }

        let content = SCShareableContent::get()
            .map_err(|e| anyhow::anyhow!("SCShareableContent::get failed: {e}"))?;
        let displays = content.displays();
        let display = displays
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no displays available for ScreenCaptureKit"))?;

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        // Match the Swift recorder's pixel resolution + 30fps target. The
        // display's reported width/height are in pixels (already
        // backing-scale-multiplied) on SCDisplay, so passing them through
        // gives a native-resolution capture.
        let pixel_width = display.width();
        let pixel_height = display.height();
        let frame_interval = screencapturekit::cm::CMTime::new(1, 30);
        let config = SCStreamConfiguration::new()
            .with_width(pixel_width)
            .with_height(pixel_height)
            .with_minimum_frame_interval(&frame_interval)
            .with_shows_cursor(true);

        let rec_config = SCRecordingOutputConfiguration::new()
            .with_output_url(output_path)
            .with_video_codec(SCRecordingOutputCodec::H264)
            .with_output_file_type(SCRecordingOutputFileType::MP4);

        let recording = Arc::new(SCRecordingOutput::new(&rec_config).ok_or_else(|| {
            anyhow::anyhow!(
                "SCRecordingOutput::new returned nil — macOS 15.0+ is required for \
                 native ScreenCaptureKit video; older macOS needs to use the ffmpeg \
                 backend (currently disabled on macOS)."
            )
        })?);

        let stream = Arc::new(SCStream::new(&filter, &config));
        stream
            .add_recording_output(&recording)
            .map_err(|e| anyhow::anyhow!("SCStream::add_recording_output failed: {e}"))?;
        // Hand the start to a thread we can abandon: a wedged capture stack
        // never calls Apple's completion handler, and the binding's wait for it
        // has no deadline of its own.
        let starting = (stream.clone(), recording.clone());
        bounded(
            move || {
                let (stream, _recording) = starting;
                stream.start_capture()
            },
            CAPTURE_START_TIMEOUT,
        )
        .map_err(|waited| {
            anyhow::anyhow!(
                "SCStream::start_capture did not complete within {} s — the capture stack \
                 is not answering; recording was not started",
                waited.as_secs()
            )
        })?
        .map_err(|e| anyhow::anyhow!("SCStream::start_capture failed: {e}"))?;

        tracing::info!(
            target: "recording",
            path = %output_path.display(),
            width = pixel_width,
            height = pixel_height,
            "sckit video capture started"
        );

        Ok(Self {
            stream,
            _recording: recording,
            output_path: output_path.to_path_buf(),
            started_at: Instant::now(),
        })
    }
}

impl VideoBackend for SckitVideoBackend {
    fn stop(self: Box<Self>) -> anyhow::Result<VideoMetadata> {
        let elapsed = self.started_at.elapsed();
        // SCStream::stop_capture finalises the mp4 moov atom synchronously
        // on the recording output before returning. Errors here mean the
        // file may be unplayable — surface as `finalized: false`.
        let finalized = self.stream.stop_capture().is_ok();
        if !finalized {
            tracing::warn!(
                target: "recording",
                "SCStream::stop_capture failed; recording.mp4 may be incomplete"
            );
        }
        Ok(VideoMetadata {
            path: self.output_path,
            duration_ms: elapsed.as_millis() as u64,
            finalized,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Stands in for Apple's start-capture completion handler: a caller parks
    /// on it exactly like the binding's `SyncCompletion::wait`, and this one
    /// never fires.
    fn never_fires(entered: Arc<AtomicBool>) -> impl FnOnce() -> Result<(), String> + Send {
        move || {
            entered.store(true, Ordering::SeqCst);
            loop {
                std::thread::park();
            }
        }
    }

    #[test]
    fn a_completion_that_never_fires_times_out_instead_of_parking_the_caller() {
        let entered = Arc::new(AtomicBool::new(false));
        let deadline = Duration::from_millis(150);
        let started = Instant::now();
        let outcome = bounded(never_fires(entered.clone()), deadline);
        let waited = started.elapsed();
        assert_eq!(outcome, Err(deadline));
        assert!(
            entered.load(Ordering::SeqCst),
            "the abandoned thread must actually have reached the wait"
        );
        assert!(
            waited < Duration::from_secs(2),
            "the caller must return at its own deadline, not the completion's: {waited:?}"
        );
    }

    #[test]
    fn a_completion_that_fires_delivers_its_result() {
        assert_eq!(
            bounded(
                || Err::<(), String>("native refused".into()),
                CAPTURE_START_TIMEOUT
            ),
            Ok(Err("native refused".into()))
        );
        assert_eq!(bounded(|| 7u8, CAPTURE_START_TIMEOUT), Ok(7));
    }
}
