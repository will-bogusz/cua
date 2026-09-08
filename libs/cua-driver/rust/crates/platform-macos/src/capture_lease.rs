//! Session-owned rendering support for covered native windows.
//!
//! A display filter including only the selected window keeps covered Chromium
//! renderers responsive where a desktop-independent one-shot does not. These
//! 16x16 frames are discarded without reading image bytes; screenshots still
//! come from the separate exact-window capture API. No activation is performed.
//!
//! Adapted from the OMP experiment's macos/capture_lease.rs. The retained lease
//! now belongs to Cua's authenticated lifecycle, including teardown and retry.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

use screencapturekit::{
    cm::{CMSampleBuffer, CMTime, SCFrameStatus},
    prelude::{
        SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutputType,
    },
    stream::delegate_trait::ErrorHandler,
};

use crate::tools::get_screen_size::{display_geometry, MainScreenGeometry};

const MAX_SESSION_LEASES: usize = 16;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

// The pinned 6.0.1 Swift bridge casts NSNumber to SCFrameStatus and reports
// None for every native frame. Version 9 fixes the NSNumber conversion. Keep
// the maintained SDK dependency unchanged and read this public CoreMedia
// attachment directly, with the same numeric contract and no pixel access.
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetSampleAttachmentsArray(
        buffer: *const std::ffi::c_void,
        create_if_necessary: bool,
    ) -> core_foundation::array::CFArrayRef;
}

#[link(name = "ScreenCaptureKit", kind = "framework")]
extern "C" {
    static SCStreamFrameInfoStatus: core_foundation::string::CFStringRef;
}

fn frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    // CoreMedia owns this borrowed array for the lifetime of `sample`.
    unsafe {
        frame_status_from_attachments(CMSampleBufferGetSampleAttachmentsArray(
            sample.as_ptr(),
            false,
        ))
    }
}

unsafe fn frame_status_from_attachments(
    attachments: core_foundation::array::CFArrayRef,
) -> Option<SCFrameStatus> {
    use core_foundation::{
        array::{CFArrayGetCount, CFArrayGetValueAtIndex},
        base::CFGetTypeID,
        dictionary::{CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef},
        number::{kCFNumberSInt64Type, CFNumberGetTypeID, CFNumberGetValue, CFNumberRef},
    };
    if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
        return None;
    }
    let dictionary = CFArrayGetValueAtIndex(attachments, 0);
    if dictionary.is_null() || CFGetTypeID(dictionary) != CFDictionaryGetTypeID() {
        return None;
    }
    let number = CFDictionaryGetValue(
        dictionary as CFDictionaryRef,
        SCStreamFrameInfoStatus.cast(),
    );
    if number.is_null() || CFGetTypeID(number) != CFNumberGetTypeID() {
        return None;
    }
    let mut raw = 0i64;
    if !CFNumberGetValue(
        number as CFNumberRef,
        kCFNumberSInt64Type,
        (&mut raw as *mut i64).cast(),
    ) {
        return None;
    }
    SCFrameStatus::from_raw(i32::try_from(raw).ok()?)
}

#[derive(Default)]
struct FrameObservations {
    screen_callbacks: u64,
    complete: u64,
    other_status: u64,
    unknown_status: u64,
}

