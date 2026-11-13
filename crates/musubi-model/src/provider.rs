//! The adapter contract v0: reading only (docs/plan/phase1.md, chapter 4;
//! docs/plan/phase2.md, chapter 3).
//!
//! Reading and writing are separate traits. Phase 3 adds
//! `ProviderWrite: Provider` (`apply`, `locate`), reached through a default
//! method `Provider::writer` like [`Provider::push`] and [`Provider::oauth`],
//! and `CodecWrite<K>: Codec<K>`, without changing anything here and without
//! methods that would only panic before then.
//!
//! Public structs and enums here are `#[non_exhaustive]` so that adding a field
//! or a variant stays an additive (v0.x) change: build structs with their
//! `new` and set optional fields by assignment, and give matches a `_` arm.
//!
//! Who calls what:
//!
//! - The engine (one actor per account) calls [`Provider::discover`],
//!   [`Provider::changes`] and [`Provider::fetch`], and owns retries, back-off,
//!   token refresh ([`TokenRefresh`]) and cursor storage.
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
use crate::capability::{DeltaSupport, PushSupport};
use crate::error::{PushRejected, SyncError};
use crate::id::AccountId;
use crate::kind::{EntityKind, Kind};
use crate::remote::{CollectionRef, Cursor, Raw, RemoteKey, RemoteObject, RemoteRef, Validator};
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

    /// Fetches specific objects: the bodies of changed [`ChangePage::stubs`],
    /// the re-read of the mass-deletion guard, phase 3 conflict handling.
    ///
    /// The engine passes at most `limits.max_batch` refs per call (100 when
    /// `None`). Missing objects (deleted since they were listed) are left out
    /// of the result, which is not an error; the order of the result is
    /// unspecified. Every [`SyncError`] except `PartialDiscovery` is valid.
    async fn fetch(
        &self,
        cx: &AccountCx,
        refs: &[RemoteRef],
    ) -> Result<Vec<RemoteObject>, SyncError>;

    /// OAuth token refresh, if the provider's accounts use OAuth
    /// ([`Credential::Bearer`]). `None` (the default): the engine never
    /// refreshes and a rejected credential is [`SyncError::ReauthRequired`]
    /// (DAV with Basic credentials).
    fn oauth(&self) -> Option<&dyn TokenRefresh> {
        None
    }

    /// Push notifications, if the provider supports them
    /// ([`crate::capability::PushSupport::Webhook`]).
    fn push(&self) -> Option<&dyn PushSource> {
        None
    }
}

/// OAuth token refresh of a provider, reached through [`Provider::oauth`].
#[async_trait]
pub trait TokenRefresh: Send + Sync {
    /// Exchanges the account's OAuth refresh token for a new access token at
    /// the provider's token endpoint.
    ///
    /// Called by the engine, at most once per account at a time, before the
    /// access token expires or after [`SyncError::Unauthorized`]. OAuth
    /// `invalid_grant` maps to [`SyncError::ReauthRequired`] with
    /// [`crate::error::ReauthReason::Rejected`].
    async fn refresh(&self, refresh_token: &SecretString) -> Result<AccessToken, SyncError>;
}

