//! Hermetic in-process fake of Google Calendar API v3 and Google's OAuth token
//! endpoint (plan, K3 and 6.2). Deviations from Google's documentation are in
//! `crates/musubi-testkit/FIDELITY.md`.
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! use musubi_testkit::google::{Config, FakeGoogle, Seed};
//! let g = FakeGoogle::start(Config { seed: Seed { seed: 7, calendars: 2, events: 50 }, ..Config::default() }).await?;
//! // Point the adapter at these instead of www.googleapis.com / oauth2.googleapis.com:
//! let api = g.calendar_api(); // http://127.0.0.1:<port>/calendar/v3
//! let token = g.token_url();  // http://127.0.0.1:<port>/token
//! let refresh = g.refresh_token();
//! # Ok(()) }
//! ```
//!
//! Routes (paths relative to [`FakeGoogle::base_url`]):
//!
//! | Route | Google |
//! | --- | --- |
//! | `GET /calendar/v3/users/me/calendarList` | `calendarList.list` |
//! | `GET /calendar/v3/calendars/{cal}/events` | `events.list` |
//! | `GET /calendar/v3/calendars/{cal}/events/{id}` | `events.get` |
//! | `POST /calendar/v3/calendars/{cal}/events/watch` | `events.watch` |
//! | `POST /calendar/v3/channels/stop` | `channels.stop` |
//! | `GET /o/oauth2/v2/auth` | `https://accounts.google.com/o/oauth2/v2/auth` (consent granted at once) |
//! | `POST /token` | `https://oauth2.googleapis.com/token` (`refresh_token` and `authorization_code` grants) |
//!
//! Every API route needs `Authorization: Bearer <token>` with a token issued by
//! `/token` or [`FakeGoogle::access_token`]. Must run inside a tokio runtime.

#![allow(
    clippy::result_large_err,
    reason = "handlers short-circuit with the HTTP error response itself; size is irrelevant in a test fake"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Form, Path, Query, State as AxState};
use axum::http::{HeaderMap, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hyper_util::rt::TokioIo;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Google's hard cap on `maxResults` for `events.list`.
pub const MAX_EVENTS_PAGE: u32 = 2500;
/// Google's default `maxResults` for `events.list`.
pub const DEFAULT_EVENTS_PAGE: u32 = 250;
/// Default `params.ttl` of `events.watch` (604 800 s, 7 days).
pub const DEFAULT_WATCH_TTL: Duration = Duration::from_secs(604_800);

/// Server configuration. All fields have working defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// Initial data, generated deterministically.
    pub seed: Seed,
    /// Base of the logical clock: seeded events lie around it and `created` /
    /// `updated` are `epoch + version` milliseconds, so bodies are reproducible.
    pub epoch: Timestamp,
    /// Delay before every response (benchmarks B1/B2 use 150 ms).
    pub latency: Duration,
    /// Server-side page cap below `maxResults` (Google may return fewer).
    pub max_page_size: u32,
    /// The first response of every `events.list` pass is an empty page with
    /// `nextPageToken` (Google does this; a client must not stop there).
    pub empty_first_page: bool,
    /// After every this many pages with items, one empty page with
    /// `nextPageToken` (0 = never). With a small `max_page_size` this puts
    /// empty pages in the middle of a read, as Google does.
    pub empty_page_every: u32,
    /// The account's valid refresh token.
    pub refresh_token: String,
    /// `/token` returns a new refresh token and revokes the old one.
    pub rotate_refresh_tokens: bool,
    /// Google account id (`sub` of the `id_token`) the authorization-code grant reports.
    pub subject: String,
    /// Channel lifetime when the watch request sets neither `expiration` nor `params.ttl`.
    pub default_watch_ttl: Duration,
    /// Longest channel lifetime the server grants (Google's maximum is undocumented, T8).
    pub max_watch_ttl: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seed: Seed::default(),
            epoch: "2026-11-11T00:00:00Z".parse().expect("valid constant"),
            latency: Duration::ZERO,
            max_page_size: MAX_EVENTS_PAGE,
            empty_first_page: false,
            empty_page_every: 0,
            refresh_token: "1//fake-refresh-token".into(),
            rotate_refresh_tokens: false,
            subject: "100000000000000000001".into(),
            default_watch_ttl: DEFAULT_WATCH_TTL,
            max_watch_ttl: DEFAULT_WATCH_TTL,
        }
    }
}

/// Deterministic initial data: the same seed gives byte-identical state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Seed {
    /// PRNG seed.
    pub seed: u64,
    /// Secondary calendars besides the primary one.
    pub calendars: u32,
    /// Top-level events per calendar. Every tenth is a weekly series with one
    /// moved and one cancelled instance (two extra exception events).
    pub events: u32,
}

/// Request class, for [`FakeGoogle::fail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// Matches every route.
    Any,
    /// `calendarList.list`.
    CalendarList,
    /// `events.list`.
    EventsList,
    /// `events.get`.
    EventsGet,
    /// `events.watch`.
    Watch,
    /// `channels.stop`.
    Stop,
    /// OAuth token endpoint.
    Token,
}

/// A scripted error response in Google's error shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// HTTP status.
    pub status: u16,
    /// `error.errors[0].domain`.
    pub domain: &'static str,
    /// `error.errors[0].reason`.
    pub reason: &'static str,
    /// `error.message`.
    pub message: &'static str,
    /// `Retry-After` header in seconds.
    pub retry_after: Option<u64>,
}

impl Fault {
    /// `429 rateLimitExceeded`.
    pub fn rate_limited(retry_after: Option<u64>) -> Self {
        Self {
            status: 429,
            domain: "usageLimits",
            reason: "rateLimitExceeded",
            message: "Rate Limit Exceeded",
            retry_after,
        }
    }

    /// `403 rateLimitExceeded` (Google uses both 403 and 429 for rate limits).
    pub fn forbidden_rate_limited(retry_after: Option<u64>) -> Self {
        Self {
            status: 403,
            ..Self::rate_limited(retry_after)
        }
    }

    /// `503 backendError`.
    pub fn backend_error() -> Self {
        Self {
            status: 503,
            domain: "global",
            reason: "backendError",
            message: "Backend Error",
            retry_after: None,
        }
    }
}

/// A way one calendar departs from normal behaviour, for [`FakeGoogle::quirk`].
/// Each models a suspected cause of a loop seen live on a subscribed holiday
/// calendar (FIDELITY.md, "Quirks"); none is confirmed against Google yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quirk {
    /// `events.watch` answers `400 pushNotSupportedForRequestedResource`.
    RefuseWatch,
    /// Every `syncToken`, even one issued a moment ago, answers `410 fullSyncRequired`.
    ExpireEverySyncToken,
    /// The last page of a read carries no `nextSyncToken`.
    NoSyncToken,
}