impl FrameObservations {
    fn observe(&mut self, status: Option<SCFrameStatus>) {
        self.screen_callbacks += 1;
        match status {
            Some(SCFrameStatus::Complete) => self.complete += 1,
            Some(_) => self.other_status += 1,
            None => self.unknown_status += 1,
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({"screen_callbacks": self.screen_callbacks, "complete": self.complete,
            "other_status": self.other_status, "unknown_status": self.unknown_status})
    }
}

trait ManagedLease {
    fn stop(&mut self) -> Result<(), String>;
}

struct LeaseState<L> {
    closed: bool,
    leases: HashMap<String, L>,
}

/// Serializes setup, capture and teardown within this runtime. In particular,
/// cancellation of a tool future cannot detach setup from lifecycle cleanup:
/// its blocking operation still holds this lock until capture has drained.
struct LeaseRegistry<L: ManagedLease> {
    state: Mutex<LeaseState<L>>,
}

impl<L: ManagedLease> Default for LeaseRegistry<L> {
    fn default() -> Self {
        Self {
            state: Mutex::new(LeaseState {
                closed: false,
                leases: HashMap::new(),
            }),
        }
    }
}

impl<L: ManagedLease> LeaseRegistry<L> {
    fn with_lease<T>(
        &self,
        session: &str,
        ended: impl Fn() -> bool,
        matches: impl Fn(&L) -> bool,
        start: impl FnOnce() -> Result<L, String>,
        capture: impl FnOnce(&mut L) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "rendering lease lock poisoned")?;
        if state.closed || ended() {
            return Err("rendering lease session has ended".into());
        }
        if state
            .leases
            .get(session)
            .is_some_and(|lease| !matches(lease))
        {
            Self::remove(&mut state, session)?;
        }
        if !state.leases.contains_key(session) {
            if state.leases.len() >= MAX_SESSION_LEASES {
                return Err("rendering lease capacity reached; end an unused session".into());
            }
            let lease = start()?;
            state.leases.insert(session.to_owned(), lease);
        }
        // A native setup may outlive a cancelled dispatch. Do not allow a late
        // success to resurrect a session whose teardown is waiting on this lock.
        if ended() {
            Self::remove(&mut state, session)?;
            return Err("rendering lease session ended during setup".into());
        }
        capture(state.leases.get_mut(session).expect("lease inserted above"))
    }

    fn remove(state: &mut LeaseState<L>, session: &str) -> Result<(), String> {
        if let Some(lease) = state.leases.get_mut(session) {
            // Keep a failed stop owned and retryable. Never report successful
            // cleanup while a stream may still be running.
            lease.stop()?;
            state.leases.remove(session);
        }
        Ok(())
    }