/// Push notifications (Google watch channels, Graph subscriptions).
///
/// A notification only marks a collection dirty; it never carries data the
/// engine applies. The engine then runs a delta read.
#[async_trait]
pub trait PushSource: Send + Sync {
    /// Opens a channel that calls `callback` when `col` changes. `channel` is
    /// the caller-chosen id the provider must be able to stop the subscription
    /// by later, even if this call's reply is lost (Google: the watch id; a
    /// Graph adapter would carry it in `clientState` and find the subscription
    /// by it). The engine stores it before the call, so a crash between the
    /// call and storing the reply leaves a pending channel it can stop.
    ///
    /// The returned [`Subscription`] has `channel` as its id and carries the
    /// expiration the provider actually granted; the engine schedules renewal
    /// from it, never from a constant.
    ///
    /// Errors tell the engine whether a channel may exist at the provider:
    ///
    /// - [`SyncError::RateLimited`], [`SyncError::ReauthRequired`],
    ///   [`SyncError::Unauthorized`] and [`SyncError::Permanent`] mean the
    ///   provider created nothing. The engine deletes its pending channel row
    ///   without calling [`PushSource::unsubscribe`]. An adapter that cannot
    ///   guarantee this for a response must return `Transient` instead.
    /// - [`SyncError::Transient`] means unknown (timeout, lost reply, 5xx): a
    ///   channel may exist. The engine keeps the pending row and stops the
    ///   channel by `channel` later.
    async fn subscribe(
        &self,
        cx: &AccountCx,
        col: &CollectionRef,
        callback: &Uri,
        channel: &ChannelId,
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
#[non_exhaustive]
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

/// Result of [`TokenRefresh::refresh`]. Non-exhaustive: build it with
/// [`AccessToken::new`].
#[non_exhaustive]
pub struct AccessToken {
    /// The new access token.
    pub access: SecretString,
    /// Lifetime from the moment of the response (OAuth `expires_in`); the engine
    /// turns it into an instant with its clock.
    pub expires_in: std::time::Duration,
    /// A new refresh token if the provider rotated it; the old one is then invalid.
    pub rotated_refresh: Option<SecretString>,
}

impl AccessToken {
    /// A token without a rotated refresh token.
    pub fn new(access: SecretString, expires_in: std::time::Duration) -> Self {
        AccessToken {
            access,
            expires_in,
            rotated_refresh: None,
        }
    }
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
/// Built by the engine for each call with the current credential. The adapter
/// never refreshes tokens on its own: a rejected bearer token (`401`) is
/// [`SyncError::Unauthorized`] and the engine calls [`TokenRefresh::refresh`];
/// rejected Basic credentials are [`SyncError::ReauthRequired`].
/// Non-exhaustive: build it with [`AccountCx::new`].
#[non_exhaustive]
pub struct AccountCx {
    /// The account.
    pub account: AccountId,
    /// What the adapter authenticates with.
    pub credential: Credential,
    /// Non-secret account state the adapter stored through
    /// [`Discovery::endpoint`] (DAV server, principal, home sets). `None` for
    /// providers without one (Google) and before the first discovery.
    pub endpoint: Option<Endpoint>,
    /// Rate limiter of this account's lane. The adapter awaits
    /// [`Limiter::acquire`] before every HTTP request.
    pub limiter: Arc<dyn Limiter>,
    /// Clock; tests substitute a fixed one.
    pub clock: Arc<dyn Clock>,
}

impl AccountCx {
    /// A context without an endpoint.
    pub fn new(
        account: AccountId,
        credential: Credential,
        limiter: Arc<dyn Limiter>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        AccountCx {
            account,
            credential,
            endpoint: None,
            limiter,
            clock,
        }
    }
}

impl fmt::Debug for AccountCx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountCx")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// The secret an adapter authenticates one call with. Never logged: `Debug`
/// shows no secret.
#[derive(Clone)]
#[non_exhaustive]
pub enum Credential {
    /// OAuth access token, sent as `Authorization: Bearer` (Google).
    Bearer(SecretString),
    /// HTTP Basic (DAV; an app-specific password for iCloud).
    Basic {
        /// User name.
        username: String,
        /// Password.
        password: SecretString,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credential::Bearer(_) => f.write_str("Bearer(..)"),
            Credential::Basic { .. } => f.write_str("Basic(..)"),
        }
    }
}

/// Non-secret state of an account that the adapter needs between calls: the
/// server address and whatever discovery found. Stored by the engine
/// ([`crate::control::ControlStore::set_endpoint`]) outside the sealed secrets
/// and handed back in [`AccountCx::endpoint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Base URL the account was connected with.
    pub base: String,
    /// Adapter-defined state (DAV: profile, principal, home sets, partition
    /// host). Opaque to the engine and the store.
    pub state: serde_json::Value,
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
/// Non-exhaustive: build it with [`Discovery::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Discovery {
    /// All collections visible to the account.
    pub collections: Vec<RemoteCollection>,
    /// Updated account state to store ([`crate::control::ControlStore::set_endpoint`]);
    /// `None` keeps the stored one.
    pub endpoint: Option<Endpoint>,
    /// Kinds the account has no permission for (Google without the Tasks or
    /// People scope). Collections of these kinds are not deleted even though
    /// they are absent from `collections`; the engine stores the list
    /// ([`crate::control::ControlStore::set_missing`]). A kind not listed here
    /// is authorised, which clears an earlier [`SyncError::MissingScope`].
    pub unauthorized: Vec<EntityKind>,
}

impl Discovery {
    /// A complete listing without endpoint update or unauthorised kinds.
    pub fn new(collections: Vec<RemoteCollection>) -> Self {
        Discovery {
            collections,
            endpoint: None,
            unauthorized: Vec::new(),
        }
    }
}

/// A collection as the provider lists it. Stored as JSON (`collections.meta`),
/// so new fields have a serde default and old rows still read.
/// Non-exhaustive: build it with [`RemoteCollection::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RemoteCollection {
    /// Remote identity.
    pub remote: CollectionRef,
    /// Main kind of the collection, for listing it: a calendar is `Event`, a
    /// calendar with tasks only and a Google task list are `Task`, an address
    /// book is `Contact`. Objects carry their own [`RemoteObject::kind`].
    pub kind: EntityKind,
    /// Display name.
    pub name: String,
    /// Default IANA zone of the collection, if it has one.
    pub time_zone: Option<String>,
    /// The account's access level.
    pub access: Access,
    /// The account's primary collection of this kind.
    pub primary: bool,
    /// Delta mechanism of this collection; `None` = the kind's default
    /// ([`crate::capability::KindCapabilities::delta`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<DeltaSupport>,
    /// Push mechanism of this collection; `None` = the kind's default
    /// ([`crate::capability::KindCapabilities::push`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<PushSupport>,
}

