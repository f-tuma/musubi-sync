//! HTTP transport: `Provider` for Google Calendar (`calendarList`, `events`,
//! OAuth token endpoint), with Google Tasks ([`crate::tasks`]) and People
//! ([`crate::people`]) routed by the collection reference.
//!
//! One instance serves all accounts; everything per account comes in
//! [`AccountCx`]. The adapter never retries, refreshes or sleeps on its own:
//! it maps each response to a [`SyncError`] and the engine decides.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use musubi_model::capability::{
    ClientIdCharset, ClientIdRule, ConditionalWrite, DeltaSupport, Fidelity, InviteSuppression,
    KindCapabilities, Limits, ProviderId, PushSupport, RecurrenceSupport, ReminderCapabilities,
    SchedulingCapabilities, SchedulingMode, TaskCapabilities, ZoneSupport,
};
use musubi_model::error::BoxError;
use musubi_model::event::AlertAction;
use musubi_model::provider::{
    Access, AccessToken, BoxStream, Clock, Credential, Discovery, FetchHint, Pass, PushSource,
    RemoteCollection, Start, TokenRefresh,
};
use musubi_model::remote::{MediaType, RemoteRef};
use musubi_model::{
    AccountCx, ChangePage, CollectionRef, Cursor, EntityKind, Provider, ProviderDescriptor, Raw,
    ReauthReason, RemoteKey, RemoteObject, SyncError, TimeWindow, Validator,
};
use reqwest::header::RETRY_AFTER;
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Largest `events.list` page Google serves.
pub(crate) const MAX_PAGE: u32 = 2500;
/// Restarts of one pass's paging after rejected page tokens before it fails.
const PAGE_RESTARTS: u32 = 2;

/// Endpoints and OAuth client of the adapter. [`GoogleConfig::new`] gives
/// Google's production endpoints; tests point them at the fake server.
#[derive(Debug, Clone)]
pub struct GoogleConfig {
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: SecretString,
    /// Calendar API root, e.g. `https://www.googleapis.com/calendar/v3`.
    pub api_base: Url,
    /// Tasks API root, e.g. `https://tasks.googleapis.com/tasks/v1`.
    pub tasks_base: Url,
    /// People API root, e.g. `https://people.googleapis.com/v1`.
    pub people_base: Url,
    /// OAuth token endpoint.
    pub token_url: Url,
    /// Discovery also lists task lists and the contacts collection (plan 6.1).
    /// Off until the engine syncs those kinds (K8); off, discovery is phase 1's.
    pub discover_tasks_people: bool,
}

impl GoogleConfig {
    /// Production endpoints with the given OAuth client.
    pub fn new(client_id: impl Into<String>, client_secret: SecretString) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret,
            api_base: Url::parse("https://www.googleapis.com/calendar/v3").expect("constant URL"),
            tasks_base: Url::parse("https://tasks.googleapis.com/tasks/v1").expect("constant URL"),
            people_base: Url::parse("https://people.googleapis.com/v1").expect("constant URL"),
            token_url: Url::parse("https://oauth2.googleapis.com/token").expect("constant URL"),
            discover_tasks_people: false,
        }
    }
}

/// The Google Calendar adapter: [`Provider`] and [`PushSource`].
pub struct GoogleProvider {
    pub(crate) http: reqwest::Client,
    pub(crate) config: GoogleConfig,
    /// Used by [`PushSource::verify`], which has no [`AccountCx`].
    pub(crate) clock: Arc<dyn Clock>,
}

impl fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleProvider")
            .field("api_base", &self.config.api_base.as_str())
            .finish_non_exhaustive()
    }
}

