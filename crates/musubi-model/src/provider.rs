//! The adapter contract for phase 1: reading only (docs/plan/phase1.md, chapter 4).
//!
//! Reading and writing are separate traits. Phase 3 adds
//! `ProviderWrite: Provider` (`apply`, `locate`) and `Codec::encode`, without
//! changing anything here and without methods that would only panic in phase 1.
//!
//! Who calls what:
//!
//! - The engine (one actor per account) calls [`Provider::discover`],
//!   [`Provider::changes`] and [`Provider::fetch`], and owns retries, back-off,
//!   token refresh and cursor storage.
//! - `musubi-syncd` calls [`PushSource::subscribe`] and [`PushSource::unsubscribe`]
//!   through the engine's channel renewal, and [`PushSource::channel_of`] and
//!   [`PushSource::verify`] in the webhook handler.
//! - The engine calls [`Codec::decode`] on CPU workers before the write transaction.
//!
//! The adapter knows neither the engine nor the storage. It holds its own HTTP
//! client (one per provider host, shared by all accounts); everything that is
//! per account comes in [`AccountCx`].

use std::fmt;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_core::Stream;
use http::{HeaderMap, Uri};
use jiff::Timestamp;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::capability::ProviderDescriptor;
use crate::error::{PushRejected, SyncError};
use crate::id::AccountId;
use crate::kind::{EntityKind, Kind};
use crate::remote::{CollectionRef, Cursor, Raw, RemoteKey, RemoteObject, RemoteRef};
use crate::time::TimeWindow;

/// A boxed, sendable stream (the shape of `futures::stream::BoxStream`).
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

/// Read side of a provider adapter. One instance serves all accounts of that provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Static facts about the provider, including all capabilities of proposal 6.2.
    /// Cheap and constant; may be called often.
    fn descriptor(&self) -> ProviderDescriptor;

    /// Lists the account's collections.
    ///
    /// `Ok` means the list is complete: a collection the store knows and this
    /// list lacks was deleted or unshared. If the listing fails part-way, return
    /// [`SyncError::PartialDiscovery`] with what was found; the engine then does
    /// not delete anything.
    async fn discover(&self, cx: &AccountCx) -> Result<Discovery, SyncError>;

    /// Streams pages of changes in one collection.
    ///
    /// - `cursor: Some` continues exactly where the page carrying that cursor
    ///   ended (mid-pass or, after the last page, the next delta). `hint` is
    ///   then ignored except for `page_size`.
    /// - `cursor: None` starts the pass that `hint.start` asks for.
    ///
    /// The stream yields pages in order and ends after the page with
    /// [`ChangePage::last`] set, or right after yielding an error; the engine
    /// drops it then. Pages may be empty and still not last (Google returns empty
    /// pages with `nextPageToken`). Every [`SyncError`] except
    /// `PartialDiscovery` is valid here.
    /// The stream does no work until polled, so dropping it cancels the read.
    fn changes<'a>(
        &'a self,
        cx: &'a AccountCx,
        col: &'a CollectionRef,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> BoxStream<'a, Result<ChangePage, SyncError>>;

    /// Fetches specific objects (re-read for the mass-deletion guard, phase 3
    /// conflict handling). Missing objects are left out of the result; the order
    /// of the result is unspecified.
    async fn fetch(
        &self,
        cx: &AccountCx,
        refs: &[RemoteRef],
    ) -> Result<Vec<RemoteObject>, SyncError>;

    /// Exchanges the account's OAuth refresh token for a new access token at
    /// the provider's token endpoint.
    ///
    /// Called by the engine, at most once per account at a time, before the
    /// access token expires or after [`SyncError::Unauthorized`]. OAuth
    /// `invalid_grant` maps to [`SyncError::ReauthRequired`].
    async fn refresh(&self, refresh_token: &SecretString) -> Result<AccessToken, SyncError>;

    /// Push notifications, if the provider supports them
    /// ([`crate::capability::PushSupport::Webhook`]).
    fn push(&self) -> Option<&dyn PushSource> {
        None
    }
}

/// Push notifications (Google watch channels, Graph subscriptions).
///
/// A notification only marks a collection dirty; it never carries data the
/// engine applies. The engine then runs a delta read.
#[async_trait]
pub trait PushSource: Send + Sync {
    /// Opens a channel that calls `callback` when `col` changes. The returned
    /// [`Subscription`] carries the expiration the provider actually granted;
    /// the engine schedules renewal from it, never from a constant.
    async fn subscribe(
        &self,
        cx: &AccountCx,
        col: &CollectionRef,
        callback: &Uri,
    ) -> Result<Subscription, SyncError>;

    /// Closes a channel. Closing an unknown or expired channel is `Ok`.
    async fn unsubscribe(&self, cx: &AccountCx, s: &Subscription) -> Result<(), SyncError>;

    /// Extracts the channel id from a notification, so the webhook handler can
    /// look up the [`Subscription`] in the control store. `None` if absent.
    /// Pure, no I/O.
    fn channel_of(&self, headers: &HeaderMap) -> Option<ChannelId>;

    /// Checks a notification against the subscription it claims to belong to
    /// (channel token, resource id, expiration). Pure, no I/O; must answer in
    /// microseconds (the webhook replies within 50 ms).
    fn verify(&self, headers: &HeaderMap, known: &Subscription) -> Result<PushHint, PushRejected>;
}

/// Decoding of raw content into the canonical form of kind `K`.
///
/// Pure: no I/O, no clock, no panics on any input (property-tested in K4).
/// The engine passes the codec as a type parameter or as the function
/// `C::decode`; the trait is not object-safe on purpose (no `self`).
pub trait Codec<K: Kind> {
    /// Version of the mapping. Stored with each decoded item; when it changes,
    /// stored items are re-decoded from the raw layer without fetching.
    const MAPPING_VERSION: u32;