/// A change to the fake's data; also the JSON form of scripted scenarios.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    /// Insert or replace an event; see [`FakeGoogle::put_event`].
    Put {
        /// Calendar id.
        calendar: String,
        /// Event fields as Google would accept them.
        event: Value,
    },
    /// Delete an event; see [`FakeGoogle::delete_event`].
    Delete {
        /// Calendar id.
        calendar: String,
        /// Event id.
        id: String,
    },
    /// Invalidate every sync token of a calendar issued so far (next use gets `410`).
    ExpireSyncTokens {
        /// Calendar id.
        calendar: String,
    },
    /// Add a calendar list entry (`id` required, other fields defaulted).
    AddCalendar {
        /// The entry.
        entry: Value,
    },
    /// Remove a calendar with its events and channels.
    RemoveCalendar {
        /// Calendar id.
        id: String,
    },
}

/// One step of a script: apply `change` `at_ms` after the script starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Offset from the start of the script in milliseconds.
    pub at_ms: u64,
    /// What happens.
    #[serde(flatten)]
    pub change: Change,
}

/// One webhook delivery attempt, in order of sending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// `X-Goog-Channel-ID`.
    pub channel: String,
    /// `X-Goog-Resource-State`: `sync` or `exists`.
    pub state: String,
    /// `X-Goog-Message-Number`.
    pub number: u64,
    /// Callback status, or the transport error.
    pub result: Result<u16, String>,
}

/// The running fake server. Dropping it stops accepting connections and
/// sending notifications; connections a client already holds open
/// (keep-alive) are still answered until the client closes them.
pub struct FakeGoogle {
    inner: Arc<Inner>,
    addr: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for FakeGoogle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

struct Inner {
    config: Config,
    state: Mutex<State>,
    notices: mpsc::UnboundedSender<Notice>,
}

#[derive(Default)]
struct State {
    version: u64,
    primary: String,
    calendars: BTreeMap<String, Calendar>,
    channels: BTreeMap<String, Channel>,
    refresh_tokens: BTreeSet<String>,
    access_tokens: BTreeSet<String>,
    /// Unredeemed authorization codes.
    codes: BTreeMap<String, AuthCode>,
    issued: u64,
    faults: Vec<(Endpoint, Fault, u32)>,
    latency: Duration,
    requests: Vec<String>,
    deliveries: Vec<Delivery>,
}

struct AuthCode {
    client_id: String,
    redirect_uri: String,
    challenge: String,
}

struct Calendar {
    entry: Value,
    events: BTreeMap<String, Ev>,
    /// Sync tokens below this version are expired.
    min_sync: u64,
    quirks: Vec<Quirk>,
    /// Version of the last change (calendar etag, `updated`).
    changed: u64,
    resource_id: String,
}

struct Ev {
    body: Value,
    version: u64,
    /// `[start, end)`; `None` end is unbounded (series). Kept after deletion.
    span: Option<(Timestamp, Option<Timestamp>)>,
}

struct Channel {
    calendar: String,
    address: Uri,
    token: Option<String>,
    resource_id: String,
    expiration: Timestamp,
    number: u64,
}

struct Notice {
    channel: String,
    address: Uri,
    headers: Vec<(&'static str, String)>,
    state: &'static str,
    number: u64,
}

impl FakeGoogle {
    /// Binds `127.0.0.1:0`, seeds the data and starts serving.
    pub async fn start(config: Config) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = State {
            latency: config.latency,
            refresh_tokens: BTreeSet::from([config.refresh_token.clone()]),
            ..State::default()
        };
        seed(&mut state, &config);
        let inner = Arc::new(Inner {
            config,
            state: Mutex::new(state),
            notices: tx,
        });
        let app = Router::new()
            .route("/calendar/v3/users/me/calendarList", get(calendar_list))
            .route("/calendar/v3/calendars/{cal}/events", get(events_list))
            .route("/calendar/v3/calendars/{cal}/events/{id}", get(events_get))
            .route("/calendar/v3/calendars/{cal}/events/watch", post(watch))
            .route("/calendar/v3/channels/stop", post(stop))
            .route("/o/oauth2/v2/auth", get(authorize))
            .route("/token", post(token))
            .fallback(fallback)
            .method_not_allowed_fallback(fallback)
            .with_state(inner.clone());
        let server = tokio::spawn(async move {
            // shortcut: serve errors end the task silently; tests then see connection errors.
            let _ = axum::serve(listener, app).await;
        });
        let notifier = tokio::spawn(deliver(inner.clone(), rx));
        Ok(Self {
            inner,
            addr,
            tasks: vec![server, notifier],
        })
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Replacement for `https://www.googleapis.com/calendar/v3`.
    pub fn calendar_api(&self) -> String {
        format!("{}/calendar/v3", self.base_url())
    }

    /// Replacement for `https://oauth2.googleapis.com/token`.
    pub fn token_url(&self) -> String {
        format!("{}/token", self.base_url())
    }

    /// Replacement for `https://accounts.google.com/o/oauth2/v2/auth`.
    pub fn auth_url(&self) -> String {
        format!("{}/o/oauth2/v2/auth", self.base_url())
    }

    /// The currently valid refresh token (changes when rotation is on).
    pub fn refresh_token(&self) -> String {
        let s = self.inner.lock();
        s.refresh_tokens.iter().next().cloned().unwrap_or_default()
    }

    /// Issues a valid access token without going through `/token`.
    pub fn access_token(&self) -> String {
        self.inner.lock().issue_access()
    }

    /// Id of the primary calendar (also reachable as `primary`).
    pub fn primary(&self) -> String {
        self.inner.lock().primary.clone()
    }

    /// Calendar ids in list order (primary first).
    pub fn calendars(&self) -> Vec<String> {
        let s = self.inner.lock();
        let mut ids: Vec<_> = s.calendars.keys().cloned().collect();
        ids.sort_by_key(|id| *id != s.primary);
        ids
    }

    /// Inserts or replaces an event and notifies the calendar's channels.
    ///
    /// The server sets `kind`, `etag`, `created`, `updated`, `iCalUID` and the
    /// usual defaults; `id` is generated when missing. A body with
    /// `"status": "cancelled"` is stored as Google's minimal tombstone (a
    /// cancelled instance needs `recurringEventId` and `originalStartTime`).
    /// Returns the event id.
    ///
    /// # Panics
    /// If the calendar does not exist or `event` is not an object.
    pub fn put_event(&self, calendar: &str, event: Value) -> String {
        self.inner.put_event(calendar, event)
    }

