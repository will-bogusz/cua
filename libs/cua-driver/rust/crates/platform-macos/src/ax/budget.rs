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

/// Remains on the blocking thread and restores any outer deadline on every exit.
pub struct WalkBudget {
    previous: Option<Instant>,
    previous_failure: Option<StopReason>,
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
            _thread: PhantomData,
        }
    }
}

impl Drop for WalkBudget {
    fn drop(&mut self) {
        DEADLINE.with(|slot| slot.set(self.previous));
        FAILURE.with(|slot| slot.set(self.previous_failure));
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
    if DEADLINE.with(Cell::get).is_none() {
        return None;
    }
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
    if deadline.is_some() && result == CANNOT_COMPLETE {
        stop(StopReason::NativeRequestFailed { code: result });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresponsive_native_request_stops_the_walk_without_claiming_a_deadline_expired() {
        {
            let _budget = WalkBudget::new(Duration::from_secs(10));
            assert_eq!(
                request_with(DEADLINE.with(Cell::get), |_| 0, || CANNOT_COMPLETE),
                CANNOT_COMPLETE
            );
            assert_eq!(
                stop_reason(),
                Some(StopReason::NativeRequestFailed {
                    code: CANNOT_COMPLETE
                })
            );
            assert_eq!(
                request_with(
                    DEADLINE.with(Cell::get),
                    |_| panic!("walk stopped"),
                    || panic!("must not request another attribute")
                ),
                CANNOT_COMPLETE
            );
        }
        assert_eq!(stop_reason(), None);
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