    fn end(&self, session: &str) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "rendering lease lock poisoned")?;
        Self::remove(&mut state, session)
    }

    fn close(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "rendering lease lock poisoned")?;
        state.closed = true;
        let sessions = state.leases.keys().cloned().collect::<Vec<_>>();
        let errors = sessions
            .iter()
            .filter_map(|session| Self::remove(&mut state, session).err())
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl<L: ManagedLease> Drop for LeaseRegistry<L> {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            tracing::error!(%error, "rendering stream teardown failed during runtime drop");
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct LeaseIdentity {
    pid: i32,
    window_id: u32,
    bounds: [f64; 4],
    on_screen: bool,
    display: MainScreenGeometry,
}

impl LeaseIdentity {
    fn matches_current(&self, pid: i32, window_id: u32) -> bool {
        if pid != self.pid || window_id != self.window_id {
            return false;
        }
        let Some(info) = crate::windows::window_info_by_id(window_id) else {
            return false;
        };
        self.matches_observation(
            info.pid,
            info.window_id,
            [
                info.bounds.x,
                info.bounds.y,
                info.bounds.width,
                info.bounds.height,
            ],
            info.is_on_screen,
            display_geometry(self.display.identity.native_id as u32).as_ref(),
        )
    }

    fn matches_observation(
        &self,
        pid: i32,
        window_id: u32,
        bounds: [f64; 4],
        on_screen: bool,
        display: Option<&MainScreenGeometry>,
    ) -> bool {
        self.pid == pid
            && self.window_id == window_id
            && self.bounds == bounds
            && self.on_screen == on_screen
            && display == Some(&self.display)
    }
}

struct WindowLease {
    identity: LeaseIdentity,
    stream: Option<SCStream>,
    output_id: usize,
    failed: Arc<Mutex<Option<String>>>,
    ready: Option<mpsc::Receiver<Result<(), String>>>,
    start_attempted: bool,
    active: bool,
    native_stopped: Arc<AtomicBool>,
    observations: Arc<Mutex<FrameObservations>>,
}

impl WindowLease {
    fn start(pid: i32, window_id: u32) -> Result<Self, String> {
        let content = SCShareableContent::get().map_err(|e| e.to_string())?;
        let target = content
            .windows()
            .into_iter()
            .find(|window| window.window_id() == window_id)
            .ok_or("rendering lease window is not shareable")?;
        if target.owning_application().map(|app| app.process_id()) != Some(pid) {
            return Err("rendering lease window owner changed during setup".into());
        }
        let frame = target.frame();
        let display = content
            .displays()
            .into_iter()
            .filter_map(|display| {
                let bounds = display.frame();
                let area = intersection_area(
                    [
                        frame.origin.x,
                        frame.origin.y,
                        frame.size.width,
                        frame.size.height,
                    ],
                    [
                        bounds.origin.x,
                        bounds.origin.y,
                        bounds.size.width,
                        bounds.size.height,
                    ],
                );
                (area > 0.0).then_some((display, area))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(display, _)| display)
            .ok_or("rendering lease window does not intersect an active display")?;
        let geometry = display_geometry(display.display_id())
            .ok_or("rendering lease display identity or mode is unavailable")?;
        let display_frame = display.frame();
        if [
            display_frame.origin.x,
            display_frame.origin.y,
            display_frame.size.width,
            display_frame.size.height,
        ] != [
            geometry.origin.x,
            geometry.origin.y,
            geometry.width as f64,
            geometry.height as f64,
        ] {
            return Err("rendering lease display geometry changed during setup".into());
        }
        let identity = LeaseIdentity {
            pid,
            window_id,
            bounds: [
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ],
            on_screen: target.is_on_screen(),
            display: geometry,
        };
        if !identity.matches_current(pid, window_id) {
            return Err("rendering lease window or display changed during setup".into());
        }
        // Hidden/minimized windows can supply desktop-independent snapshots,
        // but SCK display streams yield no frame and window streams report
        // Suspended. Do not turn that stream state into a screenshot failure.
        // This is a selected capture mode, not recovery after a failed stream.
        // Snapshot completion does not establish application-level freshness;
        // callers still verify the rendered postcondition against app state.
        if !identity.on_screen {
            return Ok(Self {
                identity,
                stream: None,
                output_id: 0,
                failed: Arc::new(Mutex::new(None)),
                ready: None,
                start_attempted: false,
                active: false,
                native_stopped: Arc::new(AtomicBool::new(true)),
                observations: Arc::new(Mutex::new(FrameObservations::default())),
            });
        }
        let mut filter = SCContentFilter::create()
            .with_display(&display)
            .with_including_windows(&[&target])
            .build();
        filter.set_include_menu_bar(false);
        let config = stream_configuration();
        let failed = Arc::new(Mutex::new(None));
        let delegate_failed = failed.clone();
        let native_stopped = Arc::new(AtomicBool::new(false));
        let delegate_stopped = native_stopped.clone();
        let (ready, receiver) = mpsc::sync_channel(1);
        let error_ready = ready.clone();
        let delegate = ErrorHandler::new(move |error| {
            // didStopWithError confirms native capture has stopped. Preserve
            // the failure for observations while allowing callback drainage.
            delegate_stopped.store(true, Ordering::Release);
            let reason = error.to_string();
            *delegate_failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.clone());
            let _ = error_ready.try_send(Err(reason));
        });
        let mut stream = SCStream::new_with_delegate(&filter, &config, delegate);
        let observations = Arc::new(Mutex::new(FrameObservations::default()));
        let callback_observations = observations.clone();
        let output_id = stream
            .add_output_handler(
                move |sample: CMSampleBuffer, kind: SCStreamOutputType| {
                    if kind == SCStreamOutputType::Screen {
                        let status = frame_status(&sample);
                        callback_observations
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .observe(status);
                        if status == Some(SCFrameStatus::Complete) {
                            let _ = ready.try_send(Ok(()));
                        }
                    }
                    // No image-buffer access, retained sample, encoding, file or history.
                },
                SCStreamOutputType::Screen,
            )
            .ok_or("rendering lease output handler unavailable")?;
        Ok(Self {
            identity,
            stream: Some(stream),
            output_id,
            failed,
            ready: Some(receiver),
            start_attempted: false,
            active: false,
            native_stopped,
            observations,
        })
    }

    fn prepare(&mut self, pid: i32, window_id: u32) -> Result<(), String> {
        if !self.identity.on_screen {
            self.active = true;
            return self.validate(pid, window_id);
        }
        if self.active {
            return self.validate(pid, window_id);
        }
        let receiver = self
            .ready
            .take()
            .ok_or("rendering lease setup previously failed")?;
        // The pinned native binding waits for Apple's completion. It cannot be
        // cancelled by dropping a future. Keep ownership through completion;
        // an isolated worker PROCESS deadline is the hard-stop boundary if Apple
        // never calls back. Only the subsequent first-frame wait is timed here.
        self.start_attempted = true;
        let setup = self
            .stream
            .as_ref()
            .unwrap()
            .start_capture()
            .map_err(|e| e.to_string())
            .and_then(|()| {
                receiver.recv_timeout(FIRST_FRAME_TIMEOUT).map_err(|e| {
                    format!(
                        "rendering lease no complete frame: {e}; observations={}",
                        self.observations
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .json()
                    )
                })?
            })
            .and_then(|()| {
                self.active = true;
                self.validate(pid, window_id)
            });
        if let Err(ref error) = setup {
            self.active = false;
            *self.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
        }
        // The registry already owns this lease, including failed start/stop
        // attempts. Session end can retry cleanup instead of losing ownership.
        setup
    }

    fn validate(&self, pid: i32, window_id: u32) -> Result<(), String> {
        if !self.active {
            return Err("rendering lease is not active".into());
        }
        if let Some(error) = self
            .failed
            .lock()
            .map_err(|_| "rendering stream state poisoned")?
            .as_ref()
        {
            return Err(format!("rendering stream stopped: {error}"));
        }
        if !self.identity.matches_current(pid, window_id) {
            return Err("rendering lease window or display changed".into());
        }
        Ok(())
    }

    fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "state": if self.identity.on_screen { "active" } else { "snapshot_only" },
            "backend": if self.identity.on_screen { "sck_display_window_stream" } else { "sck_window_snapshot" },
            "window_on_screen": self.identity.on_screen,
            "pid": self.identity.pid, "window_id": self.identity.window_id,
            "display_identity": self.identity.display.identity,
            "display_origin": self.identity.display.origin,
            "frame_width": if self.identity.on_screen { 16 } else { 0 },
            "frame_height": if self.identity.on_screen { 16 } else { 0 },
            "frames_per_second": if self.identity.on_screen { 2 } else { 0 },
            "frames_retained": false,
            "frame_observations": self.observations.lock().unwrap_or_else(|e| e.into_inner()).json(),
        })
    }
}

