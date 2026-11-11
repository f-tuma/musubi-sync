//! Control plane storage (`control.db`, docs/plan/phase1.md 3.1 and 3.3).
//!
//! One per instance: tenants, API keys (hash only), accounts with their sealed
//! tokens, pending OAuth sign-ins, the durable job queue and watch channels.
//! Called by the API layer (tenants, keys, accounts, sign-ins), the scheduler
//! of the engine (jobs) and the webhook handler (channels). Every operation is
//! one transaction; writes are durable when the call returns (`synchronous=FULL`).
//!
//! [`TokenVault`] is the engine's view of account tokens: plain tokens in,
//! plain tokens out. `musubi-syncd` implements it over
//! [`ControlStore::write_tokens`] and [`ControlStore::read_tokens`] with
//! encryption bound to tenant and account.
//!
//! Synchronous, `&self`: implementations serialise writers internally.

use jiff::Timestamp;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::capability::ProviderId;
use crate::error::StoreError;
use crate::id::{AccountId, CollectionId, TenantId};
use crate::provider::{ChannelId, Subscription};

/// Control plane store. Implemented by `musubi-store-sqlite`.
pub trait ControlStore: Send + Sync {
    /// Registers a tenant. `AlreadyExists` if present. The caller then creates
    /// its files with [`crate::store::Storage::create_tenant`].
    fn insert_tenant(&self, t: TenantId) -> Result<(), StoreError>;

    /// Removes a tenant with its keys, accounts, tokens, jobs and channels.
    /// `NotFound` if absent. The caller first unsubscribes its channels and
    /// afterwards deletes its files.
    fn delete_tenant(&self, t: TenantId) -> Result<(), StoreError>;

    /// Stores a new API key. Only the hash is stored; the plain key is shown to
    /// the user once and never reaches the store. `AlreadyExists` on a hash collision.
    fn insert_api_key(&self, key: &ApiKeyRecord) -> Result<(), StoreError>;

    /// Finds a live (not revoked) API key by the hash of the presented key.
    /// `None` means "reject"; the caller does not distinguish unknown from revoked.
    fn find_api_key(&self, hash: &ApiKeyHash) -> Result<Option<ApiKeyRecord>, StoreError>;

    /// Revokes a key at `at`. Idempotent; `NotFound` if the hash is unknown.
    fn revoke_api_key(&self, hash: &ApiKeyHash, at: Timestamp) -> Result<(), StoreError>;

    /// Inserts or replaces an account (after OAuth sign-in or reconnect).
    /// `(tenant, provider, subject)` is unique: the same triple under another
    /// id is `AlreadyExists` (look it up with [`ControlStore::account_by_subject`]
    /// first, so a reconnect keeps the account id).
    fn upsert_account(&self, a: &AccountRecord) -> Result<(), StoreError>;

    /// An account; `None` if absent.
    fn account(&self, id: AccountId) -> Result<Option<AccountRecord>, StoreError>;

    /// The tenant's account for this provider identity; `None` if not connected.
    fn account_by_subject(
        &self,
        tenant: TenantId,
        provider: &ProviderId,
        subject: &str,
    ) -> Result<Option<AccountRecord>, StoreError>;

    /// All accounts of a tenant.
    fn accounts(&self, tenant: TenantId) -> Result<Vec<AccountRecord>, StoreError>;

    /// Removes an account with its tokens, jobs and channels; the last step of
    /// [`AccountState::Removing`]. Before it, the engine unsubscribes the
    /// account's channels and purges its collections
    /// ([`crate::store::TenantStore::purge_collection`]). `NotFound` if absent.
    fn delete_account(&self, id: AccountId) -> Result<(), StoreError>;

    /// Sets an account's state (proposal 3.1 account state machine). `NotFound` if absent.
    fn set_account_state(&self, id: AccountId, s: AccountState) -> Result<(), StoreError>;

    /// Replaces the sealed token blob of an account (after sign-in or a refresh
    /// that rotated the refresh token). `NotFound` if the account is absent.
    fn write_tokens(&self, id: AccountId, sealed: &SealedTokens) -> Result<(), StoreError>;

    /// The sealed token blob; `None` if none was written. Opened by `musubi-syncd`.
    fn read_tokens(&self, id: AccountId) -> Result<Option<SealedTokens>, StoreError>;

    /// Stores a pending OAuth sign-in under the hash of its `state` parameter
    /// and deletes expired ones. `AlreadyExists` on a hash collision.
    fn put_oauth_state(&self, s: &OAuthPending) -> Result<(), StoreError>;