    /// Deletes an event (it stays as a `cancelled` tombstone for deltas and
    /// `showDeleted`) and notifies. Deleting a deleted event does nothing.
    ///
    /// # Panics
    /// If the calendar or the event does not exist.
    pub fn delete_event(&self, calendar: &str, id: &str) {
        self.inner.delete_event(calendar, id);
    }

    /// Applies one change.
    pub fn apply(&self, change: &Change) {
        self.inner.apply(change);
    }

    /// Runs a script in the background; the handle finishes after the last step
    /// (and returns `Err` if a step panicked).
    pub fn run_script(&self, steps: Vec<Step>) -> JoinHandle<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            for step in steps {
                tokio::time::sleep_until(start + Duration::from_millis(step.at_ms)).await;
                inner.apply(&step.change);
            }
        })
    }

    /// Makes the next `times` requests to `endpoint` fail with `fault`.
    /// Faults are checked in the order they were added.
    pub fn fail(&self, endpoint: Endpoint, fault: Fault, times: u32) {
        self.inner.lock().faults.push((endpoint, fault, times));
    }

    /// Makes `calendar` behave as `quirk` from now on. Unknown calendars are ignored.
    pub fn quirk(&self, calendar: &str, quirk: Quirk) {
        let mut s = self.inner.lock();
        let id = s.resolve(calendar);
        if let Some(c) = s.calendars.get_mut(&id) {
            c.quirks.push(quirk);
        }
    }

    /// Changes the per-response delay.
    pub fn set_latency(&self, latency: Duration) {
        self.inner.lock().latency = latency;
    }

    /// Revokes a refresh token: `/token` answers `invalid_grant`.
    pub fn revoke_refresh_token(&self, token: &str) {
        self.inner.lock().refresh_tokens.remove(token);
    }

    /// Invalidates every issued access token: API calls answer `401`.
    pub fn expire_access_tokens(&self) {
        self.inner.lock().access_tokens.clear();
    }

    /// Every event of a calendar by id, tombstones included, exactly as served.
    /// This is the truth crash tests compare against.
    ///
    /// # Panics
    /// If the calendar does not exist.
    pub fn snapshot(&self, calendar: &str) -> BTreeMap<String, Value> {
        let s = self.inner.lock();
        let cal = s.calendar(calendar).expect("unknown calendar");
        cal.events
            .iter()
            .map(|(id, e)| (id.clone(), e.body.clone()))
            .collect()
    }

    /// Requests received so far, as `METHOD /path?query`.
    pub fn requests(&self) -> Vec<String> {
        self.inner.lock().requests.clone()
    }

    /// Webhook deliveries attempted so far, in sending order.
    pub fn deliveries(&self) -> Vec<Delivery> {
        self.inner.lock().deliveries.clone()
    }

    /// Ids of the live (neither stopped nor expired) channels.
    pub fn channels(&self) -> Vec<String> {
        let mut s = self.inner.lock();
        s.prune_channels();
        s.channels.keys().cloned().collect()
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Every mutation completes under the lock; a panicking test leaves it consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self, version: u64) -> Timestamp {
        logical(self.config.epoch, version)
    }

    fn put_event(&self, calendar: &str, event: Value) -> String {
        let mut s = self.lock();
        let id = s.put(self.config.epoch, calendar, event);
        let cal = s.resolve(calendar);
        self.notify(&mut s, &cal);
        id
    }

    fn delete_event(&self, calendar: &str, id: &str) {
        let mut s = self.lock();
        let cal = s.resolve(calendar);
        let epoch = self.config.epoch;
        let ev = s
            .calendars
            .get(&cal)
            .expect("unknown calendar")
            .events
            .get(id)
            .expect("unknown event");
        if is_cancelled(&ev.body) {
            return;
        }
        let mut body = ev.body.clone();
        body["status"] = json!("cancelled");
        s.put(epoch, &cal, body);
        self.notify(&mut s, &cal);
    }

    fn apply(&self, change: &Change) {
        match change {
            Change::Put { calendar, event } => {
                self.put_event(calendar, event.clone());
            }
            Change::Delete { calendar, id } => self.delete_event(calendar, id),
            Change::ExpireSyncTokens { calendar } => {
                let mut s = self.lock();
                s.version += 1;
                let v = s.version;
                let cal = s.resolve(calendar);
                s.calendars
                    .get_mut(&cal)
                    .expect("unknown calendar")
                    .min_sync = v;
            }
            Change::AddCalendar { entry } => {
                let id = entry["id"].as_str().expect("calendar entry needs an id");
                let mut s = self.lock();
                let n = s.calendars.len();
                s.add_calendar(id, entry.clone(), n);
            }
            Change::RemoveCalendar { id } => {
                let mut s = self.lock();
                let cal = s.resolve(id);
                s.calendars.remove(&cal);
                s.channels.retain(|_, c| c.calendar != cal);
            }
        }
    }

    /// Queues an `exists` notification for every live channel on `cal`.
    fn notify(&self, s: &mut State, cal: &str) {
        s.prune_channels();
        for (id, ch) in s.channels.iter_mut().filter(|(_, c)| c.calendar == cal) {
            ch.number += 1;
            let _ = self.notices.send(notice(id, ch, "exists"));
        }
    }

    /// Common prologue of every route: log, latency, scripted fault, auth.
    async fn enter(
        &self,
        endpoint: Endpoint,
        req: String,
        headers: &HeaderMap,
    ) -> Result<(), Response> {
        let latency = {
            let mut s = self.lock();
            s.requests.push(req);
            s.latency
        };
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        let mut s = self.lock();
        if let Some(f) = s
            .faults
            .iter_mut()
            .find(|(e, _, n)| *n > 0 && (*e == Endpoint::Any || *e == endpoint))
        {
            f.2 -= 1;
            let fault = f.1.clone();
            s.faults.retain(|(_, _, n)| *n > 0);
            let status =
                StatusCode::from_u16(fault.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut resp = if endpoint == Endpoint::Token {
                oauth_error(status, fault.reason, fault.message)
            } else {
                error(status, fault.domain, fault.reason, fault.message)
            };
            if let Some(secs) = fault.retry_after {
                resp.headers_mut().insert(
                    header::RETRY_AFTER,
                    secs.to_string().parse().expect("digits"),
                );
            }
            return Err(resp);
        }
        if endpoint == Endpoint::Token {
            return Ok(());
        }
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match bearer {
            Some(t) if s.access_tokens.contains(t) => Ok(()),
            _ => Err(error(
                StatusCode::UNAUTHORIZED,
                "global",
                "authError",
                "Invalid Credentials",
            )),
        }
    }
}