impl RemoteCollection {
    /// A non-primary collection without a default zone, with the kind's
    /// default delta and push.
    pub fn new(
        remote: CollectionRef,
        kind: EntityKind,
        name: impl Into<String>,
        access: Access,
    ) -> Self {
        RemoteCollection {
            remote,
            kind,
            name: name.into(),
            time_zone: None,
            access,
            primary: false,
            delta: None,
            push: None,
        }
    }
}

/// Access level of the account to a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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

/// How to start a read when there is no cursor. Non-exhaustive: build it with
/// [`FetchHint::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FetchHint {
    /// Which pass to start when `cursor` is `None`.
    pub start: Start,
    /// Preferred page size; the adapter caps it at `limits.max_page_size`.
    pub page_size: Option<NonZeroU32>,
}

impl FetchHint {
    /// A hint without a preferred page size.
    pub fn new(start: Start) -> Self {
        FetchHint {
            start,
            page_size: None,
        }
    }
}

/// Which pass to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum Pass {
    /// Window read: upserts only; absence means nothing.
    Window,
    /// Full read: objects absent from all pages of the pass are deleted, which
    /// the engine applies through [`crate::store::TenantStore::finish_full_read`].
    /// A stream that started from a delta cursor may switch to `Full` (a DAV
    /// collection whose ctag changed lists everything); the engine accepts it.
    Full,
    /// Delta since the cursor: upserts and removals.
    Delta,
}

/// One page of changes, written by the engine in one transaction together with
/// its cursor ("the whole page with its cursor, or nothing").
///
/// Non-exhaustive: build it with [`ChangePage::new`] and fill the vectors.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangePage {
    /// The pass this page belongs to; the same for all pages of one stream,
    /// except that a stream started from a delta cursor may yield `Full` pages.
    pub pass: Pass,
    /// New or changed objects with their bodies (or unchanged ones during a
    /// full read). `upserts` and `stubs` together hold at most
    /// `limits.max_page_size` entries, each key at most once per page.
    pub upserts: Vec<RemoteObject>,
    /// Objects listed without a body (DAV: href and ETag). They count as
    /// present in a full read. The engine compares each validator with the
    /// stored one: equal means unchanged; different, `None` or an unknown key
    /// means it fetches the body through [`Provider::fetch`] and writes it as
    /// an upsert of this page, in the page's transaction. A stub that `fetch`
    /// does not return is not written. The cursor is stored only after all
    /// bodies of the page are fetched. Google pages have no stubs.
    pub stubs: Vec<Stub>,
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

impl ChangePage {
    /// A page without upserts, stubs or removals.
    pub fn new(pass: Pass, cursor: Option<Cursor>, last: bool) -> Self {
        ChangePage {
            pass,
            upserts: Vec::new(),
            stubs: Vec::new(),
            removals: Vec::new(),
            cursor,
            last,
        }
    }
}

/// An object listed without its body: see [`ChangePage::stubs`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Stub {
    /// The object in the page's collection.
    pub key: RemoteKey,
    /// Its current validator (DAV `getetag` unchanged). `None` when the server
    /// sent none; the body is then always fetched.
    pub validator: Option<Validator>,
}

/// Provider id of a push channel (Google `X-Goog-Channel-ID`). Chosen by the adapter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelId(pub String);

/// An open push channel. Stored in the control store; the token is stored as
/// a secret. Non-exhaustive: build it with [`Subscription::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
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

impl Subscription {
    /// A subscription with these values.
    pub fn new(
        channel: ChannelId,
        collection: CollectionRef,
        resource: impl Into<String>,
        token: SecretString,
        expiration: Timestamp,
    ) -> Self {
        Subscription {
            channel,
            collection,
            resource: resource.into(),
            token,
            expiration,
        }
    }
}

/// What a verified notification means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PushHint {
    /// The channel was just opened (Google `X-Goog-Resource-State: sync`). Nothing changed.
    Handshake,
    /// The collection changed: mark it dirty; the engine coalesces notifications
    /// within 100–200 ms into one delta read.
    Changed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_meta_without_new_fields_still_reads() {
        // A `collections.meta` row as phase 1 wrote it.
        let old = r#"{"remote":"primary","kind":"event","name":"Work",
            "time_zone":"Europe/Prague","access":"owner","primary":true}"#;
        let c: RemoteCollection = serde_json::from_str(old).unwrap();
        assert_eq!((c.delta, c.push), (None, None));
        assert_eq!(c.time_zone.as_deref(), Some("Europe/Prague"));
        // Unset fields are not written, so phase 1 rows round-trip unchanged.
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(
            back,
            serde_json::from_str::<serde_json::Value>(old).unwrap()
        );

        let mut c = c;
        c.delta = Some(DeltaSupport::CtagEtag);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["delta"], "ctag_etag");
        assert_eq!(serde_json::from_value::<RemoteCollection>(v).unwrap(), c);
    }

    #[test]
    fn credential_debug_hides_secrets() {
        let c = Credential::Basic {
            username: "u".into(),
            password: SecretString::from("hunter2"),
        };
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