    /// Takes a pending sign-in: returns and deletes it in one transaction, so a
    /// `state` works once. `None` if unknown, already taken or expired at `now`.
    fn take_oauth_state(
        &self,
        hash: &OAuthStateHash,
        now: Timestamp,
    ) -> Result<Option<OAuthPending>, StoreError>;

    /// Enqueues a job. If a not-yet-leased job with the same [`JobSpec::dedup`]
    /// exists, nothing is added: its `due` becomes the earlier of both and its id
    /// is returned (coalescing of push notifications and polls).
    fn enqueue(&self, job: &JobSpec) -> Result<JobId, StoreError>;

    /// Leases up to `limit` jobs of `lane` with `due <= now`, highest priority
    /// first, then oldest `due`. A lease lasts until `now + lease_for`; an expired
    /// lease makes the job leasable again (crash recovery).
    fn lease_due(
        &self,
        lane: Lane,
        now: Timestamp,
        limit: u32,
        lease_for: std::time::Duration,
    ) -> Result<Vec<Job>, StoreError>;

    /// Extends a held lease to `now + lease_for` (a long full read). Returns the
    /// new lease end. `LeaseLost` as for [`ControlStore::complete`].
    fn extend_lease(
        &self,
        id: JobId,
        lease: LeaseId,
        now: Timestamp,
        lease_for: std::time::Duration,
    ) -> Result<Timestamp, StoreError>;

    /// Removes a finished job. `LeaseLost` if `lease` is no longer the job's
    /// current lease (it was taken over); the caller discards its result.
    ///
    /// A job with a follow-up (the next part of a full read) enqueues the
    /// follow-up before calling `complete`: a crash in between leaves the old
    /// job to run again (harmless, pages are idempotent), never no job.
    fn complete(&self, id: JobId, lease: LeaseId) -> Result<(), StoreError>;

    /// Releases a job to run again at `due`. `count_attempt` increments
    /// `attempts` (an error retried with back-off); a rate limit or a planned
    /// continuation passes `false`. `LeaseLost` as for [`ControlStore::complete`].
    fn reschedule(
        &self,
        id: JobId,
        lease: LeaseId,
        due: Timestamp,
        count_attempt: bool,
    ) -> Result<(), StoreError>;

    /// Inserts or replaces a watch channel (after subscribe or renewal).
    fn put_channel(&self, c: &ChannelRecord) -> Result<(), StoreError>;

    /// A channel by its id (webhook handler, see
    /// [`crate::provider::PushSource::channel_of`]); `None` if unknown. The
    /// record carries the whole [`Subscription`], so the handler verifies the
    /// notification without opening the tenant store.
    fn find_channel(&self, id: &ChannelId) -> Result<Option<ChannelRecord>, StoreError>;

    /// Channels of a tenant, or only of one of its accounts (unsubscribe before
    /// `delete_account` and `delete_tenant`).
    fn channels(
        &self,
        tenant: TenantId,
        account: Option<AccountId>,
    ) -> Result<Vec<ChannelRecord>, StoreError>;

    /// Removes a channel (after unsubscribe or expiry). Idempotent.
    fn delete_channel(&self, id: &ChannelId) -> Result<(), StoreError>;

    /// Channels expiring before `before`, for renewal.
    fn channels_expiring(&self, before: Timestamp) -> Result<Vec<ChannelRecord>, StoreError>;
}

/// Account tokens as the engine sees them, in plain form.
///
/// Implemented by `musubi-syncd` over the sealed blob of [`ControlStore`]
/// (AEAD bound to tenant and account). Synchronous; writes are durable on return.
pub trait TokenVault: Send + Sync {
    /// The account's tokens; `None` if none were stored. `Corrupt` if the blob
    /// cannot be opened (wrong key, moved to another account).
    fn load(&self, account: AccountId) -> Result<Option<AccountTokens>, StoreError>;

    /// Replaces the account's tokens.
    ///
    /// After [`crate::provider::Provider::refresh`] returned a rotated refresh
    /// token, the engine stores it here and uses the new access token only after
    /// this returned `Ok` (proposal 7): the old refresh token is already invalid,
    /// so losing the new one would force the user to sign in again.
    fn store(&self, account: AccountId, t: &AccountTokens) -> Result<(), StoreError>;
}

/// Tokens of one account. `Debug` shows no secret.
#[derive(Debug, Clone)]
pub struct AccountTokens {
    /// OAuth refresh token.
    pub refresh: SecretString,
    /// Current access token.
    pub access: SecretString,
    /// When `access` expires.
    pub access_until: Timestamp,
}

/// SHA-256 of an OAuth `state` parameter. The plain value only travels in the
/// redirect.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct OAuthStateHash(pub [u8; 32]);