impl State {
    /// Resolves the `primary` alias.
    fn resolve(&self, cal: &str) -> String {
        if cal == "primary" {
            self.primary.clone()
        } else {
            cal.to_owned()
        }
    }

    /// Google forgets expired channels: their ids are free again.
    fn prune_channels(&mut self) {
        let now = Timestamp::now();
        self.channels.retain(|_, c| c.expiration > now);
    }

    fn calendar(&self, cal: &str) -> Option<&Calendar> {
        self.calendars.get(&self.resolve(cal))
    }

    fn issue_access(&mut self) -> String {
        self.issued += 1;
        let t = format!("ya29.fake-access-{}", self.issued);
        self.access_tokens.insert(t.clone());
        t
    }

    fn add_calendar(&mut self, id: &str, mut entry: Value, n: usize) {
        self.version += 1;
        let defaults = json!({
            "kind": "calendar#calendarListEntry",
            "etag": etag(self.version),
            "id": id,
            "summary": id,
            "timeZone": "UTC",
            "colorId": "14",
            "backgroundColor": "#9fe1e7",
            "foregroundColor": "#000000",
            "selected": true,
            "accessRole": "owner",
            "defaultReminders": [],
            "conferenceProperties": { "allowedConferenceSolutionTypes": ["hangoutsMeet"] },
        });
        merge_defaults(&mut entry, defaults);
        self.calendars.insert(
            id.to_owned(),
            Calendar {
                entry,
                events: BTreeMap::new(),
                min_sync: 0,
                quirks: Vec::new(),
                changed: self.version,
                resource_id: format!("fake-resource-{n}"),
            },
        );
    }