fn stream_configuration() -> SCStreamConfiguration {
    SCStreamConfiguration::new()
        .with_width(16)
        .with_height(16)
        .with_minimum_frame_interval(&CMTime::new(1, 2))
        .with_queue_depth(3)
        .with_shows_cursor(false)
        .with_captures_audio(false)
        .with_captures_microphone(false)
        .with_includes_child_windows(false)
}

fn intersection_area(a: [f64; 4], b: [f64; 4]) -> f64 {
    if a.into_iter().chain(b).any(|v| !v.is_finite())
        || a[2] <= 0.0
        || a[3] <= 0.0
        || b[2] <= 0.0
        || b[3] <= 0.0
    {
        return 0.0;
    }
    ((a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0])).max(0.0)
        * ((a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1])).max(0.0)
}

impl ManagedLease for WindowLease {
    fn stop(&mut self) -> Result<(), String> {
        if let Some(stream) = self.stream.as_mut() {
            if self.start_attempted && !self.native_stopped.load(Ordering::Acquire) {
                stream
                    .stop_capture()
                    .map_err(|e| format!("rendering lease stop: {e}"))?;
                self.start_attempted = false;
            }
            // Removing the handler takes the native binding's handler write
            // lock, draining any callback that still holds its read lock.
            if !stream.remove_output_handler(self.output_id, SCStreamOutputType::Screen) {
                return Err("rendering lease output callback could not be drained".into());
            }
            self.stream.take();
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for WindowLease {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(%error, "rendering lease final native release after failed stop");
        }
    }
}

#[derive(Default)]
pub(crate) struct WindowRenderingLeases(LeaseRegistry<WindowLease>);

impl WindowRenderingLeases {
    pub(crate) fn capture<T>(
        &self,
        session: Option<&str>,
        pid: i32,
        window_id: u32,
        capture: impl FnOnce() -> Result<T, String>,
    ) -> Result<(T, serde_json::Value), String> {
        let operation = |lease: &mut WindowLease| {
            lease.prepare(pid, window_id)?;
            if session.is_some_and(cua_driver_core::session::is_session_ended) {
                return Err("rendering lease session ended before screenshot".into());
            }
            let result = capture()?;
            lease.validate(pid, window_id)?;
            Ok((result, lease.metadata()))
        };
        if let Some(session) = session.filter(|s| !s.is_empty() && *s != "default") {
            self.0.with_lease(
                session,
                || cua_driver_core::session::is_session_ended(session),
                |lease| lease.validate(pid, window_id).is_ok(),
                || WindowLease::start(pid, window_id),
                operation,
            )
        } else {
            // Direct in-process tools without a lifecycle must not leave an
            // unowned stream behind. Public registry calls get an implicit id.
            let mut lease = WindowLease::start(pid, window_id)?;
            let result = operation(&mut lease);
            lease.stop()?;
            result.map(|(result, mut metadata)| {
                metadata["state"] = "completed".into();
                metadata["lifetime"] = "call".into();
                (result, metadata)
            })
        }
    }

