//! Thread-scoped deadline for a synchronous AX traversal, including helper reads.
//! Outside a traversal the existing native API behavior is unchanged.

use std::{
    cell::Cell,
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

use super::bindings::{kAXErrorSuccess, AXError, AXUIElementRef, AXUIElementSetMessagingTimeout};

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    static FAILURE: Cell<Option<StopReason>> = const { Cell::new(None) };
    static NATIVE_FAILURES: Cell<u32> = const { Cell::new(0) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StopReason {
    Deadline,
    Cancelled,
    NativeRequestFailed { code: AXError },
    TimeoutConfigurationFailed { code: AXError },
}

pub const WALK_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const CANNOT_COMPLETE: AXError = -25204;
const TOLERATED_CONSECUTIVE_NATIVE_FAILURES: u32 = 12;

/// Remains on the blocking thread and restores any outer deadline on every exit.
pub struct WalkBudget {
    previous: Option<Instant>,
    previous_failure: Option<StopReason>,
    previous_native_failures: u32,
    _thread: PhantomData<Rc<()>>,
}

impl WalkBudget {
    pub fn new(timeout: Duration) -> Self {
        let deadline = Instant::now() + timeout;
        let previous = DEADLINE.with(|slot| {
            let previous = slot.get();
            slot.set(Some(previous.map_or(deadline, |outer| outer.min(deadline))));
            previous
        });
        Self {
            previous,
            previous_failure: FAILURE.with(Cell::get),
            previous_native_failures: NATIVE_FAILURES.with(Cell::get),
            _thread: PhantomData,
        }
    }
}

impl Drop for WalkBudget {
    fn drop(&mut self) {
        DEADLINE.with(|slot| slot.set(self.previous));
        FAILURE.with(|slot| slot.set(self.previous_failure));
        NATIVE_FAILURES.with(|slot| slot.set(self.previous_native_failures));
    }
}

pub fn exhausted() -> bool {
    stop_reason().is_some()
}

/// A traversal stops for a deadline, a native failure, or a cancelled
/// operation. Every gate in the walk — each subtree, each child batch, each
/// native request — already consults this, so cancellation lands at the same
/// points as the budget instead of needing its own checks.
pub fn stop_reason() -> Option<StopReason> {
    DEADLINE.with(Cell::get)?;
    FAILURE.with(Cell::get).or_else(|| {
        if cua_driver_core::operation::check().is_err() {
            return Some(StopReason::Cancelled);
        }
        DEADLINE.with(|slot| {
            slot.get()
                .filter(|deadline| Instant::now() >= *deadline)
                .map(|_| StopReason::Deadline)
        })
    })
}

fn stop(reason: StopReason) {
    if DEADLINE.with(Cell::get).is_some() {
        FAILURE.with(|slot| {
            if slot.get().is_none() {
                slot.set(Some(reason));
            }
        });
    }
}

fn note_native_failure(code: AXError) {
    let consecutive = NATIVE_FAILURES.with(|slot| {
        let consecutive = slot.get().saturating_add(1);
        slot.set(consecutive);
        consecutive
    });
    if consecutive > TOLERATED_CONSECUTIVE_NATIVE_FAILURES {
        stop(StopReason::NativeRequestFailed { code });
    }
}

fn clear_native_failures() {
    NATIVE_FAILURES.with(|slot| slot.set(0));
}

/// The timeout is attached to each AX proxy, including helper-created proxies.
/// Never use zero: AX interprets it as restoring the default timeout.
pub unsafe fn request(element: AXUIElementRef, call: impl FnOnce() -> AXError) -> AXError {
    let deadline = DEADLINE.with(Cell::get);
    request_with(
        deadline,
        |timeout| AXUIElementSetMessagingTimeout(element, timeout),
        call,
    )
}

fn request_with(
    deadline: Option<Instant>,
    set_timeout: impl FnOnce(f32) -> AXError,
    call: impl FnOnce() -> AXError,
) -> AXError {
    if let Some(deadline) = deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || exhausted() {
            return CANNOT_COMPLETE;
        }
        let configured = set_timeout(remaining.min(REQUEST_TIMEOUT).as_secs_f32());
        if configured != kAXErrorSuccess {
            stop(StopReason::TimeoutConfigurationFailed { code: configured });
            return configured;
        }
        if Instant::now() >= deadline {
            return CANNOT_COMPLETE;
        }
    }
    let result = call();
    if let Some(deadline) = deadline {
        if result != CANNOT_COMPLETE {
            clear_native_failures();
        } else if Instant::now() < deadline {
            note_native_failure(result);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::bindings::kAXErrorAttributeUnsupported;
    use super::*;

    #[test]
    fn a_native_failure_once_the_deadline_has_passed_is_reported_as_the_deadline() {
        let _budget = WalkBudget::new(Duration::from_millis(15));
        assert_eq!(
            request_with(
                DEADLINE.with(Cell::get),
                |_| kAXErrorSuccess,
                || {
                    std::thread::sleep(Duration::from_millis(20));
                    CANNOT_COMPLETE
                }
            ),
            CANNOT_COMPLETE
        );
        assert_eq!(stop_reason(), Some(StopReason::Deadline));
    }

    #[test]
    fn an_isolated_unresponsive_native_request_leaves_the_walk_running() {
        let _budget = WalkBudget::new(Duration::from_secs(10));
        let deadline = DEADLINE.with(Cell::get);
        assert_eq!(
            request_with(deadline, |_| kAXErrorSuccess, || CANNOT_COMPLETE),
            CANNOT_COMPLETE
        );
        assert_eq!(stop_reason(), None);
        assert!(!exhausted());
        assert_eq!(
            request_with(deadline, |_| kAXErrorSuccess, || kAXErrorSuccess),
            kAXErrorSuccess
        );
    }

    #[test]
    fn a_wedged_app_stops_the_walk_once_the_tolerated_native_failures_are_exceeded() {
        {
            let _budget = WalkBudget::new(Duration::from_secs(10));
            let deadline = DEADLINE.with(Cell::get);
            for _ in 0..TOLERATED_CONSECUTIVE_NATIVE_FAILURES {
                assert_eq!(
                    request_with(deadline, |_| kAXErrorSuccess, || CANNOT_COMPLETE),
                    CANNOT_COMPLETE
                );
                assert!(!exhausted());
            }
            assert_eq!(
                request_with(deadline, |_| kAXErrorSuccess, || CANNOT_COMPLETE),
                CANNOT_COMPLETE
            );
            assert_eq!(
                stop_reason(),
                Some(StopReason::NativeRequestFailed {
                    code: CANNOT_COMPLETE
                })
            );
        }
        assert_eq!(stop_reason(), None);
    }

    #[test]
    fn a_completed_request_between_failures_resets_the_tolerated_native_failures() {
        let _budget = WalkBudget::new(Duration::from_secs(10));
        let deadline = DEADLINE.with(Cell::get);
        for _ in 0..TOLERATED_CONSECUTIVE_NATIVE_FAILURES * 3 {
            assert_eq!(
                request_with(deadline, |_| kAXErrorSuccess, || CANNOT_COMPLETE),
                CANNOT_COMPLETE
            );
            assert_eq!(
                request_with(
                    deadline,
                    |_| kAXErrorSuccess,
                    || kAXErrorAttributeUnsupported
                ),
                kAXErrorAttributeUnsupported
            );
        }
        assert!(!exhausted());
    }

    #[tokio::test]
    async fn a_cancelled_operation_stops_the_walk_at_the_same_gate_as_the_deadline() {
        let operation = std::sync::Arc::new(cua_driver_core::operation::Cancellation::default());
        let signal = operation.clone();
        cua_driver_core::operation::scope(operation, async move {
            cua_driver_core::operation::spawn_blocking(move || {
                {
                    let _budget = WalkBudget::new(Duration::from_secs(10));
                    assert!(!exhausted(), "a live operation must not stop the walk");
                    signal.cancel();
                    assert_eq!(stop_reason(), Some(StopReason::Cancelled));
                    assert_eq!(
                        request_with(
                            DEADLINE.with(Cell::get),
                            |_| 0,
                            || panic!("a cancelled walk must not issue another native request")
                        ),
                        CANNOT_COMPLETE
                    );
                }
                // Outside a traversal the cancelled operation changes nothing:
                // ordinary native API behaviour is unaffected.
                assert!(!exhausted());
            })
            .await
            .unwrap();
        })
        .await;
    }

    #[test]
    fn exhausted_walk_stops_subsequent_native_requests_and_restores_caller() {
        let calls = Cell::new(0);
        {
            let _budget = WalkBudget::new(Duration::from_millis(15));
            let deadline = DEADLINE.with(Cell::get);
            assert_eq!(
                request_with(
                    deadline,
                    |timeout| {
                        assert!(timeout > 0.0 && timeout <= 0.015);
                        kAXErrorSuccess
                    },
                    || {
                        calls.set(calls.get() + 1);
                        std::thread::sleep(Duration::from_millis(20));
                        kAXErrorSuccess
                    }
                ),
                kAXErrorSuccess
            );
            assert!(exhausted());
            assert_eq!(
                request_with(
                    DEADLINE.with(Cell::get),
                    |_| panic!("expired timeout must not be reset"),
                    || {
                        calls.set(calls.get() + 1);
                        kAXErrorSuccess
                    }
                ),
                CANNOT_COMPLETE
            );
        }
        assert_eq!(calls.get(), 1);
        assert!(!exhausted());
        assert_eq!(
            request_with(
                DEADLINE.with(Cell::get),
                |_| panic!("unscoped request must remain unchanged"),
                || 123
            ),
            123
        );
    }

    #[test]
    fn nested_walk_cannot_extend_outer_deadline_or_leak_to_another_thread() {
        let _outer = WalkBudget::new(Duration::ZERO);
        {
            let _inner = WalkBudget::new(Duration::from_secs(30));
            assert!(exhausted());
            assert!(!std::thread::spawn(exhausted).join().unwrap());
        }
        assert!(exhausted());
    }

    #[test]
    fn failure_to_install_native_timeout_refuses_the_unbounded_request() {
        assert_eq!(
            request_with(
                Some(Instant::now() + Duration::from_secs(5)),
                |timeout| {
                    assert_eq!(timeout, 2.0);
                    -25202
                },
                || panic!("must not dispatch without the native timeout")
            ),
            -25202
        );
    }
}