impl GoogleProvider {
    /// Builds the adapter and its HTTP client.
    ///
    /// Installs rustls' `ring` provider as the process default if none is
    /// installed yet: reqwest is built with `rustls-no-provider` (no aws-lc-rs)
    /// and takes the process default.
    pub fn new(config: GoogleConfig, clock: Arc<dyn Clock>) -> Result<Self, BoxError> {
        for base in [&config.api_base, &config.tasks_base, &config.people_base] {
            if base.cannot_be_a_base() {
                return Err(format!("API base {base} cannot be a base URL").into());
            }
        }
        // Err only means a provider is installed already, which is what we need.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            config,
            clock,
        })
    }

    /// `api_base` + percent-encoded path segments.
    pub(crate) fn url(&self, segments: &[&str]) -> Url {
        join(&self.config.api_base, segments)
    }

    /// Waits for the account's limiter, authorises and sends. Only transport
    /// failures are errors here; the caller looks at the status.
    pub(crate) async fn send(
        &self,
        cx: &AccountCx,
        req: RequestBuilder,
    ) -> Result<Response, SyncError> {
        let Credential::Bearer(token) = &cx.credential else {
            return Err(permanent("Google needs an OAuth bearer token"));
        };
        cx.limiter.acquire().await;
        req.bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(|e| SyncError::Transient(e.into()))
    }

    /// [`Self::send`], then the body of a 2xx response or the mapped error
    /// (`scope`: see [`error_for`]).
    pub(crate) async fn call(
        &self,
        cx: &AccountCx,
        req: RequestBuilder,
        scope: Option<EntityKind>,
    ) -> Result<Vec<u8>, SyncError> {
        let resp = self.send(cx, req).await?;
        if !resp.status().is_success() {
            return Err(error_for(resp, cx.clock.now(), scope).await);
        }
        body(resp).await
    }

    /// One `events.list` request. `Ok(None)`: Google rejected the page token
    /// (`400 invalid`), the caller restarts the pass's paging.
    async fn events_page(
        &self,
        cx: &AccountCx,
        col: &CollectionRef,
        q: &Query,
        page_size: u32,
    ) -> Result<Option<(ChangePage, Option<Query>)>, SyncError> {
        let mut url = self.url(&["calendars", &col.0, "events"]);
        {
            let mut p = url.query_pairs_mut();
            p.append_pair("maxResults", &page_size.to_string());
            if let Some(window) = q.window {
                p.append_pair("timeMin", &window.start().to_string());
                p.append_pair("timeMax", &window.end().to_string());
            }
            // Google's own incremental-sync sample keeps syncToken on every page.
            if let Some(sync) = &q.sync {
                p.append_pair("syncToken", sync);
            }
            if let Some(page) = &q.page {
                p.append_pair("pageToken", page);
            }
        }
        let resp = self.send(cx, self.http.get(url)).await?;
        let status = resp.status();
        // Live (K8): an expired or bogus sync token answers 410, a bogus page
        // token `400 invalid` ("Invalid page token value"). The calendar id is
        // not logged: a primary calendar's id is the user's address.
        if status == StatusCode::GONE {
            let body = resp.bytes().await.unwrap_or_default();
            tracing::warn!(status = 410, body = %excerpt(&body), pass = ?q.pass, "events.list: cursor expired");
            return Err(SyncError::CursorExpired);
        }
        if status == StatusCode::BAD_REQUEST && q.page.is_some() {
            let body = resp.bytes().await.unwrap_or_default();
            if has_reason(&body, &["invalid"]) {
                tracing::warn!(body = %excerpt(&body), pass = ?q.pass, "events.list: page token rejected; restarting the pass");
                return Ok(None);
            }
            return Err(SyncError::Permanent(
                HttpStatus {
                    status,
                    body: excerpt(&body),
                }
                .into(),
            ));
        }
        if !status.is_success() {
            return Err(error_for(resp, cx.clock.now(), None).await);
        }
        build_page(q, &body(resp).await?).map(Some)
    }
}

/// `base` + percent-encoded path segments.
pub(crate) fn join(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(segments);
    }
    url
}

pub(crate) async fn body(resp: Response) -> Result<Vec<u8>, SyncError> {
    resp.bytes()
        .await
        .map(Vec::from)
        .map_err(|e| SyncError::Transient(e.into()))
}

/// A non-2xx response that is neither throttling nor an auth problem.
#[derive(Debug, thiserror::Error)]
#[error("HTTP {status}: {body}")]
pub(crate) struct HttpStatus {
    status: StatusCode,
    body: String,
}

pub(crate) fn permanent(msg: impl Into<String>) -> SyncError {
    SyncError::Permanent(msg.into().into())
}

