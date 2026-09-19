//! Cancellation belongs to an admitted operation, including its blocking work.
//! A caller disappearing requests cancellation; it must not destroy the task
//! that owns native input cleanup or the runtime's admission lease.
use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// One admitted operation's cancel flag.
///
/// `check` is on every hot loop in the platform crates (per keystroke, per
/// pointer step, per native AX request), so the flag itself is a relaxed
/// atomic. The mutex/condvar pair exists only to make a blocking pacing delay
/// wake immediately, and the watch channel does the same for `.await` points;
/// both are woken under the same lock so neither can miss a cancel.
pub struct Cancellation {
    cancelled: AtomicBool,
    /// Whether the cancel came from the caller (a cancel by id, a notification,
    /// a vanished caller) rather than from runtime shutdown. Only a caller's
    /// cancel turns a finished result into the typed cancelled outcome:
    /// shutdown interrupts pacing so the runtime can drain promptly, but a call
    /// that still completes answers its caller with its own result.
    requested_by_caller: AtomicBool,
    guard: Mutex<()>,
    changed: Condvar,
    signal: tokio::sync::watch::Sender<bool>,
}
impl Default for Cancellation {
    fn default() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            requested_by_caller: AtomicBool::new(false),
            guard: Mutex::new(()),
            changed: Condvar::new(),
            signal: tokio::sync::watch::Sender::new(false),
        }
    }
}
impl Cancellation {
    /// Cancel on the caller's behalf: the call answers with the cancelled outcome.
    pub fn cancel(&self) {
        self.requested_by_caller.store(true, Ordering::Release);
        self.interrupt();
    }
    /// Stop pacing and wake waiters without claiming the result: used when the
    /// runtime shuts down under an admitted call it still drains.
    pub fn interrupt(&self) {
        {
            let _locked = self.guard.lock().unwrap_or_else(|e| e.into_inner());
            self.cancelled.store(true, Ordering::Release);
        }
        self.changed.notify_all();
        self.signal.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn is_cancelled_by_caller(&self) -> bool {
        self.requested_by_caller.load(Ordering::Acquire)
    }
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
    pub fn wait(&self, duration: Duration) -> Result<(), Cancelled> {
        let guard = self.guard.lock().unwrap_or_else(|e| e.into_inner());
        let _unused = self
            .changed
            .wait_timeout_while(guard, duration, |()| !self.is_cancelled())
            .unwrap_or_else(|e| e.into_inner());
        self.check()
    }
    /// Resolve once this operation is cancelled. The watch channel retains the
    /// cancelled state, so a subscriber created after `cancel` still resolves.
    pub async fn cancelled(&self) {
        let mut signal = self.signal.subscribe();
        if *signal.borrow_and_update() {
            return;
        }
        let _closed = signal.changed().await;
    }
}
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("operation cancelled; reconcile any input already delivered before retrying")]
pub struct Cancelled;

