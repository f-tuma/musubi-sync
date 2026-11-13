//! One-off re-decoding of stored items after a codec's `MAPPING_VERSION` or a
//! kind's `SCHEMA_VERSION` changed (docs/plan/phase2.md ch. 10, item 1).

use std::collections::BTreeMap;

use musubi_model::{EntityKind, Storage, StoreError, TenantId, TenantStore};

use crate::{Engine, EngineError, Running, blocking, ingest};

/// Items per [`TenantStore::stale_items`] page.
const PAGE: u32 = 200;

impl<S: Storage + 'static> Engine<S> {
    /// Re-decodes the tenant's stored items whose versions differ from the
    /// registered codecs' and the kinds' schema, from their raw content and
    /// without a network call, then records the versions
    /// ([`TenantStore::set_redecoded`]) so the next start does nothing. The
    /// versions are written only after the last page; an error leaves them
    /// unset and the next start runs again. Returns the number of items
    /// rewritten.
    ///
    /// It may run alongside syncing: each write carries the raw hash it was
    /// decoded from, and the store skips an item that a sync changed since
    /// (its newer canonical form stays). A canonical form that comes out the
    /// same is not journaled.
    ///
    /// The series master of an exception comes from its stored correlation;
    /// the recurrence id is computed afresh. Known limit: an exception that
    /// never joined its master stays unjoined until the provider sends it again.
    pub async fn redecode(&self, tenant: TenantId) -> Result<usize, EngineError> {
        let accounts = self.ctl(move |c| c.accounts(tenant)).await?;
        let mut written = 0;
        for (id, (_, codecs)) in &self.inner.providers.map {
            let ids: Vec<_> = accounts
                .iter()
                .filter(|a| a.provider == *id)
                .map(|a| a.id)
                .collect();
            let cols: Vec<_> = self
                .collections(tenant)
                .await?
                .into_iter()
                .filter(|c| ids.contains(&c.account))
                .map(|c| c.id)
                .collect();
            for kind in [
                EntityKind::Event,
                EntityKind::Task,
                EntityKind::Contact,
                EntityKind::ContactGroup,
            ] {
                let (schema, mapping) = ingest::versions(codecs, kind);
                // No codec: items of the kind are stored raw only.
                if mapping == 0 {
                    continue;
                }
                let provider = id.clone();
                let done = self
                    .read(tenant, move |s| s.redecoded(&provider, kind))
                    .await?;
                if done == Some((schema, mapping)) {
                    continue;
                }
                // Stale items include stubs a sync left alone because their
                // validator was equal (it neither fetches nor re-decodes
                // them): they are brought up to date here, at the next start.
                let mut after = None;
                loop {
                    let ids = cols.clone();
                    let items = self
                        .read(tenant, move |s| {
                            s.stale_items(kind, &ids, schema, mapping, after, PAGE)
                        })
                        .await?;
                    let Some(last) = items.last() else { break };
                    after = Some(last.id);
                    let more = items.len() >= PAGE as usize;
                    let (codecs, index) = (*codecs, self.index());
                    let writes = blocking(move || {
                        let mut by_col: BTreeMap<_, Vec<_>> = BTreeMap::new();
                        for view in items {
                            let col = view.collection;
                            if let Some(w) = ingest::redecode_write(view, &codecs, index) {
                                by_col.entry(col).or_default().push(w);
                            }
                        }
                        Ok(by_col)
                    })
                    .await?;
                    for (col, items) in writes {
                        let n = items.len();
                        match self.write(tenant, move |s| s.redecode(col, &items)).await {
                            Ok(_) => written += n,
                            // Purged meanwhile.
                            Err(EngineError::Store(StoreError::NotFound)) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    if !more {
                        break;
                    }
                }
                // Written even if the raw-hash guard skipped items: a sync
                // changed them meanwhile and stored them in the current
                // versions already.
                let provider = id.clone();
                self.write(tenant, move |s| {
                    s.set_redecoded(&provider, kind, schema, mapping)
                })
                .await?;
            }
        }
        Ok(written)
    }

    /// Runs [`Engine::redecode`] for each tenant in the background, one after
    /// another; a failure (a codec panic too) is logged and retried on the
    /// next start.
    pub fn start_redecode(&self, tenants: Vec<TenantId>) -> Running {
        let engine = self.clone();
        let task = tokio::spawn(async move {
            for tenant in tenants {
                // Its own task: a codec panic stops this tenant, not the rest.
                let e = engine.clone();
                match tokio::spawn(async move { e.redecode(tenant).await }).await {
                    Ok(Ok(0)) => {}
                    Ok(Ok(items)) => tracing::info!(%tenant, items, "stored items re-decoded"),
                    Ok(Err(e)) => {
                        tracing::warn!(%tenant, error = %e, "re-decode failed; it runs again on the next start")
                    }
                    Err(e) => {
                        tracing::warn!(%tenant, error = %e, "re-decode panicked; it runs again on the next start")
                    }
                }
            }
        });
        Running { tasks: vec![task] }
    }
}
