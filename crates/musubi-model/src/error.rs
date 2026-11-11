//! Errors, typed by what the caller has to do next.

use std::time::Duration;

use crate::id::Seq;
use crate::provider::RemoteCollection;

/// Any underlying error, kept as the source for logs.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Error of a [`crate::provider::Provider`] or [`crate::provider::PushSource`] call.
///
/// The adapter maps provider responses to these variants; the engine decides
/// from the variant alone, never from the source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The grant is gone (OAuth `invalid_grant`, revoked access, 401 after a
    /// fresh token). The engine moves the account to "requires sign-in" and stops
    /// polling it until the user reconnects. Token refresh is the engine's job;
    /// the adapter never retries this itself.
    #[error("account requires re-authorisation")]
    ReauthRequired,
    /// The provider rejected the access token (`401`): expired early, clock skew.
    /// The engine calls [`crate::provider::Provider::refresh`] and retries once;
    /// a second `Unauthorized` right after a refresh is handled as `ReauthRequired`.
    #[error("access token rejected")]
    Unauthorized,
    /// The provider throttled us (Google `429`, `403 rateLimitExceeded`). The
    /// engine feeds `retry_after` (from `Retry-After`, if sent) into the account's
    /// limiter and retries the same call later with the same cursor.
    #[error("rate limited by provider (retry after {retry_after:?})")]
    RateLimited {
        /// Provider's requested pause, if it sent one.
        retry_after: Option<Duration>,
    },
    /// The cursor is no longer valid (Google `410 Gone` on a sync token). The
    /// engine drops the stored cursor and starts a full read ([`crate::provider::Start::Full`]);
    /// absence-based deletion then applies only after that read finishes.
    #[error("cursor expired; a full read is required")]
    CursorExpired,
    /// Network failure, timeout, 5xx. Retried with back-off, same cursor.
    #[error("transient provider error")]
    Transient(#[source] BoxError),
    /// The request cannot succeed by retrying (4xx other than the above, a
    /// malformed response). The engine records it on the collection
    /// ([`crate::store::CollectionStatus`]) and does not retry until something changes.
    #[error("permanent provider error")]
    Permanent(#[source] BoxError),
    /// Discovery listed some collections but could not finish. The engine uses
    /// `found` to add and update collections but must not treat a collection
    /// missing from it as deleted.
    #[error("discovery incomplete ({} collections found)", found.len())]
    PartialDiscovery {
        /// Collections that were listed before the failure.
        found: Vec<RemoteCollection>,
        /// What stopped the listing.
        #[source]
        source: BoxError,
    },
}

/// Why a push notification was rejected by [`crate::provider::PushSource::verify`].
///
/// The webhook handler answers with an error status, enqueues nothing and logs
/// the variant. None of these is retried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PushRejected {
    /// The channel or resource id does not match the known subscription.
    #[error("notification for an unknown channel")]
    UnknownChannel,
    /// The channel token does not match (forged or stale notification).
    #[error("channel token mismatch")]
    BadToken,
    /// The subscription has expired.
    #[error("subscription expired")]
    Expired,
    /// Required headers are missing or malformed.
    #[error("malformed notification: {0}")]
    Malformed(String),
}

/// Error of a [`crate::store::Storage`], [`crate::store::TenantStore`] or
/// [`crate::control::ControlStore`] call.
///
/// A failed write operation has changed nothing (each is one transaction).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The addressed tenant, collection, account, job or channel does not exist.
    /// Callers treat it as a bug or a race with deletion, not as retryable.
    #[error("not found")]
    NotFound,
    /// The object to create exists already (`create_tenant` twice).
    #[error("already exists")]
    AlreadyExists,
    /// `changes(since)` asked for a position older than the pruned journal.
    /// The client must start a new initial sync; `oldest` is the first position
    /// still available.
    #[error("cursor too old; oldest available position is {oldest:?}")]
    CursorTooOld {
        /// Oldest `since` that `changes` still accepts.
        oldest: Seq,
    },
    /// The tenant file was written by a newer schema than this binary knows.
    /// The tenant is not opened and nothing is written; upgrade the binary.
    #[error("schema version {found} is newer than supported {supported}")]
    SchemaTooNew {
        /// Version in the file.
        found: u32,
        /// Newest version this build can open.
        supported: u32,
    },
    /// A full-read operation does not match the collection's read state
    /// ([`crate::store::TenantStore::apply_page`],
    /// [`crate::store::TenantStore::finish_full_read`]). A writer bug or a stale
    /// read after a restart; nothing was written.
    #[error("full read id does not match the collection state")]
    StaleRead,
    /// A job lease expired and was taken over; the result of the old holder is
    /// discarded. The holder stops working on the job.
    #[error("job lease lost")]
    LeaseLost,
    /// The database stayed locked longer than the busy timeout. Retryable.
    #[error("store busy")]
    Busy,
    /// The data violates an invariant (failed integrity check, impossible row).
    /// Not retryable; the tenant needs an operator.
    #[error("store corrupt: {0}")]
    Corrupt(String),
    /// File-system error (disk full, permissions).
    #[error("store I/O error")]
    Io(#[from] std::io::Error),
    /// Any other backend error (SQLite, later PostgreSQL).
    #[error("store backend error")]
    Backend(#[source] BoxError),
}