    /// Decodes one raw object. Fields the canonical form lacks stay in `raw`,
    /// which the store keeps unchanged; the canonical form is only a view.
    fn decode(raw: &Raw) -> Decoded<K::Canonical>;
}

/// Result of [`Codec::decode`].
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded<T> {
    /// The canonical view.
    Canonical(T),
    /// The object is valid for the provider but has no canonical view (unknown
    /// event type, malformed JSON). The item is stored with its raw content and
    /// without a view; it is not an error of the read.
    Unsupported {
        /// Short reason for logs and the item view.
        reason: String,
    },
}

/// Result of [`Provider::refresh`].
pub struct AccessToken {
    /// The new access token.
    pub access: SecretString,
    /// Lifetime from the moment of the response (OAuth `expires_in`); the engine
    /// turns it into an instant with its clock.
    pub expires_in: std::time::Duration,
    /// A new refresh token if the provider rotated it; the old one is then invalid.
    pub rotated_refresh: Option<SecretString>,
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken")
            .field("expires_in", &self.expires_in)
            .field("rotated_refresh", &self.rotated_refresh.is_some())
            .finish_non_exhaustive()
    }
}

/// Per-account context of an adapter call.
///
/// Built by the engine for each call with the current access token. The adapter
/// never refreshes tokens on its own: a rejected token (`401`) is
/// [`SyncError::Unauthorized`] and the engine calls [`Provider::refresh`].
pub struct AccountCx {
    /// The account.
    pub account: AccountId,
    /// OAuth access token.
    pub token: SecretString,
    /// Rate limiter of this account's lane. The adapter awaits
    /// [`Limiter::acquire`] before every HTTP request.
    pub limiter: Arc<dyn Limiter>,
    /// Clock; tests substitute a fixed one.
    pub clock: Arc<dyn Clock>,
}

impl fmt::Debug for AccountCx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountCx")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// Request budget shared by the calls of one account lane (proposal 5.3).
/// Implemented by the engine (over `governor`), by tests as a no-op.
#[async_trait]
pub trait Limiter: Send + Sync {
    /// Waits until one more request may be sent.
    async fn acquire(&self);
}

/// Source of the current time.
pub trait Clock: Send + Sync {
    /// Now.
    fn now(&self) -> Timestamp;
}

/// Result of [`Provider::discover`]: the complete list of collections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// All collections visible to the account.
    pub collections: Vec<RemoteCollection>,
}

/// A collection as the provider lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCollection {
    /// Remote identity.
    pub remote: CollectionRef,
    /// Kind of items it holds.
    pub kind: EntityKind,
    /// Display name.
    pub name: String,
    /// Default IANA zone of the collection, if it has one.
    pub time_zone: Option<String>,
    /// The account's access level.
    pub access: Access,
    /// The account's primary collection of this kind.
    pub primary: bool,
}

/// Access level of the account to a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Owner.
    Owner,
    /// May write.
    Writer,
    /// May read details.
    Reader,
    /// Sees free/busy only.
    FreeBusyReader,
}

/// How to start a read when there is no cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchHint {
    /// Which pass to start when `cursor` is `None`.
    pub start: Start,
    /// Preferred page size; the adapter caps it at `limits.max_page_size`.
    pub page_size: Option<NonZeroU32>,
}

/// Which pass to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Everything in the collection, ending with a delta cursor.
    Full,
    /// Only objects overlapping the window (first impression after connecting).
    /// Produces no delta cursor; a full read follows.
    Window(TimeWindow),
}

/// Kind of pass a page belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pass {
    /// Window read: upserts only; absence means nothing.
    Window,
    /// Full read: objects absent from all pages of the pass are deleted, which
    /// the engine applies through [`crate::store::TenantStore::finish_full_read`].
    Full,
    /// Delta since the cursor: upserts and removals.
    Delta,
}

/// One page of changes, written by the engine in one transaction together with
/// its cursor ("the whole page with its cursor, or nothing").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePage {
    /// The pass this page belongs to; the same for all pages of one stream.
    pub pass: Pass,
    /// New or changed objects (or unchanged ones during a full read). At most
    /// `limits.max_page_size`, each key at most once per page.
    pub upserts: Vec<RemoteObject>,
    /// Objects the provider reports as deleted (Google `status: cancelled` of a
    /// non-exception event). Unknown keys are ignored by the store.
    pub removals: Vec<RemoteKey>,
    /// Where to resume after this page. `Some` on every page of a `Full` or
    /// `Delta` pass; on the last page it is the delta cursor for the next sync.
    /// `None` on `Window` pages: a window read is repeated, not resumed.
    pub cursor: Option<Cursor>,
    /// This page ends the pass.
    pub last: bool,
}

/// Provider id of a push channel (Google `X-Goog-Channel-ID`). Chosen by the adapter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelId(pub String);

/// An open push channel. Stored in the control store; the token is stored as
/// a secret.
#[derive(Debug, Clone)]
pub struct Subscription {
    /// Channel id; the key of the control store's channel table.
    pub channel: ChannelId,
    /// Watched collection.
    pub collection: CollectionRef,
    /// Provider's id of the watched resource (Google `resourceId`, needed to stop it).
    pub resource: String,
    /// Secret the provider echoes in every notification (Google `X-Goog-Channel-Token`).
    pub token: SecretString,
    /// Expiration granted by the provider.
    pub expiration: Timestamp,
}

/// What a verified notification means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PushHint {
    /// The channel was just opened (Google `X-Goog-Resource-State: sync`). Nothing changed.
    Handshake,
    /// The collection changed: mark it dirty; the engine coalesces notifications
    /// within 100–200 ms into one delta read.
    Changed,
}
