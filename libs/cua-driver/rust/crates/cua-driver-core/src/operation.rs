//! Cancellation belongs to an admitted operation, including its blocking work.
//! A caller disappearing requests cancellation; it must not destroy the task
//! that owns native input cleanup or the runtime's admission lease.
use std::cell::RefCell;
use std::future::Future;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Cancellation {
    cancelled: Mutex<bool>,
    changed: Condvar,
}
impl Cancellation {
    pub fn cancel(&self) {
        *self.cancelled.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.changed.notify_all();
    }
    pub fn check(&self) -> Result<(), Cancelled> {
        if *self.cancelled.lock().unwrap_or_else(|e| e.into_inner()) {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
    pub fn wait(&self, duration: Duration) -> Result<(), Cancelled> {
        let guard = self.cancelled.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, duration, |cancelled| !*cancelled)
            .unwrap_or_else(|e| e.into_inner());
        if *guard {
            Err(Cancelled)
        } else {
            Ok(())
        }
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
}
