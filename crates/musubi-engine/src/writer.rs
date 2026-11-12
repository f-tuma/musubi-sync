//! The tenant writer: one dedicated thread per tenant owns the only writing
//! [`TenantStore`] handle and runs write commands one at a time. An idle
//! writer retires, so sleeping tenants hold no thread; the next write starts
//! a new one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use musubi_model::store::TenantStore;
use musubi_model::{StoreError, TenantId};
use tokio::sync::oneshot;

use crate::{CommitSink, EngineError};

/// Runs after the commit notification: hands the result back to the caller.
type Reply = Box<dyn FnOnce() + Send>;
type Command<T> = Box<dyn FnOnce(&mut T) -> Reply + Send>;

/// Handle to a tenant's writer thread. The thread ends when the last handle is
/// dropped; queued commands hold one, so they always run first.
pub(crate) struct Writer<T> {
    tx: mpsc::Sender<Command<T>>,
    /// Tells this writer from a later one of the same tenant.
    pub(crate) id: u64,
}

impl<T: TenantStore + 'static> Writer<T> {
    /// Starts the writer thread over an opened store. After `idle` without a
    /// command the thread calls `retire(id)`, which drops the last handle when
    /// no caller holds one; the thread then ends.
    pub(crate) fn spawn(
        tenant: TenantId,
        store: T,
        sink: CommitSink,
        idle: Duration,
        retire: impl Fn(u64) + Send + 'static,
    ) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<Command<T>>();
        std::thread::Builder::new()
            .name(format!("musubi-writer-{tenant}"))
            .spawn(move || {
                let mut store = store;
                let mut notified = store.head().unwrap_or_default();
                loop {
                    let cmd = match rx.recv_timeout(idle) {
                        Ok(cmd) => cmd,
                        Err(RecvTimeoutError::Timeout) => {
                            // Widens the gap a command can be sent in, so the
                            // writer stress test hits it.
                            #[cfg(test)]
                            std::thread::sleep(Duration::from_millis(1));
                            retire(id);
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    };
                    let reply = cmd(&mut store);
                    // Every command that committed moved the head: notify once per
                    // commit, before the caller learns the result.
                    match store.head() {
                        Ok(head) if head > notified => {
                            notified = head;
                            sink(tenant, head);
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(%tenant, error = %e, "reading head failed"),
                    }
                    reply();
                }
            })?;
        Ok(Self { tx, id })
    }

    /// Runs `f` on the writer thread and returns its result. The command holds
    /// a handle until its reply, so the writer is not retired while it is
    /// queued, even when its caller is cancelled.
    pub(crate) async fn run<R: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&mut T) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, EngineError> {
        let (tx, rx) = oneshot::channel();
        let handle = self.clone();
        let cmd: Command<T> = Box::new(move |store| {
            let out = f(store);
            Box::new(move || {
                let _ = tx.send(out);
                drop(handle);
            })
        });
        self.tx.send(cmd).map_err(|_| EngineError::Stopped)?;
        Ok(rx.await.map_err(|_| EngineError::Stopped)??)
    }
}