    pub(crate) fn end(&self, session: &str) -> Result<(), String> {
        self.0.end(session)
    }
    pub(crate) fn close(&self) -> Result<(), String> {
        self.0.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct FakeLease {
        target: u32,
        events: Arc<Mutex<Vec<String>>>,
        fail_stop: Arc<AtomicBool>,
    }

    impl ManagedLease for FakeLease {
        fn stop(&mut self) -> Result<(), String> {
            self.events
                .lock()
                .unwrap()
                .push(format!("stop:{}", self.target));
            if self.fail_stop.load(Ordering::SeqCst) {
                Err("native stop failed".into())
            } else {
                Ok(())
            }
        }
    }

    fn fake(
        target: u32,
        events: &Arc<Mutex<Vec<String>>>,
        fail_stop: &Arc<AtomicBool>,
    ) -> FakeLease {
        events.lock().unwrap().push(format!("start:{target}"));
        FakeLease {
            target,
            events: events.clone(),
            fail_stop: fail_stop.clone(),
        }
    }

    #[test]
    fn reuses_only_same_target_and_stops_before_switching() {
        let registry = LeaseRegistry::default();
        let events = Arc::new(Mutex::new(vec![]));
        let failed = Arc::new(AtomicBool::new(false));
        for target in [1, 1, 2] {
            registry
                .with_lease(
                    "session",
                    || false,
                    |l: &FakeLease| l.target == target,
                    || Ok(fake(target, &events, &failed)),
                    |l| {
                        l.events
                            .lock()
                            .unwrap()
                            .push(format!("capture:{}", l.target));
                        Ok(())
                    },
                )
                .unwrap();
        }
        registry.end("session").unwrap();
        registry.end("session").unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            [
                "start:1",
                "capture:1",
                "capture:1",
                "stop:1",
                "start:2",
                "capture:2",
                "stop:2"
            ]
        );
    }

    #[test]
    fn failed_stop_retains_owner_and_blocks_replacement_until_retry() {
        let registry = LeaseRegistry::default();
        let events = Arc::new(Mutex::new(vec![]));
        let failed = Arc::new(AtomicBool::new(true));
        registry
            .with_lease(
                "session",
                || false,
                |_| true,
                || Ok(fake(1, &events, &failed)),
                |_| Ok(()),
            )
            .unwrap();
        assert!(registry.end("session").is_err());
        assert!(registry
            .with_lease(
                "session",
                || false,
                |_| false,
                || Ok(fake(2, &events, &failed)),
                |_| Ok(())
            )
            .is_err());
        assert_eq!(registry.state.lock().unwrap().leases.len(), 1);
        assert!(!events.lock().unwrap().contains(&"start:2".into()));
        failed.store(false, Ordering::SeqCst);
        registry.end("session").unwrap();
        assert!(registry.state.lock().unwrap().leases.is_empty());
    }

    #[test]
    fn setup_failure_and_capture_failure_keep_cleanup_ownership() {
        let registry = LeaseRegistry::default();
        let events = Arc::new(Mutex::new(vec![]));
        let failed = Arc::new(AtomicBool::new(false));
        let error = registry
            .with_lease(
                "session",
                || false,
                |_| true,
                || Ok(fake(1, &events, &failed)),
                |_| Err::<(), _>("first frame failed".into()),
            )
            .unwrap_err();
        assert_eq!(error, "first frame failed");
        assert_eq!(registry.state.lock().unwrap().leases.len(), 1);
        registry.end("session").unwrap();
        assert_eq!(*events.lock().unwrap(), ["start:1", "stop:1"]);
    }