/// Maps a non-2xx API response. Call sites handle the statuses that mean
/// something specific to them (404/410 "gone", 410 "sync token expired").
///
/// `scope`: the kind a `403` for a missing OAuth scope (plan 6.1) is
/// [`SyncError::MissingScope`] for. `None` (Calendar, whose scope every
/// account has, and push) keeps such a `403` `Permanent`.
pub(crate) async fn error_for(
    resp: Response,
    now: Timestamp,
    scope: Option<EntityKind>,
) -> SyncError {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| retry_after(v, now));
    let body = resp.bytes().await.unwrap_or_default();
    match (status.as_u16(), scope) {
        (401, _) => SyncError::Unauthorized,
        (429, _) => SyncError::RateLimited { retry_after },
        (403, _) if is_rate_limit(&body) => SyncError::RateLimited { retry_after },
        // A disabled API needs the same outside fix as a missing grant.
        (403, Some(kind)) if is_missing_scope(&body) || is_api_disabled(&body) => {
            SyncError::MissingScope { kind }
        }
        _ => {
            let err = HttpStatus {
                status,
                body: excerpt(&body),
            };
            // Logged by the caller, which knows the job and the collection.
            if status.is_server_error() {
                SyncError::Transient(err.into())
            } else {
                SyncError::Permanent(err.into())
            }
        }
    }
}

/// The first 300 bytes of an error body, for logs and error messages. Google's
/// error bodies hold the error object only, no tokens or event data.
/// Control characters are escaped: a newline in a body must not forge a log line.
pub(crate) fn excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(300)]);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// `403` for an API the OAuth client's Cloud project has not enabled, in the
/// classic and the AIP-193 shape.
fn is_api_disabled(body: &[u8]) -> bool {
    has_reason(body, &["accessNotConfigured", "SERVICE_DISABLED"])
}

/// `403` with a quota reason is throttling, not a permission error.
fn is_rate_limit(body: &[u8]) -> bool {
    has_reason(
        body,
        &[
            "rateLimitExceeded",
            "userRateLimitExceeded",
            "quotaExceeded",
            "dailyLimitExceeded",
        ],
    )
}

/// `403` for an OAuth scope the user did not grant, in either shape Google
/// uses (plan 6.1; which API sends which is unverified, T10): the older
/// `errors[].reason` and the newer `google.rpc.ErrorInfo` in `details[]`.
pub(crate) fn is_missing_scope(body: &[u8]) -> bool {
    has_reason(
        body,
        &["insufficientPermissions", "ACCESS_TOKEN_SCOPE_INSUFFICIENT"],
    )
}

/// Whether a Google error body carries one of `reasons`, in `error.errors[]`
/// (older APIs) or in a `google.rpc.ErrorInfo` of `error.details[]` (newer).
pub(crate) fn has_reason(body: &[u8], reasons: &[&str]) -> bool {
    #[derive(Deserialize)]
    struct Body {
        error: Inner,
    }
    #[derive(Deserialize)]
    struct Inner {
        #[serde(default)]
        errors: Vec<Reason>,
        #[serde(default)]
        details: Vec<Reason>,
    }
    #[derive(Deserialize)]
    struct Reason {
        #[serde(default)]
        reason: String,
    }
    serde_json::from_slice::<Body>(body).is_ok_and(|b| {
        b.error
            .errors
            .iter()
            .chain(&b.error.details)
            .any(|r| reasons.contains(&r.reason.as_str()))
    })
}

/// `Retry-After`: delay in seconds or an HTTP date (RFC 9110).
pub(crate) fn retry_after(v: &str, now: Timestamp) -> Option<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(v)
        .ok()?;
    // A date in the past means "now".
    Some(Duration::try_from(at.duration_since(now)).unwrap_or(Duration::ZERO))
}

/// What the engine stores as [`Cursor`]: JSON, opaque to everyone else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum CursorState {
    /// Mid full read: the next page.
    Full { page: String },
    /// Delta from `sync`; `page` while a delta spans several pages.
    Delta {
        sync: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<String>,
    },
    /// A read ended without `nextSyncToken`: no delta is possible, the next
    /// sync is a full read again (the engine backs those off).
    FullOnly,
}

impl CursorState {
    fn to_cursor(&self) -> Cursor {
        Cursor(serde_json::to_vec(self).unwrap_or_default())
    }
}

/// One `events.list` request of a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Query {
    pass: Pass,
    window: Option<TimeWindow>,
    sync: Option<String>,
    page: Option<String>,
}