pub struct CancelOnDrop(pub Arc<Cancellation>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

tokio::task_local! { static ASYNC_OPERATION: Arc<Cancellation>; }
thread_local! { static BLOCKING_OPERATION: RefCell<Option<Arc<Cancellation>>> = const { RefCell::new(None) }; }
fn current() -> Option<Arc<Cancellation>> {
    ASYNC_OPERATION
        .try_with(Arc::clone)
        .ok()
        .or_else(|| BLOCKING_OPERATION.with(|op| op.borrow().clone()))
}
pub async fn scope<T>(operation: Arc<Cancellation>, future: impl Future<Output = T>) -> T {
    ASYNC_OPERATION.scope(operation, future).await
}
pub fn check() -> Result<(), Cancelled> {
    current().map_or(Ok(()), |operation| operation.check())
}
/// Whether the current operation's caller asked for cancellation, as opposed
/// to runtime shutdown interrupting it. Work whose only consumer has vanished
/// can stop publishing; work interrupted by shutdown still answers its caller.
pub fn cancelled_by_caller() -> bool {
    current().is_some_and(|operation| operation.is_cancelled_by_caller())
}
/// Interrupt a pacing delay. Call only between complete input pairs, or while
/// an enclosing guard guarantees release on every early return.
pub fn sleep(duration: Duration) -> Result<(), Cancelled> {
    match current() {
        Some(operation) => operation.wait(duration),
        None => {
            std::thread::sleep(duration);
            Ok(())
        }
    }
}
/// Async counterpart of [`sleep`], for polling loops that live in the async
/// half of a tool (`verify_state`) rather than on a blocking worker.
pub async fn sleep_async(duration: Duration) -> Result<(), Cancelled> {
    let Some(operation) = current() else {
        tokio::time::sleep(duration).await;
        return Ok(());
    };
    tokio::select! {
        () = tokio::time::sleep(duration) => operation.check(),
        () = operation.cancelled() => Err(Cancelled),
    }
}
/// The one cancelled outcome every transport reports. A cancelled call is
/// never `Ok`: input already delivered has to be reconciled by the caller.
///
/// `partial` carries whatever the interrupted tool had already established —
/// the delivered-character count, the samples taken — because "cancelled" on
/// its own does not tell the caller what to reconcile.
pub fn cancelled_result(
    call_id: Option<&str>,
    partial: Option<serde_json::Value>,
) -> crate::protocol::ToolResult {
    let mut structured = serde_json::json!({"code": CANCELLED_CODE});
    let object = structured
        .as_object_mut()
        .expect("cancelled structure is an object");
    if let Some(call_id) = call_id {
        object.insert(
            "call_id".to_owned(),
            serde_json::Value::String(call_id.to_owned()),
        );
    }
    // A tool that already reported the cancelled outcome itself carries its
    // own `partial`; keep one envelope rather than nesting two.
    if let Some(partial) = partial {
        let partial = match partial.get("code").and_then(serde_json::Value::as_str) {
            Some(CANCELLED_CODE) => partial.get("partial").cloned(),
            _ => Some(partial),
        };
        if let Some(partial) = partial {
            object.insert("partial".to_owned(), partial);
        }
    }
    crate::protocol::ToolResult::error(Cancelled.to_string()).with_structured(structured)
}
pub const CANCELLED_CODE: &str = "cancelled";
/// Carry operation cancellation into a native blocking worker. The enclosing
/// admitted operation must await this handle even when its caller disappears.
pub fn spawn_blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let operation = current();
    tokio::task::spawn_blocking(move || {
        struct Restore(Option<Arc<Cancellation>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                BLOCKING_OPERATION.with(|op| {
                    op.replace(self.0.take());
                });
            }
        }
        let _restore = Restore(BLOCKING_OPERATION.with(|op| op.replace(operation)));
        work()
    })
}

/// Run native release even when a gesture returns early or unwinds. Prepare
/// the release event before posting its matching down event.
pub struct ReleaseOnDrop<F: FnOnce()>(Option<F>);
impl<F: FnOnce()> ReleaseOnDrop<F> {
    pub fn new(release: F) -> Self {
        Self(Some(release))
    }
}
impl<F: FnOnce()> Drop for ReleaseOnDrop<F> {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn blocking_delay_wakes_on_cancellation_and_next_scope_is_independent() {
        let operation = Arc::new(Cancellation::default());
        let signal = operation.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let work = tokio::spawn(scope(operation, async move {
            spawn_blocking(move || {
                entered.send(()).unwrap();
                sleep(Duration::from_secs(60))
            })
            .await
            .unwrap()
        }));
        ready.await.unwrap();
        signal.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(1), work)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(scope(Arc::new(Cancellation::default()), async {
            spawn_blocking(check).await.unwrap()
        })
        .await
        .is_ok());
    }
    #[test]
    fn cancel_before_wait_is_not_lost() {
        let operation = Cancellation::default();
        operation.cancel();
        assert!(operation.wait(Duration::from_secs(60)).is_err());
    }
    #[tokio::test]
    async fn async_delay_wakes_on_cancellation_and_a_late_waiter_still_sees_it() {
        let operation = Arc::new(Cancellation::default());
        let signal = operation.clone();
        let waiting = tokio::spawn(scope(operation.clone(), async move {
            sleep_async(Duration::from_secs(60)).await
        }));
        tokio::task::yield_now().await;
        signal.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("cancelled delay returned promptly")
            .unwrap()
            .is_err());
        // Subscribing after the cancel must not park forever.
        tokio::time::timeout(Duration::from_secs(1), operation.cancelled())
            .await
            .expect("a waiter created after the cancel resolves");
    }
}