    #[test]
    fn late_setup_cannot_resurrect_ended_session() {
        let registry = LeaseRegistry::default();
        let events = Arc::new(Mutex::new(vec![]));
        let failed = Arc::new(AtomicBool::new(false));
        let ended = AtomicBool::new(false);
        let result: Result<(), String> = registry.with_lease(
            "session",
            || ended.load(Ordering::SeqCst),
            |_| true,
            || {
                ended.store(true, Ordering::SeqCst);
                Ok(fake(1, &events, &failed))
            },
            |_| panic!("ended session must not capture"),
        );
        assert!(result.is_err());
        assert_eq!(*events.lock().unwrap(), ["start:1", "stop:1"]);
        assert!(registry.state.lock().unwrap().leases.is_empty());
    }

    #[test]
    fn cleanup_drains_an_operation_even_if_its_caller_stops_waiting() {
        let registry = Arc::new(LeaseRegistry::default());
        let events = Arc::new(Mutex::new(vec![]));
        let failed = Arc::new(AtomicBool::new(false));
        let (entered, entered_rx) = mpsc::channel();
        let (finish, finish_rx) = mpsc::channel();
        let worker_registry = registry.clone();
        let worker_events = events.clone();
        let worker = std::thread::spawn(move || {
            worker_registry.with_lease(
                "session",
                || false,
                |_| true,
                || Ok(fake(1, &worker_events, &failed)),
                |lease| {
                    entered.send(()).unwrap();
                    finish_rx.recv().unwrap();
                    lease.events.lock().unwrap().push("capture_done".into());
                    Ok(())
                },
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let cleanup_registry = registry.clone();
        let (cleanup_started, cleanup_started_rx) = mpsc::channel();
        let (cleanup_done, cleanup_done_rx) = mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            cleanup_started.send(()).unwrap();
            cleanup_registry.end("session").unwrap();
            cleanup_done.send(()).unwrap();
        });
        cleanup_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(cleanup_done_rx.try_recv().is_err());
        assert_eq!(*events.lock().unwrap(), ["start:1"]);
        finish.send(()).unwrap();
        worker.join().unwrap().unwrap();
        cleanup.join().unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            ["start:1", "capture_done", "stop:1"]
        );
    }

    #[test]
    fn runtime_close_and_capacity_are_fail_closed_without_eviction() {
        let registry = LeaseRegistry::default();
        let events = Arc::new(Mutex::new(vec![]));
        let failed = Arc::new(AtomicBool::new(false));
        for n in 0..MAX_SESSION_LEASES {
            registry
                .with_lease(
                    &format!("session-{n}"),
                    || false,
                    |_| true,
                    || Ok(fake(n as u32, &events, &failed)),
                    |_| Ok(()),
                )
                .unwrap();
        }
        let starts = AtomicUsize::new(0);
        let result = registry.with_lease(
            "overflow",
            || false,
            |_| true,
            || {
                starts.fetch_add(1, Ordering::SeqCst);
                Ok(fake(99, &events, &failed))
            },
            |_| Ok(()),
        );
        assert!(result.is_err());
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        registry.close().unwrap();
        assert!(registry.state.lock().unwrap().leases.is_empty());
        assert!(registry
            .with_lease(
                "new",
                || false,
                |_| true,
                || panic!("closed runtime must not start"),
                |_| Ok(())
            )
            .is_err());
    }

    #[test]
    fn display_selection_requires_positive_real_overlap() {
        let window = [100.0, 50.0, 700.0, 500.0];
        assert_eq!(
            intersection_area(window, [0.0, 0.0, 500.0, 1000.0]),
            200_000.0
        );
        assert_eq!(intersection_area(window, [800.0, 50.0, 900.0, 500.0]), 0.0);
        assert_eq!(intersection_area(window, [0.0, 0.0, f64::NAN, 50.0]), 0.0);
        assert_eq!(intersection_area(window, [0.0, 0.0, -1.0, 50.0]), 0.0);
    }

