//! CalDAV and CardDAV [`musubi_model::Provider`], profile `generic`
//! (docs/plan/phase2.md, chapters 5.1–5.3 and 5.5).
//!
//! - Discovery after RFC 6764 (well-known), RFC 4791 and RFC 6352, separately
//!   for CalDAV and CardDAV ([`DavProvider::connect`]).
//! - Deltas through `sync-collection`, ctag and ETags, or a plain ETag listing;
//!   bodies through `multiget`.
//! - The password goes only to the account's allowed origins, never over
//!   `http` except to loopback with `allow_private`, and never to a private
//!   address without it (`net`).
//!
//! The account's non-secret state (entries, allowed origins, principals, home
//! sets, collections, delta descents) lives in [`musubi_model::Endpoint`];
//! [`DavSetup::endpoint`] makes the first one.

mod discover;
mod net;
mod state;
mod sync;
mod xml;

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use musubi_model::capability::{DeltaSupport, KindCapabilities, Limits, ProviderId, PushSupport};
use musubi_model::error::BoxError;
use musubi_model::provider::{BoxStream, Discovery, FetchHint};
use musubi_model::remote::RemoteRef;
use musubi_model::{
    AccountCx, AccountId, ChangePage, CollectionRef, Cursor, EntityKind, Provider,
    ProviderDescriptor, ReauthReason, RemoteObject, SyncError,
};

pub use state::{DavSetup, Principals};

/// Largest page of [`Provider::changes`].
const MAX_PAGE: u32 = 500;
/// References per `multiget` (plan, chapter 3.4).
const MAX_BATCH: u32 = 50;

/// Instance configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct DavConfig {
    /// `dav.allow_private`: private addresses and `http` to loopback (tests
    /// with Radicale, a server on the LAN).
    pub allow_private: bool,
}

/// Why connecting an account failed: the reasons of `POST /v1/accounts/dav`
/// (plan, chapter 5.5) that the adapter can tell.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// The server rejected the credentials.
    #[error("unauthorized")]
    Unauthorized,
    /// No CalDAV or CardDAV server at the address.
    #[error("not a DAV server: {0}")]
    NotDav(String),
    /// The address resolves to a forbidden address (or is `http` off loopback).
    #[error("blocked address: {0}")]
    BlockedAddress(String),
    /// A redirect or home set outside the allowed origins; nothing was sent
    /// there. The user may confirm the target through `allow_origins`.
    #[error("redirect outside the allowed origins: {0}")]
    CrossOriginRedirect(String),
    /// A reconnect found other principals than the stored account.
    #[error("principal does not match the account")]
    SubjectMismatch,
    /// Bad setup (no entry URL, not a URL).
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// Anything else (network, server error).
    #[error(transparent)]
    Sync(#[from] SyncError),
}

/// The DAV adapter.
pub struct DavProvider {
    net: net::Net,
    /// Collections whose `sync-collection` failed with `405`/`501` since the
    /// last discovery; the next discovery writes them into `delta_override`.
    /// shortcut: lost on restart; the report fails again and re-records it.
    descents: Mutex<HashMap<AccountId, BTreeSet<String>>>,
}

impl DavProvider {
    /// Builds the HTTP clients. Installs the ring rustls provider as the
    /// process default if none is installed yet.
    pub fn new(config: DavConfig) -> Result<Self, BoxError> {
        Ok(Self {
            net: net::Net::new(config.allow_private)?,
            descents: Mutex::new(HashMap::new()),
        })
    }

    /// Discovers the account at `cx.endpoint` (from [`DavSetup::endpoint`]
    /// or a stored one). The returned [`Discovery::endpoint`] carries the
    /// principals ([`Principals::of`]), home sets and collections.
    pub async fn connect(&self, cx: &AccountCx) -> Result<Discovery, ConnectError> {
        discover::run(self, cx).await
    }

    fn descents(&self) -> std::sync::MutexGuard<'_, HashMap<AccountId, BTreeSet<String>>> {
        // Insert and remove only: a panic elsewhere leaves it consistent.
        self.descents.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl Provider for DavProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        let kinds = [
            EntityKind::Event,
            EntityKind::Task,
            EntityKind::Contact,
            EntityKind::ContactGroup,
        ]
        .into_iter()
        .map(|k| KindCapabilities::new(k, DeltaSupport::SyncToken, PushSupport::None))
        .collect();
        let mut limits = Limits::new(MAX_PAGE);
        limits.max_batch = Some(MAX_BATCH);
        limits.max_concurrency_per_account = Some(2);
        ProviderDescriptor::new(ProviderId("dav".into()), kinds, limits)
    }

    async fn discover(&self, cx: &AccountCx) -> Result<Discovery, SyncError> {
        self.connect(cx).await.map_err(|e| match e {
            ConnectError::Unauthorized => SyncError::ReauthRequired {
                reason: ReauthReason::Rejected,
            },
            ConnectError::CrossOriginRedirect(_) => SyncError::ReauthRequired {
                reason: ReauthReason::EndpointMoved,
            },
            ConnectError::Sync(e) => e,
            e => SyncError::Permanent(e.into()),
        })
    }

    fn changes<'a>(
        &'a self,
        cx: &'a AccountCx,
        col: &'a CollectionRef,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> BoxStream<'a, Result<ChangePage, SyncError>> {
        sync::changes(self, cx, col, cursor, hint)
    }

    async fn fetch(
        &self,
        cx: &AccountCx,
        refs: &[RemoteRef],
    ) -> Result<Vec<RemoteObject>, SyncError> {
        sync::fetch(self, cx, refs).await
    }
}
