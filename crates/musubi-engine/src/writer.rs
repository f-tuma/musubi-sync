//! The tenant writer: one dedicated thread per tenant owns the only writing
//! [`TenantStore`] handle and runs write commands one at a time.

use std::sync::mpsc;

use musubi_model::store::TenantStore;
use musubi_model::{StoreError, TenantId};
use tokio::sync::oneshot;

use crate::{CommitSink, EngineError};

/// Runs after the commit notification: hands the result back to the caller.
type Reply = Box<dyn FnOnce() + Send>;
type Command<T> = Box<dyn FnOnce(&mut T) -> Reply + Send>;

/// Handle to a tenant's writer thread. The thread ends when the last handle is
/// dropped.
pub(crate) struct Writer<T> {
    tx: mpsc::Sender<Command<T>>,
}

impl<T: TenantStore + 'static> Writer<T> {
    /// Starts the writer thread over an opened store.
    pub(crate) fn spawn(tenant: TenantId, store: T, sink: CommitSink) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Command<T>>();
        std::thread::Builder::new()
            .name(format!("musubi-writer-{tenant}"))
            .spawn(move || {
                let mut store = store;
                let mut notified = store.head().unwrap_or_default();
                for cmd in rx {
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
        Ok(Self { tx })
    }

    /// Runs `f` on the writer thread and returns its result.
    pub(crate) async fn run<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut T) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, EngineError> {
        let (tx, rx) = oneshot::channel();
        let cmd: Command<T> = Box::new(move |store| {
            let out = f(store);
            Box::new(move || {
                let _ = tx.send(out);
            })
        });
        self.tx.send(cmd).map_err(|_| EngineError::Stopped)?;
        Ok(rx.await.map_err(|_| EngineError::Stopped)??)
    }
}