    /// Stamps and stores an event; returns its id.
    fn put(&mut self, epoch: Timestamp, calendar: &str, mut event: Value) -> String {
        assert!(event.is_object(), "event must be a JSON object");
        let cal_id = self.resolve(calendar);
        self.version += 1;
        let v = self.version;
        let primary = self.primary.clone();
        let cal = self.calendars.get_mut(&cal_id).expect("unknown calendar");
        let id = match event["id"].as_str() {
            Some(id) => id.to_owned(),
            None => b32hex(v.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
        };
        let old = cal.events.get(&id);
        let created = old
            .and_then(|e| e.body.get("created").cloned())
            .unwrap_or_else(|| json!(logical(epoch, v).to_string()));
        let mut span = event_span(&event);
        if is_cancelled(&event) {
            // Google keeps only identity for deleted events and cancelled instances.
            span = span.or_else(|| old.and_then(|e| e.span));
            let mut t = json!({ "kind": "calendar#event", "etag": etag(v), "id": id, "status": "cancelled" });
            for k in ["recurringEventId", "originalStartTime"] {
                if let Some(x) = event.get(k) {
                    t[k] = x.clone();
                }
            }
            event = t;
        } else {
            assert!(
                when(&event["start"]).is_some(),
                "event {id}: start needs a date, a dateTime with an offset, or a dateTime \
                 with a known timeZone; got {}",
                event["start"]
            );
            let uid = event["recurringEventId"].as_str().unwrap_or(&id).to_owned();
            event["kind"] = json!("calendar#event");
            event["etag"] = json!(etag(v));
            event["id"] = json!(id);
            event["created"] = created;
            event["updated"] = json!(logical(epoch, v).to_string());
            merge_defaults(
                &mut event,
                json!({
                    "status": "confirmed",
                    "htmlLink": format!("https://www.google.com/calendar/event?eid={id}"),
                    "iCalUID": format!("{uid}@google.com"),
                    "sequence": 0,
                    "creator": { "email": primary, "self": true },
                    "organizer": { "email": primary, "self": true },
                    "reminders": { "useDefault": true },
                    "eventType": "default",
                }),
            );
        }
        cal.events.insert(
            id.clone(),
            Ev {
                body: event,
                version: v,
                span,
            },
        );
        cal.changed = v;
        id
    }
}

fn is_cancelled(body: &Value) -> bool {
    body["status"] == "cancelled"
}

/// Adds the fields of `defaults` that `v` lacks.
fn merge_defaults(v: &mut Value, defaults: Value) {
    if let (Some(v), Value::Object(d)) = (v.as_object_mut(), defaults) {
        for (k, x) in d {
            v.entry(k).or_insert(x);
        }
    }
}

fn logical(epoch: Timestamp, version: u64) -> Timestamp {
    epoch + SignedDuration::from_millis(i64::try_from(version).unwrap_or(i64::MAX))
}

/// Google's etag shape: a quoted decimal number.
fn etag(version: u64) -> String {
    format!("\"{}\"", 3_500_000_000_000_000u64 + version)
}

/// Lowercase base32hex (RFC 4648), as Google event ids use.
fn b32hex(mut n: u64) -> String {
    const A: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";
    let mut s = Vec::with_capacity(13);
    for _ in 0..13 {
        s.push(A[(n & 31) as usize]);
        n >>= 5;
    }
    String::from_utf8(s).expect("ascii")
}

/// The instant of an event time (`start`, `end`, `originalStartTime`). A
/// `dateTime` without an offset is local time in its `timeZone`; a `date` is
/// read as UTC midnight (a documented deviation).
fn when(v: &Value) -> Option<Timestamp> {
    if let Some(s) = v["dateTime"].as_str() {
        if let Ok(t) = s.parse() {
            return Some(t);
        }
        let local: jiff::civil::DateTime = s.parse().ok()?;
        let tz = TimeZone::get(v["timeZone"].as_str()?).ok()?;
        return local.to_zoned(tz).ok().map(|z| z.timestamp());
    }
    let d: Date = v["date"].as_str()?.parse().ok()?;
    d.to_zoned(TimeZone::UTC).ok().map(|z| z.timestamp())
}

/// `start`/`end` of an event as an interval; a series is unbounded. A
/// cancelled instance spans one second at its `originalStartTime`.
fn event_span(body: &Value) -> Option<(Timestamp, Option<Timestamp>)> {
    if let Some(start) = when(&body["start"]) {
        let end = if body.get("recurrence").is_some() {
            None
        } else {
            Some(when(&body["end"]).unwrap_or(start))
        };
        return Some((start, end));
    }
    let t = when(&body["originalStartTime"])?;
    Some((t, Some(t + SignedDuration::from_secs(1))))
}

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn seed(s: &mut State, config: &Config) {
    let mut rng = Rng(config.seed.seed);
    let zones = ["Europe/Prague", "America/New_York", "UTC"];
    s.primary = "fake.user@example.com".into();
    let primary = s.primary.clone();
    s.add_calendar(
        &primary,
        json!({ "summary": primary, "timeZone": zones[0], "primary": true }),
        0,
    );
    for n in 1..=config.seed.calendars as usize {
        let id = format!("{}@group.calendar.google.com", b32hex(rng.next()));
        let entry = json!({
            "summary": format!("Calendar {n}"),
            "timeZone": zones[n % zones.len()],
            "accessRole": if n % 2 == 0 { "reader" } else { "owner" },
        });
        s.add_calendar(&id, entry, n);
    }
    let epoch = config.epoch;
    let ids: Vec<String> = s.calendars.keys().cloned().collect();
    for cal in ids {
        for i in 0..config.seed.events {
            let quarter = i64::try_from(rng.below(60 * 24 * 4)).unwrap_or(0) - 30 * 24 * 4;
            let start = epoch + SignedDuration::from_mins(15 * quarter);
            let len =
                SignedDuration::from_mins(30 * (1 + i64::try_from(rng.below(6)).unwrap_or(0)));
            let summary = format!("Event {i} ({:04x})", rng.below(0x10000));
            match i % 10 {
                9 => seed_series(s, epoch, &cal, &summary, start, len),
                7 | 8 => {
                    let day = start.to_zoned(TimeZone::UTC).date();
                    let next = day.tomorrow().unwrap_or(day);
                    s.put(
                        epoch,
                        &cal,
                        json!({ "summary": summary, "start": { "date": day.to_string() }, "end": { "date": next.to_string() }, "transparency": "transparent" }),
                    );
                }
                n => {
                    // UTC, Prague and New York offsets; every tenth is local time without one.
                    let zone = ["UTC", "Europe/Prague", "America/New_York"][i as usize % 3];
                    let tz = TimeZone::get(zone).expect("bundled tzdb");
                    let at = |t: Timestamp| {
                        let dt = if n == 6 {
                            t.to_zoned(tz.clone()).datetime().to_string()
                        } else {
                            t.display_with_offset(tz.to_offset(t)).to_string()
                        };
                        json!({ "dateTime": dt, "timeZone": zone })
                    };
                    s.put(
                        epoch,
                        &cal,
                        json!({ "summary": summary, "start": at(start), "end": at(start + len) }),
                    );
                }
            }
        }
    }
}

/// A weekly series of ten with the third instance moved and the fifth cancelled.
fn seed_series(
    s: &mut State,
    epoch: Timestamp,
    cal: &str,
    summary: &str,
    start: Timestamp,
    len: SignedDuration,
) {
    let dt = |t: Timestamp| json!({ "dateTime": t.to_string(), "timeZone": "UTC" });
    let id = s.put(
        epoch,
        cal,
        json!({ "summary": summary, "start": dt(start), "end": dt(start + len), "recurrence": ["RRULE:FREQ=WEEKLY;COUNT=10"] }),
    );
    let week = SignedDuration::from_hours(24 * 7);
    let instance = |k: i32| start + week * k;
    let inst_id = |t: Timestamp| format!("{id}_{}", t.strftime("%Y%m%dT%H%M%SZ"));
    let moved = instance(2);
    let shifted = moved + SignedDuration::from_hours(1);
    s.put(
        epoch,
        cal,
        json!({ "id": inst_id(moved), "summary": summary, "recurringEventId": id, "originalStartTime": dt(moved), "start": dt(shifted), "end": dt(shifted + len) }),
    );
    let gone = instance(4);
    s.put(
        epoch,
        cal,
        json!({ "id": inst_id(gone), "status": "cancelled", "recurringEventId": id, "originalStartTime": dt(gone) }),
    );
}

fn notice(id: &str, ch: &Channel, state: &'static str) -> Notice {
    let mut headers = vec![
        ("x-goog-channel-id", id.to_owned()),
        ("x-goog-message-number", ch.number.to_string()),
        ("x-goog-resource-id", ch.resource_id.clone()),
        ("x-goog-resource-state", state.to_owned()),
        (
            "x-goog-resource-uri",
            format!(
                "https://www.googleapis.com/calendar/v3/calendars/{}/events?alt=json",
                ch.calendar
            ),
        ),
        (
            "x-goog-channel-expiration",
            jiff::fmt::rfc2822::DateTimePrinter::new()
                .timestamp_to_rfc9110_string(&ch.expiration)
                .unwrap_or_default(),
        ),
    ];
    if let Some(t) = &ch.token {
        headers.push(("x-goog-channel-token", t.clone()));
    }
    Notice {
        channel: id.to_owned(),
        address: ch.address.clone(),
        headers,
        state,
        number: ch.number,
    }
}

/// Sends notifications one at a time, in queue order.
async fn deliver(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Notice>) {
    while let Some(n) = rx.recv().await {
        let result = tokio::time::timeout(Duration::from_secs(5), post_empty(&n))
            .await
            .unwrap_or_else(|_| Err("timeout".into()));
        inner.lock().deliveries.push(Delivery {
            channel: n.channel,
            state: n.state.to_owned(),
            number: n.number,
            result,
        });
    }
}

/// `POST` with an empty body over plain HTTP/1.1.
async fn post_empty(n: &Notice) -> Result<u16, String> {
    if n.address.scheme_str() != Some("http") {
        return Err("the fake delivers only to http:// callbacks".into());
    }
    let authority = n
        .address
        .authority()
        .ok_or("callback without host")?
        .clone();
    // Hermetic: never reach a host outside this machine, whatever a test registers.
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return Err(format!(
            "the fake delivers only to loopback callbacks, not {host}"
        ));
    }
    let port = authority.port_u16().unwrap_or(80);
    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| e.to_string())?;
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    let path = n.address.path_and_query().map_or("/", |p| p.as_str());
    let mut req = Request::post(path)
        .header(header::HOST, authority.as_str())
        .header(header::CONTENT_LENGTH, "0");
    for (k, v) in &n.headers {
        req = req.header(*k, v);
    }
    let req = req.body(Body::empty()).map_err(|e| e.to_string())?;
    let resp = send.send_request(req).await.map_err(|e| e.to_string())?;
    Ok(resp.status().as_u16())
}

/// Google's JSON error shape.
fn error(status: StatusCode, domain: &str, reason: &str, message: &str) -> Response {
    let body = json!({ "error": {
        "errors": [{ "domain": domain, "reason": reason, "message": message }],
        "code": status.as_u16(),
        "message": message,
    }});
    (status, axum::Json(body)).into_response()
}