    #[test]
    fn identity_rejects_owner_frame_display_replacement_and_disconnect() {
        let identity = LeaseIdentity {
            pid: 100,
            window_id: 20,
            bounds: [10.0, 20.0, 700.0, 500.0],
            on_screen: true,
            display: MainScreenGeometry {
                identity: cua_driver_contract::DisplayIdentityOutput {
                    uuid: "display-a".into(),
                    native_id: 1,
                },
                origin: cua_driver_contract::ScreenOriginOutput { x: 0.0, y: 0.0 },
                width: 1728,
                height: 1117,
                scale_factor: 2.0,
                pixel_width: 3456,
                pixel_height: 2234,
            },
        };
        assert!(identity.matches_observation(
            100,
            20,
            identity.bounds,
            true,
            Some(&identity.display)
        ));
        assert!(!identity.matches_observation(
            101,
            20,
            identity.bounds,
            true,
            Some(&identity.display)
        ));
        assert!(!identity.matches_observation(
            100,
            21,
            identity.bounds,
            true,
            Some(&identity.display)
        ));
        for index in 0..4 {
            let mut bounds = identity.bounds;
            bounds[index] += 1.0;
            assert!(!identity.matches_observation(100, 20, bounds, true, Some(&identity.display)));
        }
        for index in 0..5 {
            let mut display = identity.display.clone();
            match index {
                0 => display.identity.uuid = "replacement".into(),
                1 => display.identity.native_id += 1,
                2 => display.origin.x -= 100.0,
                3 => display.scale_factor = 1.0,
                _ => display.pixel_width += 1,
            }
            assert!(!identity.matches_observation(100, 20, identity.bounds, true, Some(&display)));
        }
        assert!(!identity.matches_observation(100, 20, identity.bounds, true, None));
        assert!(!identity.matches_observation(
            100,
            20,
            identity.bounds,
            false,
            Some(&identity.display)
        ));
        let hidden = LeaseIdentity {
            on_screen: false,
            ..identity.clone()
        };
        assert!(hidden.matches_observation(100, 20, hidden.bounds, false, Some(&hidden.display)));
        assert!(!hidden.matches_observation(100, 20, hidden.bounds, true, Some(&hidden.display)));
    }

    #[test]
    fn native_numeric_frame_attachment_decodes_complete_and_rejects_invalid_values() {
        use core_foundation::{
            array::CFArray,
            base::{CFType, TCFType},
            dictionary::CFDictionary,
            number::CFNumber,
            string::CFString,
        };
        let key = unsafe { CFString::wrap_under_get_rule(SCStreamFrameInfoStatus) };
        for raw in [0i64, 1, 2, 3, 4, 5, -1, 99, i64::MAX] {
            // CFNumber is the actual toll-free NSNumber representation placed
            // in SCStreamFrameInfo attachments, not a Swift enum object.
            let dictionary = CFDictionary::from_CFType_pairs(&[(
                key.as_CFType(),
                CFNumber::from(raw).as_CFType(),
            )]);
            let array = CFArray::from_CFTypes(&[dictionary]);
            let expected = i32::try_from(raw).ok().and_then(SCFrameStatus::from_raw);
            assert_eq!(
                unsafe { frame_status_from_attachments(array.as_concrete_TypeRef()) },
                expected
            );
        }
        let dictionary =
            CFDictionary::from_CFType_pairs(&[(key.as_CFType(), CFString::new("0").as_CFType())]);
        let array = CFArray::from_CFTypes(&[dictionary]);
        assert_eq!(
            unsafe { frame_status_from_attachments(array.as_concrete_TypeRef()) },
            None
        );
        let empty = CFArray::<CFType>::from_CFTypes(&[]);
        assert_eq!(
            unsafe { frame_status_from_attachments(empty.as_concrete_TypeRef()) },
            None
        );
        assert_eq!(
            unsafe { frame_status_from_attachments(std::ptr::null()) },
            None
        );
    }

    #[test]
    fn production_configuration_is_minimal_and_silent() {
        let config = stream_configuration();
        assert_eq!(config.width(), 16);
        assert_eq!(config.height(), 16);
        assert_eq!(config.queue_depth(), 3);
        assert!(!config.shows_cursor());
        assert!(!config.captures_audio());
        assert!(!config.captures_microphone());
        assert!(!config.includes_child_windows());
        let interval = config.minimum_frame_interval();
        assert_eq!(interval.value, 1);
        assert_eq!(interval.timescale, 2);
    }
}