impl std::fmt::Debug for OAuthStateHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OAuthStateHash(..)")
    }
}

/// A started OAuth sign-in, waiting for the provider's redirect.
#[derive(Debug, Clone)]
pub struct OAuthPending {
    /// Hash of the one-time `state`.
    pub state: OAuthStateHash,
    /// Tenant that started the sign-in; the redirect is bound to it.
    pub tenant: TenantId,
    /// Provider.
    pub provider: ProviderId,
    /// Account being reconnected, if any.
    pub account: Option<AccountId>,
    /// PKCE code verifier. Short-lived; deleted when taken or expired.
    pub verifier: SecretString,
    /// The `state` is rejected after this instant.
    pub expires: Timestamp,
}

/// SHA-256 of an API key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApiKeyHash(pub [u8; 32]);

impl std::fmt::Debug for ApiKeyHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKeyHash(..)")
    }
}

/// A stored API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRecord {
    /// Hash of the key.
    pub hash: ApiKeyHash,
    /// Tenant the key belongs to.
    pub tenant: TenantId,
    /// What the key may do.
    pub scope: Scope,
    /// Creation time.
    pub created: Timestamp,
    /// Revocation time; `None` while live.
    pub revoked: Option<Timestamp>,
}

/// Permissions of an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Read the tenant's data.
    Read,
    /// Read and manage accounts (phase 1 has no data writes).
    Admin,
}

/// A connected provider account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    /// Id (assigned by the control plane).
    pub id: AccountId,
    /// Owning tenant.
    pub tenant: TenantId,
    /// Provider.
    pub provider: ProviderId,
    /// Provider's stable account identifier (Google: the `sub` claim).
    pub subject: String,
    /// State.
    pub state: AccountState,
}

/// Account state (proposal 3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// Signed in, first sync not finished.
    Connecting,
    /// Syncing.
    Active,
    /// [`crate::error::SyncError::ReauthRequired`]: nothing runs until the user reconnects.
    ReauthRequired,
    /// Paused by the user or operator.
    Paused,
    /// Being disconnected: the engine unsubscribes its channels, purges its
    /// collections and calls [`ControlStore::delete_account`]. A restart
    /// continues the steps.
    Removing,
}

/// Encrypted token blob (AES-GCM, sealed and opened by `musubi-syncd`). Opaque here.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedTokens(pub Vec<u8>);

impl std::fmt::Debug for SealedTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SealedTokens({} bytes)", self.0.len())
    }
}

/// Id of a queued job (assigned by the store).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// Id of one lease of a job; changes whenever the job is leased again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LeaseId(pub u64);

/// Scheduler lane; each has its own workers so that slow jobs do not block fast ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// Delta syncs, push-triggered syncs, discovery.
    Sync,
    /// Full reads, index extension, prune, channel renewal.
    Background,
}

/// What a job does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobKind {
    /// List an account's collections.
    Discover {
        /// Account.
        account: AccountId,
    },
    /// Read changes of a collection (delta, or the next part of a full read).
    Sync {
        /// Account.
        account: AccountId,
        /// Collection.
        collection: CollectionId,
    },
    /// Renew a watch channel.
    RenewChannel {
        /// Channel.
        channel: ChannelId,
    },
    /// Extend occurrence indexes of a tenant.
    ExtendIndex,
    /// Prune and checkpoint a tenant.
    Maintain,
}

/// A job to enqueue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// Tenant.
    pub tenant: TenantId,
    /// Lane.
    pub lane: Lane,
    /// What to do.
    pub kind: JobKind,
    /// Higher runs first within a lane.
    pub priority: i32,
    /// Earliest time to run.
    pub due: Timestamp,
    /// Coalescing key: an unleased job with the same key absorbs this one.
    pub dedup: Option<String>,
}

/// A leased job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Id.
    pub id: JobId,
    /// Current lease, required by `extend_lease`, `complete` and `reschedule`.
    pub lease: LeaseId,
    /// Lease end; the holder stops before it.
    pub lease_until: Timestamp,
    /// Previous unsuccessful attempts.
    pub attempts: u32,
    /// The job.
    pub spec: JobSpec,
}

/// A watch channel. Keyed by `sub.channel`.
#[derive(Debug, Clone)]
pub struct ChannelRecord {
    /// Tenant.
    pub tenant: TenantId,
    /// Account.
    pub account: AccountId,
    /// Our id of the watched collection.
    pub collection: CollectionId,
    /// The subscription as the adapter returned it: channel id, resource,
    /// secret token and expiration. Passed to
    /// [`crate::provider::PushSource::verify`] and `unsubscribe`.
    pub sub: Subscription,
}