/// The OAuth 2.0 error shape (RFC 6749 section 5.2) of the token endpoint.
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    let body = json!({ "error": error, "error_description": description });
    (status, axum::Json(body)).into_response()
}

/// Unknown paths and wrong methods: logged, then Google's JSON `404 notFound`.
async fn fallback(AxState(g): St, method: axum::http::Method, uri: Uri) -> Response {
    g.lock().requests.push(req_line(method.as_str(), &uri));
    not_found()
}

fn bad_request(message: &str) -> Response {
    error(StatusCode::BAD_REQUEST, "global", "badRequest", message)
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "global", "notFound", "Not Found")
}

fn ok(v: Value) -> Response {
    axum::Json(v).into_response()
}

type Params = Query<Vec<(String, String)>>;
type St = AxState<Arc<Inner>>;

/// Query parameters as a map; repeated or unsupported parameters are a `400`
/// so a client cannot silently rely on behaviour the fake does not model.
fn params(
    q: Vec<(String, String)>,
    supported: &[&str],
) -> Result<BTreeMap<String, String>, Response> {
    const IGNORED: &[&str] = &["alt", "prettyPrint", "fields", "quotaUser", "timeZone"];
    let mut out = BTreeMap::new();
    for (k, v) in q {
        if IGNORED.contains(&k.as_str()) {
            continue;
        }
        if !supported.contains(&k.as_str()) {
            return Err(bad_request(&format!(
                "parameter {k} is not supported by the fake"
            )));
        }
        if out.insert(k.clone(), v).is_some() {
            return Err(bad_request(&format!("repeated parameter {k}")));
        }
    }
    Ok(out)
}

fn max_results(p: &BTreeMap<String, String>, default: u32, cap: u32) -> Result<usize, Response> {
    let n = match p.get("maxResults") {
        None => default,
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| bad_request("Invalid maxResults"))?,
    };
    Ok(n.min(cap) as usize)
}

fn flag(p: &BTreeMap<String, String>, k: &str) -> Result<bool, Response> {
    match p.get(k).map(String::as_str) {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(bad_request(&format!("Invalid {k}"))),
    }
}

/// Opaque token: hex of the JSON of `T`.
fn encode<T: Serialize>(t: &T) -> String {
    let bytes = serde_json::to_vec(t).expect("serializable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode<T: for<'de> Deserialize<'de>>(s: &str) -> Option<T> {
    let bytes = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(Serialize, Deserialize)]
struct SyncToken {
    cal: String,
    v: u64,
}

/// Where an `events.list` pass continues. Pages continue after the last id
/// served (keyset), so concurrent changes never shift a page.
#[derive(Serialize, Deserialize)]
struct PageToken {
    cal: String,
    /// `Some((since, until))` for a delta pass; `None` for a full or window read.
    delta: Option<(u64, u64)>,
    /// Version the read started at; its `nextSyncToken`.
    snapshot: u64,
    after: Option<String>,
    time_min: Option<Timestamp>,
    time_max: Option<Timestamp>,
    show_deleted: bool,
    /// Pages with items served so far in this pass.
    served: u32,
    /// The previous response of this pass was an empty page.
    empty: bool,
}

fn req_line(method: &str, uri: &Uri) -> String {
    format!(
        "{method} {}",
        uri.path_and_query().map_or("", |p| p.as_str())
    )
}

async fn calendar_list(AxState(g): St, uri: Uri, h: HeaderMap, Query(q): Params) -> Response {
    if let Err(r) = g
        .enter(Endpoint::CalendarList, req_line("GET", &uri), &h)
        .await
    {
        return r;
    }
    let p = match params(
        q,
        &[
            "maxResults",
            "pageToken",
            "showDeleted",
            "showHidden",
            "minAccessRole",
        ],
    ) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let size = match max_results(&p, 100, 250) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let s = g.lock();
    let start = match p.get("pageToken") {
        None => 0,
        Some(t) => match decode::<usize>(t) {
            Some(n) => n,
            None => return bad_request("Invalid page token"),
        },
    };
    let mut entries: Vec<&Calendar> = s.calendars.values().collect();
    entries.sort_by_key(|c| c.entry["id"] != s.primary.as_str());
    let items: Vec<Value> = entries
        .iter()
        .skip(start)
        .take(size)
        .map(|c| c.entry.clone())
        .collect();
    let mut body = json!({
        "kind": "calendar#calendarList",
        "etag": etag(s.version),
        "items": items,
    });
    if start + size < entries.len() {
        body["nextPageToken"] = json!(encode(&(start + size)));
    }
    ok(body)
}

async fn events_list(
    AxState(g): St,
    Path(cal): Path<String>,
    uri: Uri,
    h: HeaderMap,
    Query(q): Params,
) -> Response {
    if let Err(r) = g
        .enter(Endpoint::EventsList, req_line("GET", &uri), &h)
        .await
    {
        return r;
    }
    events_list_inner(&g, &cal, q).unwrap_or_else(|r| r)
}

fn events_list_inner(g: &Inner, cal: &str, q: Vec<(String, String)>) -> Result<Response, Response> {
    let p = params(
        q,
        &[
            "maxResults",
            "pageToken",
            "syncToken",
            "timeMin",
            "timeMax",
            "showDeleted",
            "singleEvents",
        ],
    )?;
    if flag(&p, "singleEvents")? {
        return Err(bad_request(
            "singleEvents=true is not supported by the fake",
        ));
    }
    let size = max_results(&p, DEFAULT_EVENTS_PAGE, MAX_EVENTS_PAGE)?
        .min(g.config.max_page_size.max(1) as usize);
    let s = g.lock();
    let cal_id = s.resolve(cal);
    let c = s.calendar(&cal_id).ok_or_else(not_found)?;
    let time = |k: &str| -> Result<Option<Timestamp>, Response> {
        p.get(k)
            .map(|v| v.parse().map_err(|_| bad_request(&format!("Invalid {k}"))))
            .transpose()
    };
    let gone = || {
        error(
            StatusCode::GONE,
            "global",
            "fullSyncRequired",
            "Sync token is no longer valid, a full sync is required.",
        )
    };
    let pos = match (p.get("pageToken"), p.get("syncToken")) {
        // Continuation: the token carries the query; other parameters are ignored.
        (Some(t), _) => {
            let pos: PageToken = decode(t)
                .filter(|t: &PageToken| t.cal == cal_id)
                .ok_or_else(|| bad_request("Invalid page token"))?;
            let stale =
                |since| since < c.min_sync || c.quirks.contains(&Quirk::ExpireEverySyncToken);
            if pos.delta.is_some_and(|(since, _)| stale(since)) {
                return Err(gone());
            }
            pos
        }
        (None, Some(t)) => {
            if ["timeMin", "timeMax"].iter().any(|k| p.contains_key(*k)) {
                return Err(bad_request(
                    "syncToken cannot be combined with timeMin or timeMax",
                ));
            }
            let tok: SyncToken = decode(t)
                .filter(|t: &SyncToken| t.cal == cal_id && t.v >= c.min_sync)
                .filter(|_| !c.quirks.contains(&Quirk::ExpireEverySyncToken))
                .ok_or_else(gone)?;
            PageToken {
                cal: cal_id.clone(),
                delta: Some((tok.v, s.version)),
                snapshot: s.version,
                after: None,
                time_min: None,
                time_max: None,
                show_deleted: true,
                served: 0,
                empty: false,
            }
        }
        (None, None) => {
            let (time_min, time_max) = (time("timeMin")?, time("timeMax")?);
            if let (Some(a), Some(b)) = (time_min, time_max)
                && b <= a
            {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "calendar",
                    "timeRangeEmpty",
                    "The specified time range is empty.",
                ));
            }
            PageToken {
                cal: cal_id.clone(),
                delta: None,
                snapshot: s.version,
                after: None,
                time_min,
                time_max,
                show_deleted: flag(&p, "showDeleted")?,
                served: 0,
                empty: false,
            }
        }
    };
    let mut body = json!({
        "kind": "calendar#events",
        "etag": etag(c.changed),
        "summary": c.entry["summary"],
        "description": "",
        "updated": g.now(c.changed).to_string(),
        "timeZone": c.entry["timeZone"],
        "accessRole": c.entry["accessRole"],
        "defaultReminders": c.entry["defaultReminders"],
        "items": [],
    });
    let every = g.config.empty_page_every;
    let empty_now = !pos.empty
        && if pos.served == 0 {
            g.config.empty_first_page
        } else {
            every > 0 && pos.served.is_multiple_of(every)
        };
    if empty_now {
        body["nextPageToken"] = json!(encode(&PageToken { empty: true, ..pos }));
        return Ok(ok(body));
    }
    let after = pos
        .after
        .as_deref()
        .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
    let mut rest = c
        .events
        .range::<str, _>((after, std::ops::Bound::Unbounded))
        .filter(|(_, e)| match pos.delta {
            // A key changed again after `until` drops out; the next delta from `until` has it.
            Some((since, until)) => e.version > since && e.version <= until,
            None => {
                // Cancelled instances of a series are listed even without showDeleted.
                let visible = pos.show_deleted
                    || !is_cancelled(&e.body)
                    || e.body.get("recurringEventId").is_some();
                visible && overlaps(e.span, pos.time_min, pos.time_max)
            }
        });
    let page: Vec<(&String, &Ev)> = rest.by_ref().take(size).collect();
    let more = rest.next().is_some();
    body["items"] = page.iter().map(|(_, e)| e.body.clone()).collect();
    if more {
        let next = PageToken {
            after: page.last().map(|(id, _)| (*id).clone()),
            served: pos.served + 1,
            empty: false,
            ..pos
        };
        body["nextPageToken"] = json!(encode(&next));
    } else if !c.quirks.contains(&Quirk::NoSyncToken) {
        let v = pos.delta.map_or(pos.snapshot, |(_, until)| until);
        body["nextSyncToken"] = json!(encode(&SyncToken { cal: cal_id, v }));
    }
    Ok(ok(body))
}