impl Query {
    fn start(cursor: Option<Cursor>, hint: FetchHint) -> Result<Self, SyncError> {
        let q = |pass, window, sync, page| Query {
            pass,
            window,
            sync,
            page,
        };
        Ok(match cursor {
            None => match hint.start {
                Start::Full => q(Pass::Full, None, None, None),
                Start::Window(w) => q(Pass::Window, Some(w), None, None),
                _ => return Err(permanent("unsupported start of a read")),
            },
            // A cursor we cannot read (older format, corruption) is as good as expired.
            Some(c) => match serde_json::from_slice(&c.0) {
                Ok(CursorState::Full { page }) => q(Pass::Full, None, None, Some(page)),
                Ok(CursorState::Delta { sync, page }) => q(Pass::Delta, None, Some(sync), page),
                Ok(CursorState::FullOnly) => {
                    tracing::info!("no delta cursor (last read had no nextSyncToken); full read");
                    return Err(SyncError::CursorExpired);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "unreadable cursor; full read");
                    return Err(SyncError::CursorExpired);
                }
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsPage {
    #[serde(default)]
    items: Vec<Box<RawValue>>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
}

/// The fields of an event the transport itself needs.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventMeta {
    id: String,
    etag: Option<String>,
    status: Option<String>,
    recurring_event_id: Option<String>,
}

impl EventMeta {
    /// Google `status: cancelled` without `recurringEventId`: the event is
    /// gone. A cancelled exception is a live object (a removed occurrence).
    fn is_deleted(&self) -> bool {
        self.status.as_deref() == Some("cancelled") && self.recurring_event_id.is_none()
    }
}

/// One event exactly as received: the raw layer keeps these bytes.
fn remote_object(raw: &RawValue, meta: &EventMeta) -> RemoteObject {
    let mut object = raw_object(raw, &meta.id, EntityKind::Event, meta.etag.as_deref());
    object.master = meta.recurring_event_id.clone().map(RemoteKey);
    object
}

/// One item exactly as received, validated by its `etag` (a hash of the
/// bytes when Google sends none).
pub(crate) fn raw_object(
    raw: &RawValue,
    key: &str,
    kind: EntityKind,
    etag: Option<&str>,
) -> RemoteObject {
    let raw = Raw {
        media_type: MediaType::Json,
        body: raw.get().as_bytes().to_vec(),
    };
    let validator = match etag {
        Some(etag) => etag.to_owned(),
        None => {
            let hex: String = raw.hash().0.iter().map(|b| format!("{b:02x}")).collect();
            format!("sha256:{hex}")
        }
    };
    RemoteObject::new(RemoteKey(key.to_owned()), kind, Validator(validator), raw)
}

/// The pages of a pass that has no restarts (Tasks, People): `page` reads one
/// request and returns the next one, `None` after the last page.
pub(crate) fn pages<'a, Q, F, Fut>(
    first: Result<Q, SyncError>,
    page: F,
) -> BoxStream<'a, Result<ChangePage, SyncError>>
where
    Q: Send + 'a,
    F: FnMut(Q) -> Fut + Send + 'a,
    Fut: std::future::Future<Output = Result<(ChangePage, Option<Q>), SyncError>> + Send + 'a,
{
    Box::pin(futures_util::stream::unfold(
        (Some(first), page),
        |(state, mut page)| async move {
            let result = match state? {
                Ok(q) => page(q).await,
                Err(e) => Err(e),
            };
            Some(match result {
                Ok((p, next)) => (Ok(p), (next.map(Ok), page)),
                Err(e) => (Err(e), (None, page)),
            })
        },
    ))
}

/// Which API a collection belongs to. Task lists are `tasks/<tasklist id>`,
/// the contacts are [`crate::people::COLLECTION`]; everything else is a
/// calendar id, as in phase 1 (calendar ids are e-mail-like or `primary`).
enum Target<'a> {
    Calendar,
    Tasks(&'a str),
    People,
}

impl<'a> Target<'a> {
    fn of(col: &'a CollectionRef) -> Self {
        if col.0 == crate::people::COLLECTION {
            Target::People
        } else if let Some(list) = col.0.strip_prefix(crate::tasks::PREFIX) {
            Target::Tasks(list)
        } else {
            Target::Calendar
        }
    }
}

/// Turns one `events.list` response into a page and the next request.
fn build_page(q: &Query, body: &[u8]) -> Result<(ChangePage, Option<Query>), SyncError> {
    let resp: EventsPage =
        serde_json::from_slice(body).map_err(|e| permanent(format!("events.list: {e}")))?;
    let mut upserts = Vec::new();
    let mut removals = Vec::new();
    let mut seen = HashSet::new();
    // Each key at most once per page; the last occurrence is the newest.
    for raw in resp.items.iter().rev() {
        let meta: EventMeta =
            serde_json::from_str(raw.get()).map_err(|e| permanent(format!("event: {e}")))?;
        if !seen.insert(meta.id.clone()) {
            continue;
        }
        match (meta.is_deleted(), q.pass) {
            // A window read carries upserts only; absence means nothing there.
            (true, Pass::Window) => {}
            (true, _) => removals.push(RemoteKey(meta.id)),
            (false, _) => upserts.push(remote_object(raw, &meta)),
        }
    }
    upserts.reverse();
    removals.reverse();

    let (cursor, next) = match resp.next_page_token {
        Some(page) => {
            let next = Query {
                page: Some(page.clone()),
                ..q.clone()
            };
            let cursor = match &q.sync {
                _ if q.pass == Pass::Window => None,
                Some(sync) => Some(CursorState::Delta {
                    sync: sync.clone(),
                    page: Some(page),
                }),
                None => Some(CursorState::Full { page }),
            };
            (cursor, Some(next))
        }
        None if q.pass == Pass::Window => (None, None),
        None => match resp.next_sync_token {
            Some(sync) => (Some(CursorState::Delta { sync, page: None }), None),
            None => {
                tracing::warn!(pass = ?q.pass, "last events.list page without nextSyncToken");
                (Some(CursorState::FullOnly), None)
            }
        },
    };
    let mut page = ChangePage::new(q.pass, cursor.map(|c| c.to_cursor()), next.is_none());
    page.upserts = upserts;
    page.removals = removals;
    Ok((page, next))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarList {
    #[serde(default)]
    items: Vec<CalendarListEntry>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarListEntry {
    id: String,
    summary: Option<String>,
    summary_override: Option<String>,
    time_zone: Option<String>,
    access_role: Option<String>,
    #[serde(default)]
    primary: bool,
}

impl From<CalendarListEntry> for RemoteCollection {
    fn from(c: CalendarListEntry) -> Self {
        let access = match c.access_role.as_deref() {
            Some("owner") => Access::Owner,
            Some("writer") => Access::Writer,
            Some("freeBusyReader") => Access::FreeBusyReader,
            _ => Access::Reader,
        };
        let name = c.summary_override.or(c.summary).unwrap_or_default();
        let mut col = RemoteCollection::new(CollectionRef(c.id), EntityKind::Event, name, access);
        col.time_zone = c.time_zone;
        col.primary = c.primary;
        col
    }
}

/// RFC 6749 5.1: snake_case, unlike the Calendar API.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
}

#[async_trait]
impl Provider for GoogleProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        let fidelity = [
            ("title", Fidelity::Exact),
            ("description", Fidelity::Exact),
            // One free-text location.
            ("locations", Fidelity::Approximate),
            ("start", Fidelity::Exact),
            // No floating times: a floating timed event gets a zone.
            ("timeZone", Fidelity::Approximate),
            ("duration", Fidelity::Exact),
            ("showWithoutTime", Fidelity::Exact),
            ("status", Fidelity::Exact),
            ("freeBusyStatus", Fidelity::Exact),
            // `private` and `confidential` are both Private; no Secret.
            ("privacy", Fidelity::Approximate),
            ("recurrenceRules", Fidelity::Exact),
            ("excludedRecurrenceRules", Fidelity::Exact),
            ("recurrenceOverrides", Fidelity::Exact),
            // Only "optional"; no chair or informational roles.
            ("participants", Fidelity::Approximate),
            // Relative popup/e-mail only.
            ("alerts", Fidelity::Approximate),
        ];
        let mut events = KindCapabilities::new(
            EntityKind::Event,
            DeltaSupport::SyncToken,
            PushSupport::Webhook,
        );
        // Phase 1 is read-only.
        events.conditional_update = ConditionalWrite::StrongValidator;
        events.conditional_delete = ConditionalWrite::Unproven;
        events.client_id = Some(ClientIdRule::new(ClientIdCharset::Base32HexLower, 5, 1024));
        // Read-only scopes (plan 6.1); no push for either (plan 6.2, 6.3).
        let tasks = KindCapabilities::new(
            EntityKind::Task,
            DeltaSupport::WindowedDelta,
            PushSupport::None,
        );
        let contacts = KindCapabilities::new(
            EntityKind::Contact,
            DeltaSupport::SyncToken,
            PushSupport::None,
        );
        let mut limits = Limits::new(MAX_PAGE);
        limits.requests_per_minute = Some(600);
        limits.max_batch = Some(1000);
        let mut d = ProviderDescriptor::new(
            ProviderId("google".into()),
            vec![events, tasks, contacts],
            limits,
        );
        // Due dates without time, subtasks via `parent`, no recurring tasks in the API.
        d.tasks = Some(TaskCapabilities::new(true, true, false));
        d.recurrence = RecurrenceSupport::Full;
        d.zones = vec![ZoneSupport::Iana];
        d.scheduling = SchedulingCapabilities::new(
            SchedulingMode::ProviderSide,
            InviteSuppression::WholeRequest,
            false,
        );
        d.field_fidelity = fidelity
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<BTreeMap<_, _>>();
        // extendedProperties.private
        d.hidden_marker = true;
        d.reminders = ReminderCapabilities::new(
            true,
            false,
            vec![AlertAction::Display, AlertAction::Email],
            Some(5),
        );
        d
    }

    async fn discover(&self, cx: &AccountCx) -> Result<Discovery, SyncError> {
        let mut found: Vec<RemoteCollection> = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut url = self.url(&["users", "me", "calendarList"]);
            // Calendars hidden in Google's UI are still synced; Ok means the
            // complete list, and a missing calendar would count as deleted.
            url.query_pairs_mut()
                .append_pair("maxResults", "250")
                .append_pair("showHidden", "true");
            if let Some(p) = &page {
                url.query_pairs_mut().append_pair("pageToken", p);
            }
            let result = self
                .call(cx, self.http.get(url), None)
                .await
                .and_then(|body| {
                    serde_json::from_slice::<CalendarList>(&body)
                        .map_err(|e| permanent(format!("calendarList: {e}")))
                });
            let list = match result {
                Ok(list) => list,
                Err(e) if found.is_empty() && page.is_none() => return Err(e),
                Err(e) => {
                    return Err(SyncError::PartialDiscovery {
                        found,
                        source: e.into(),
                    });
                }
            };
            found.extend(list.items.into_iter().map(RemoteCollection::from));
            match list.next_page_token {
                Some(next) => page = Some(next),
                None => break,
            }
        }
        if !self.config.discover_tasks_people {
            return Ok(Discovery::new(found));
        }
        // Tasks and People: a scope the user did not grant, or an API the
        // Cloud project has not enabled, is an unauthorised kind, not an error
        // (plan 6.1). Any other probe failure (throttling, server error,
        // refused token, other 403) skips that kind for this round: a partial
        // listing with the calendars and whatever kinds did succeed, which the
        // engine only adds from. It never fails or blocks the account, and the
        // kind is not unauthorised, so its collections keep their state.
        let tasks = self.task_lists(cx, &mut found).await;
        let people = self.people_probe(cx, &mut found).await;
        let mut unauthorized = Vec::new();
        let mut failed = None;
        for r in [tasks, people] {
            match r {
                Ok(()) => {}
                Err(SyncError::MissingScope { kind }) => unauthorized.push(kind),
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        if let Some(e) = failed {
            return Err(SyncError::PartialDiscovery {
                found,
                source: e.into(),
            });
        }
        let mut d = Discovery::new(found);
        d.unauthorized = unauthorized;
        Ok(d)
    }

    fn changes<'a>(
        &'a self,
        cx: &'a AccountCx,
        col: &'a CollectionRef,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> BoxStream<'a, Result<ChangePage, SyncError>> {
        match Target::of(col) {
            Target::Tasks(list) => return self.task_changes(cx, list, cursor, hint),
            Target::People => return self.people_changes(cx, cursor, hint),
            Target::Calendar => {}
        }
        let page_size = hint.page_size.map_or(MAX_PAGE, |n| n.get().min(MAX_PAGE));
        let first = Query::start(cursor, hint).map(|q| (q, 0));
        Box::pin(futures_util::stream::unfold(
            Some(first),
            move |state| async move {
                let (mut q, mut restarts) = match state? {
                    Ok(s) => s,
                    Err(e) => return Some((Err(e), None)),
                };
                loop {
                    match self.events_page(cx, col, &q, page_size).await {
                        Ok(Some((page, next))) => {
                            return Some((Ok(page), next.map(|n| Ok((n, restarts)))));
                        }
                        // The pass starts over (pages already read are read
                        // again; at-least-once), a few times per call.
                        Ok(None) if restarts < PAGE_RESTARTS => {
                            restarts += 1;
                            q.page = None;
                        }
                        Ok(None) => {
                            let msg = format!(
                                "events.list: page token rejected {} times in one pass",
                                restarts + 1
                            );
                            return Some((Err(SyncError::Transient(msg.into())), None));
                        }
                        Err(e) => return Some((Err(e), None)),
                    }
                }
            },
        ))
    }

    async fn fetch(
        &self,
        cx: &AccountCx,
        refs: &[RemoteRef],
    ) -> Result<Vec<RemoteObject>, SyncError> {
        let mut out = Vec::with_capacity(refs.len());
        for r in refs {
            match Target::of(&r.collection) {
                Target::Tasks(list) => {
                    out.extend(self.fetch_task(cx, list, &r.key).await?);
                    continue;
                }
                Target::People => {
                    out.extend(self.fetch_person(cx, &r.key).await?);
                    continue;
                }
                Target::Calendar => {}
            }
            let url = self.url(&["calendars", &r.collection.0, "events", &r.key.0]);
            let resp = self.send(cx, self.http.get(url)).await?;
            match resp.status() {
                StatusCode::NOT_FOUND | StatusCode::GONE => continue,
                s if !s.is_success() => return Err(error_for(resp, cx.clock.now(), None).await),
                _ => {}
            }
            let body = body(resp).await?;
            let raw: Box<RawValue> =
                serde_json::from_slice(&body).map_err(|e| permanent(format!("events.get: {e}")))?;
            let meta: EventMeta =
                serde_json::from_str(raw.get()).map_err(|e| permanent(format!("event: {e}")))?;
            if !meta.is_deleted() {
                out.push(remote_object(&raw, &meta));
            }
        }
        Ok(out)
    }

    fn oauth(&self) -> Option<&dyn TokenRefresh> {
        Some(self)
    }

    fn push(&self) -> Option<&dyn PushSource> {
        Some(self)
    }
}

#[async_trait]
impl TokenRefresh for GoogleProvider {
    async fn refresh(&self, refresh_token: &SecretString) -> Result<AccessToken, SyncError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose_secret()),
            ("client_id", self.config.client_id.as_str()),
            ("client_secret", self.config.client_secret.expose_secret()),
        ];
        let resp = self
            .http
            .post(self.config.token_url.clone())
            .form(&form)
            .send()
            .await
            .map_err(|e| SyncError::Transient(e.into()))?;
        let status = resp.status();
        if status.is_success() {
            let t: TokenResponse = serde_json::from_slice(&body(resp).await?)
                .map_err(|e| permanent(format!("token response: {e}")))?;
            // Google always sends `expires_in`; an hour is its documented lifetime.
            let expires_in = Duration::from_secs(t.expires_in.unwrap_or(3600));
            let mut token = AccessToken::new(t.access_token.into(), expires_in);
            token.rotated_refresh = t.refresh_token.map(Into::into);
            return Ok(token);
        }
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
            let body = body(resp).await?;
            // RFC 6749 5.2: the grant is revoked, expired or for another client.
            return match serde_json::from_slice::<TokenError>(&body) {
                Ok(e) if e.error == "invalid_grant" => Err(SyncError::ReauthRequired {
                    reason: ReauthReason::Rejected,
                }),
                _ => Err(SyncError::Permanent(
                    HttpStatus {
                        status,
                        body: excerpt(&body),
                    }
                    .into(),
                )),
            };
        }
        Err(error_for(resp, self.clock.now(), None).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> TimeWindow {
        TimeWindow::new(
            "2026-11-01T00:00:00Z".parse().unwrap(),
            "2026-12-01T00:00:00Z".parse().unwrap(),
        )
        .unwrap()
    }

    fn query(pass: Pass) -> Query {
        Query {
            pass,
            window: None,
            sync: None,
            page: None,
        }
    }

    #[test]
    fn error_excerpt_cannot_forge_a_log_line() {
        let body = b"{\"error\":1}\n2026-11-12T00:00:00Z  INFO forged\r\x1b[31m";
        let e = excerpt(body);
        assert!(!e.chars().any(char::is_control), "{e}");
        assert!(e.contains(r"\n2026") && e.contains(r"\r"), "{e}");
    }

    #[test]
    fn unknown_field_stays_byte_for_byte_in_the_raw_layer() {
        let item = include_str!("../tests/fixtures/events/unknown_field.json").trim();
        let body =
            format!(r#"{{"kind":"calendar#events","items":[ {item} ],"nextSyncToken":"s1"}}"#);
        let (page, _) = build_page(&query(Pass::Full), body.as_bytes()).unwrap();
        assert_eq!(page.upserts.len(), 1);
        assert_eq!(page.upserts[0].raw.body, item.as_bytes());
        assert!(std::str::from_utf8(&page.upserts[0].raw.body).unwrap().contains(
            r#""futureFeature": {"mode": "hologram", "levels": [1, 2, 3], "note": "kept byte for byte"}"#
        ));
        assert_eq!(
            page.upserts[0].validator,
            Validator(r#""3425621400000000""#.into())
        );
    }

    #[test]
    fn deleted_events_are_removals_and_cancelled_exceptions_upserts() {
        let body = br#"{"items":[
            {"id":"a","etag":"\"1\"","status":"confirmed"},
            {"id":"gone","status":"cancelled"},
            {"id":"s_1","status":"cancelled","recurringEventId":"s"},
            {"id":"a","etag":"\"2\"","status":"confirmed"}
        ],"nextSyncToken":"s2"}"#;
        let q = Query {
            sync: Some("s1".into()),
            ..query(Pass::Delta)
        };
        let (page, next) = build_page(&q, body).unwrap();
        let keys: Vec<_> = page.upserts.iter().map(|o| o.key.0.as_str()).collect();
        assert_eq!(keys, ["s_1", "a"]);
        assert_eq!(page.upserts[1].validator, Validator(r#""2""#.into()));
        assert!(page.upserts[0].validator.0.starts_with("sha256:"));
        assert_eq!(page.upserts[0].master, Some(RemoteKey("s".into())));
        assert_eq!(page.upserts[1].master, None);
        assert_eq!(page.removals, [RemoteKey("gone".into())]);
        assert!(page.last && next.is_none());
        assert_eq!(
            Query::start(page.cursor, FetchHint::new(Start::Full)).unwrap(),
            Query {
                sync: Some("s2".into()),
                ..query(Pass::Delta)
            }
        );
        // Window reads drop deletions entirely.
        let (page, _) = build_page(
            &Query {
                window: Some(window()),
                ..query(Pass::Window)
            },
            body,
        )
        .unwrap();
        assert!(page.removals.is_empty() && page.cursor.is_none());
    }

    #[test]
    fn cursors_resume_mid_pass() {
        let mid = br#"{"items":[],"nextPageToken":"p2"}"#;
        let (page, next) = build_page(&query(Pass::Full), mid).unwrap();
        assert!(!page.last && page.upserts.is_empty());
        let resumed = Query::start(page.cursor, FetchHint::new(Start::Full)).unwrap();
        assert_eq!(Some(resumed.clone()), next);
        assert_eq!(
            resumed,
            Query {
                page: Some("p2".into()),
                ..query(Pass::Full)
            }
        );

        let delta = Query {
            sync: Some("s1".into()),
            ..query(Pass::Delta)
        };
        let (page, _) = build_page(&delta, mid).unwrap();
        let resumed = Query::start(page.cursor, FetchHint::new(Start::Full)).unwrap();
        assert_eq!(
            resumed,
            Query {
                page: Some("p2".into()),
                ..delta
            }
        );

        let (page, _) = build_page(
            &Query {
                window: Some(window()),
                ..query(Pass::Window)
            },
            mid,
        )
        .unwrap();
        assert!(page.cursor.is_none() && !page.last);

        // No nextSyncToken: the read completes, and its cursor asks for a full read.
        let (page, next) = build_page(&query(Pass::Full), br#"{"items":[]}"#).unwrap();
        assert!(page.last && next.is_none());
        assert!(matches!(
            Query::start(page.cursor, FetchHint::new(Start::Full)),
            Err(SyncError::CursorExpired)
        ));
        assert!(matches!(
            Query::start(
                Some(Cursor(b"garbage".to_vec())),
                FetchHint::new(Start::Full)
            ),
            Err(SyncError::CursorExpired)
        ));
    }

    #[test]
    fn retry_after_takes_seconds_or_an_http_date() {
        let now: Timestamp = "2026-11-11T12:00:00Z".parse().unwrap();
        assert_eq!(retry_after("120", now), Some(Duration::from_secs(120)));
        assert_eq!(
            retry_after("Wed, 11 Nov 2026 12:00:30 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            retry_after("Wed, 11 Nov 2026 11:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after("soon", now), None);
    }

    #[test]
    fn rate_limit_403_is_told_apart_from_forbidden() {
        assert!(is_rate_limit(
            br#"{"error":{"errors":[{"reason":"rateLimitExceeded"}],"code":403}}"#
        ));
        assert!(!is_rate_limit(
            br#"{"error":{"errors":[{"reason":"forbidden"}],"code":403}}"#
        ));
        assert!(!is_rate_limit(b"<html>"));
    }
}
