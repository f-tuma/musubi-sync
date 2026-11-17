//! The tenant writer: the only writing [`TenantStore`] handle of a tenant,
//! behind a fair (FIFO) lock. Writes run one at a time in arrival order on the
//! runtime's shared blocking pool, so no thread belongs to a tenant and writer
//! threads are bounded by the runtime, not by the tenant count. An idle writer
//! is dropped with its handle; the next write opens a new one.

use std::sync::Arc;
use std::time::Instant;

use musubi_model::store::TenantStore;
use musubi_model::{Seq, StoreError, TenantId};

use crate::{CommitSink, EngineError};

pub(crate) type Writer<T> = tokio::sync::Mutex<State<T>>;

pub(crate) struct State<T> {
    /// Opened by the first write; a panic in the store drops it and the next
    /// write reopens it.
    store: Option<T>,
    /// The last head handed to the sink.
    notified: Option<Seq>,
    /// When the last write ended.
    pub(crate) used: Instant,
}

pub(crate) fn new<T>() -> Writer<T> {
    tokio::sync::Mutex::new(State {
        store: None,
        notified: None,
        used: Instant::now(),
    })
}

/// Runs `f` on the tenant's store after every write that asked before it.
/// Once it holds the lock it runs to the end, even if its caller is cancelled.
pub(crate) async fn run<T: TenantStore + 'static, R: Send + 'static>(
    writer: Arc<Writer<T>>,
    tenant: TenantId,
    open: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
    sink: CommitSink,
    f: impl FnOnce(&mut T) -> Result<R, StoreError> + Send + 'static,
) -> Result<R, EngineError> {
    let mut st = writer.lock_owned().await;
    crate::blocking(move || {
        let mut store = match st.store.take() {
            Some(s) => s,
            None => open()?,
        };
        let notified = *st
            .notified
            .get_or_insert_with(|| store.head().unwrap_or_default());
        let out = f(&mut store);
        // Every write that committed moved the head: notify once per commit,
        // before the caller learns the result.
        match store.head() {
            Ok(head) if head > notified => {
                st.notified = Some(head);
                sink(tenant, head);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%tenant, error = %e, "reading head failed"),
        }
        st.store = Some(store);
        st.used = Instant::now();
        Ok(out?)
    })
    .await
}