/// Google: `timeMin` bounds the end (exclusive), `timeMax` the start (exclusive).
fn overlaps(
    span: Option<(Timestamp, Option<Timestamp>)>,
    min: Option<Timestamp>,
    max: Option<Timestamp>,
) -> bool {
    if min.is_none() && max.is_none() {
        return true;
    }
    let Some((start, end)) = span else {
        return false;
    };
    min.is_none_or(|m| end.is_none_or(|e| e > m)) && max.is_none_or(|m| start < m)
}

async fn events_get(
    AxState(g): St,
    Path((cal, id)): Path<(String, String)>,
    uri: Uri,
    h: HeaderMap,
) -> Response {
    if let Err(r) = g
        .enter(Endpoint::EventsGet, req_line("GET", &uri), &h)
        .await
    {
        return r;
    }
    let s = g.lock();
    match s.calendar(&cal).and_then(|c| c.events.get(&id)) {
        Some(e) => ok(e.body.clone()),
        None => not_found(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatchRequest {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    address: Option<String>,
    token: Option<String>,
    /// Milliseconds since the epoch; Google sends it as a string.
    expiration: Option<Value>,
    params: Option<BTreeMap<String, String>>,
}

async fn watch(
    AxState(g): St,
    Path(cal): Path<String>,
    uri: Uri,
    h: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(r) = g.enter(Endpoint::Watch, req_line("POST", &uri), &h).await {
        return r;
    }
    let Ok(w) = serde_json::from_slice::<WatchRequest>(&body) else {
        return bad_request("Parse Error");
    };
    let (Some(id), Some(address)) = (w.id, w.address) else {
        return error(StatusCode::BAD_REQUEST, "global", "required", "Required");
    };
    if !matches!(w.kind.as_deref(), Some("web_hook" | "webhook")) {
        return bad_request("Invalid channel type");
    }
    let Ok(address) = address.parse::<Uri>() else {
        return bad_request("Invalid address");
    };
    let requested = match w.expiration.as_ref().map(millis) {
        None => None,
        Some(Some(t)) => Some(t),
        Some(None) => return bad_request("Invalid expiration"),
    };
    let ttl = match w.params.as_ref().and_then(|p| p.get("ttl")) {
        None => g.config.default_watch_ttl,
        Some(v) => match v.parse() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => return bad_request("Invalid ttl"),
        },
    };
    let now = Timestamp::now();
    let cap = |d: Duration| now + SignedDuration::try_from(d).unwrap_or(SignedDuration::MAX);
    let mut expiration = cap(ttl.min(g.config.max_watch_ttl));
    if let Some(t) = requested {
        expiration = expiration.min(t);
    }
    let mut s = g.lock();
    s.prune_channels();
    let cal_id = s.resolve(&cal);
    let Some(c) = s.calendars.get(&cal_id) else {
        return not_found();
    };
    if c.quirks.contains(&Quirk::RefuseWatch) {
        return error(
            StatusCode::BAD_REQUEST,
            "push",
            "pushNotSupportedForRequestedResource",
            "Push notifications are not supported by this resource.",
        );
    }
    if s.channels.contains_key(&id) {
        return bad_request("Channel id not unique");
    }
    let ch = Channel {
        calendar: cal_id.clone(),
        address,
        token: w.token.clone(),
        resource_id: c.resource_id.clone(),
        expiration,
        number: 1,
    };
    let mut resp = json!({
        "kind": "api#channel",
        "id": id,
        "resourceId": ch.resource_id,
        "resourceUri": format!("https://www.googleapis.com/calendar/v3/calendars/{cal_id}/events?alt=json"),
        "expiration": expiration.as_millisecond().to_string(),
    });
    if let Some(t) = &w.token {
        resp["token"] = json!(t);
    }
    let _ = g.notices.send(notice(&id, &ch, "sync"));
    s.channels.insert(id, ch);
    ok(resp)
}

fn millis(v: &Value) -> Option<Timestamp> {
    let ms = match v {
        Value::String(s) => s.parse().ok()?,
        v => v.as_i64()?,
    };
    Timestamp::from_millisecond(ms).ok()
}

async fn stop(AxState(g): St, uri: Uri, h: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Err(r) = g.enter(Endpoint::Stop, req_line("POST", &uri), &h).await {
        return r;
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return bad_request("Parse Error");
    };
    let (Some(id), Some(res)) = (v["id"].as_str(), v["resourceId"].as_str()) else {
        return error(StatusCode::BAD_REQUEST, "global", "required", "Required");
    };
    let mut s = g.lock();
    s.prune_channels();
    if s.channels.get(id).is_none_or(|c| c.resource_id != res) {
        return error(
            StatusCode::NOT_FOUND,
            "global",
            "notFound",
            &format!("Channel '{id}' not found for project 'fake'"),
        );
    }
    s.channels.remove(id);
    StatusCode::NO_CONTENT.into_response()
}

/// The user's browser at the consent screen: consent is granted at once and
/// the browser is sent back to `redirect_uri` with a one-time code.
async fn authorize(
    AxState(g): St,
    uri: Uri,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    let mut s = g.lock();
    s.requests.push(req_line("GET", &uri));
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    if get("response_type") != "code"
        || get("client_id").is_empty()
        || get("redirect_uri").is_empty()
        || get("code_challenge_method") != "S256"
        || get("code_challenge").is_empty()
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Missing or invalid parameter",
        );
    }
    s.issued += 1;
    let code = format!("4/fake-code-{}", s.issued);
    s.codes.insert(
        code.clone(),
        AuthCode {
            client_id: get("client_id").into(),
            redirect_uri: get("redirect_uri").into(),
            challenge: get("code_challenge").into(),
        },
    );
    let mut to = format!("{}?code={}", get("redirect_uri"), pct(&code));
    if let Some(state) = q.get("state") {
        to += &format!("&state={}", pct(state));
    }
    (StatusCode::FOUND, [(header::LOCATION, to)]).into_response()
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn pct(v: &str) -> String {
    v.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn token(
    AxState(g): St,
    uri: Uri,
    h: HeaderMap,
    Form(f): Form<BTreeMap<String, String>>,
) -> Response {
    if let Err(r) = g.enter(Endpoint::Token, req_line("POST", &uri), &h).await {
        return r;
    }
    let oauth_error = |error: &str, description: &str| {
        self::oauth_error(StatusCode::BAD_REQUEST, error, description)
    };
    match f.get("grant_type").map(String::as_str) {
        Some("refresh_token") => {}
        Some("authorization_code") => return redeem(&g, &f),
        other => {
            return oauth_error(
                "unsupported_grant_type",
                &format!("Invalid grant_type: {}", other.unwrap_or("")),
            );
        }
    }
    let Some(refresh) = f.get("refresh_token") else {
        return oauth_error(
            "invalid_request",
            "Missing required parameter: refresh_token",
        );
    };
    let mut s = g.lock();
    if !s.refresh_tokens.contains(refresh) {
        return oauth_error("invalid_grant", "Token has been expired or revoked.");
    }
    let access = s.issue_access();
    let mut body = json!({
        "access_token": access,
        "expires_in": 3599,
        "scope": "https://www.googleapis.com/auth/calendar.readonly",
        "token_type": "Bearer",
    });
    if g.config.rotate_refresh_tokens {
        s.refresh_tokens.remove(refresh);
        let next = format!("1//fake-refresh-{}", s.issued);
        s.refresh_tokens.insert(next.clone());
        body["refresh_token"] = json!(next);
    }
    ok(body)
}

/// `grant_type=authorization_code` with PKCE (RFC 7636, `S256` only).
fn redeem(g: &Inner, f: &BTreeMap<String, String>) -> Response {
    let bad =
        |error: &str, description: &str| oauth_error(StatusCode::BAD_REQUEST, error, description);
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
    let mut s = g.lock();
    // A code is spent by its first redemption, successful or not.
    let Some(code) = s.codes.remove(get("code")) else {
        return bad("invalid_grant", "Malformed auth code.");
    };
    if code.client_id != get("client_id") {
        return bad("invalid_client", "The OAuth client was not found.");
    }
    if code.redirect_uri != get("redirect_uri") {
        return bad("redirect_uri_mismatch", "Bad Request");
    }
    let verifier = get("code_verifier");
    if verifier.is_empty() {
        return bad("invalid_grant", "Missing code verifier.");
    }
    if b64(&Sha256::digest(verifier.as_bytes())) != code.challenge {
        return bad("invalid_grant", "Invalid code verifier.");
    }
    let access = s.issue_access();
    let refresh = s.refresh_tokens.iter().next().cloned().unwrap_or_default();
    let now = Timestamp::now().as_second();
    let claims = json!({
        "iss": "https://accounts.google.com",
        "aud": code.client_id,
        "sub": g.config.subject,
        "iat": now,
        "exp": now + 3600,
    });
    // Unsigned stand-in: a client may skip the signature check on an id_token it
    // got straight from the token endpoint over TLS (OpenID Connect Core 3.1.3.7).
    let id_token = format!(
        "{}.{}.{}",
        b64(br#"{"alg":"RS256","kid":"fake","typ":"JWT"}"#),
        b64(claims.to_string().as_bytes()),
        b64(b"fake-signature"),
    );
    ok(json!({
        "access_token": access,
        "expires_in": 3599,
        "refresh_token": refresh,
        "scope": "https://www.googleapis.com/auth/calendar.readonly",
        "token_type": "Bearer",
        "id_token": id_token,
    }))
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}
