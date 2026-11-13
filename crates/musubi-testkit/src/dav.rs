//! Hermetic in-process fake CalDAV/CardDAV server (plan, chapter 8.2) for the
//! DAV adapter (K4, K7) and the conformance runner. Deviations from RFC 4918,
//! 4791, 6352, 6578 and from real servers are in
//! `crates/musubi-testkit/FIDELITY.md`.
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! use musubi_testkit::dav::{Config, FakeDav, Kind};
//! let dav = FakeDav::start(Config::default()).await?;
//! let cal = dav.add_collection(Kind::Calendar(&["VEVENT", "VTODO"]), "work", "Work");
//! dav.put(&cal, "a.ics", "BEGIN:VCALENDAR\r\n…\r\nEND:VCALENDAR\r\n");
//! let server_url = dav.server_url(); // http://127.0.0.1:<port>/ ; user "alice", password "secret"
//! # Ok(()) }
//! ```
//!
//! Every [`Host`] is its own loopback listener; all of them share one state.
//! Resources (paths with the default user `alice`):
//!
//! | Path | Resource |
//! | --- | --- |
//! | `/.well-known/caldav`, `/.well-known/carddav` | `301` to `/`, without authentication |
//! | `/` | root; `D:current-user-principal` |
//! | `/principals/alice/` | principal: `D:principal-URL`, `C:calendar-home-set`, `CR:addressbook-home-set` |
//! | `/calendars/alice/` | calendar home (schedule inbox and outbox are there from the start) |
//! | `/addressbooks/alice/` | address book home |
//! | `{home}{slug}/` | a collection, see [`Kind`] |
//! | `{home}{slug}/{name}` | an object (`GET`) |
//!
//! Methods: `PROPFIND` (Depth 0 and 1; `prop`, `allprop` or an empty body),
//! `REPORT` (`D:sync-collection`, `C:calendar-query`, `C:calendar-multiget`,
//! `CR:addressbook-multiget`), `GET`, `HEAD`, `OPTIONS`. Anything else is `405`.
//! Every request except well-known needs `Authorization: Basic` with the
//! configured user and password, else `401` with `WWW-Authenticate: Basic`.
//!
//! Hrefs in responses are absolute paths. A home set on another host than the
//! principal (profile `icloud`: the partition hosts) is an absolute URL.
//! Must run inside a tokio runtime.

#![allow(
    clippy::result_large_err,
    reason = "handlers short-circuit with the HTTP error response itself; size is irrelevant in a test fake"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request as AxRequest, State as AxState};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Span, Zoned};
use quick_xml::NsReader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use tokio::task::JoinHandle;

/// `DAV:` namespace.
pub const NS_DAV: &str = "DAV:";
/// CalDAV namespace (RFC 4791).
pub const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
/// CardDAV namespace (RFC 6352).
pub const NS_CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
/// Apple's calendarserver namespace (`getctag`, `subscribed`).
pub const NS_CS: &str = "http://calendarserver.org/ns/";
/// Prefix of every sync token; the collection path and a version follow.
pub const SYNC_TOKEN_PREFIX: &str = "http://musubi.test/sync";

/// Server configuration. All fields have working defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// Basic auth user; also the last segment of the principal and home paths.
    pub user: String,
    /// Basic auth password (change it later with [`FakeDav::set_password`]).
    pub password: String,
    /// Server behaviour.
    pub profile: Profile,
    /// CardDAV on its own host ([`Host::Carddav`]) with its own principal, as
    /// a generic server with separate `caldav_url` and `carddav_url`.
    /// Profile `icloud` is always split.
    pub split: bool,
    /// Delay before every response (benchmark D1 uses 50 ms).
    pub latency: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            user: "alice".into(),
            password: "secret".into(),
            profile: Profile::Generic,
            split: false,
            latency: Duration::ZERO,
        }
    }
}

/// Server behaviour (plan, chapter 5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// A standards-following server (Radicale, Baïkal, Nextcloud).
    Generic,
    /// iCloud: CalDAV and CardDAV on separate entry hosts, home sets on the
    /// partition hosts, no `D:current-user-privilege-set`, vCard 3.0 only, a
    /// notifications collection in the calendar home.
    Icloud,
}

/// One listener of the fake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Host {
    /// CalDAV entry (`caldav.icloud.com`); the only entry when not split.
    Caldav,
    /// CardDAV entry (`contacts.icloud.com`); used when split.
    Carddav,
    /// Calendar home sets in profile `icloud` (`pNN-caldav.icloud.com`).
    CaldavPartition,
    /// Address book home sets in profile `icloud` (`pNN-contacts.icloud.com`).
    CarddavPartition,
    /// A host outside the account: answers `404` to everything and only logs
    /// requests, so a test can prove nothing (no password) was sent there.
    Outside,
}

const HOSTS: [Host; 5] = [
    Host::Caldav,
    Host::Carddav,
    Host::CaldavPartition,
    Host::CarddavPartition,
    Host::Outside,
];

/// Collection type. Calendars and inbox, outbox, notifications and
/// subscriptions live in the calendar home, address books in the address book home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `C:calendar` with these `C:supported-calendar-component-set` entries;
    /// an empty slice leaves the property out (RFC 4791: all components).
    Calendar(&'static [&'static str]),
    /// `CR:addressbook`.
    AddressBook,
    /// `C:schedule-inbox`.
    ScheduleInbox,
    /// `C:schedule-outbox`.
    ScheduleOutbox,
    /// `CS:notification` (iCloud, calendarserver).
    Notifications,
    /// `CS:subscribed` (a calendar subscription).
    Subscribed,
}

/// A way one collection departs from normal behaviour, for [`FakeDav::quirk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quirk {
    /// No `CS:getctag`.
    NoCtag,
    /// No `D:sync-token`, no `sync-collection` in `D:supported-report-set`;
    /// `REPORT sync-collection` answers `403` with `D:supported-report`.
    NoSync,
    /// `D:getetag` is weak (`W/"…"`).
    WeakEtag,
    /// No `D:getetag` (a `404` propstat) and no `ETag` header.
    NoEtag,
    /// `D:current-user-privilege-set` has only `D:read` (profile `generic`).
    ReadOnly,
}

/// A scripted misbehaviour, for [`FakeDav::fail`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// `status` with an empty body and `Retry-After: <s>` when set; `401`
    /// also carries `WWW-Authenticate: Basic`. Covers `401`, `403`, `404`,
    /// `405`, `429`, `500`, `501`, `503`.
    Status {
        /// HTTP status.
        status: u16,
        /// `Retry-After` in seconds.
        retry_after: Option<u64>,
    },
    /// `status` (`403` as Radicale, or `409`) with
    /// `<D:error><D:valid-sync-token/></D:error>`, whatever the token.
    InvalidSyncToken {
        /// HTTP status.
        status: u16,
    },
    /// A `sync-collection` answer keeps at most this many members: `207`
    /// with a `D:response` for the collection href in status `507` and a
    /// token that continues after the last member kept (RFC 6578, 3.6).
    /// Applies only to `REPORT sync-collection`.
    Truncate(usize),
    /// Waits this long, then answers normally.
    Delay(Duration),
    /// `status` (`301`, `302`, `307`, `308`) with `Location` set to the same
    /// path and query on `to`.
    Redirect {
        /// HTTP status.
        status: u16,
        /// Target listener.
        to: Host,
    },
    /// Every object a multiget names changes (new ETag, same body) just
    /// before the answer, as if written between listing and fetching.
    /// Applies only to multiget reports.
    ChangeBeforeMultiget,
    /// Every object a multiget names is deleted just before the answer
    /// (per-href `404`). Applies only to multiget reports.
    DeleteBeforeMultiget,
    /// `207 Multi-Status` with a body that is not well-formed XML.
    Garbage,
}

/// Which requests a [`Fault`] applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Req {
    /// Every request to every host.
    Any,
    /// One method (`PROPFIND`, `REPORT`, `GET`, …).
    Method(&'static str),
    /// One report by root element name (`sync-collection`, `calendar-query`,
    /// `calendar-multiget`, `addressbook-multiget`).
    Report(&'static str),
    /// Every request to exactly this path (a collection path with its
    /// trailing slash; the slash is optional in the request).
    Path(String),
}

/// One logged request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Listener it arrived at.
    pub host: Host,
    /// Method.
    pub method: String,
    /// Path and query.
    pub path: String,
    /// `Depth` header.
    pub depth: Option<String>,
    /// Root element name of a `REPORT` body.
    pub report: Option<String>,
    /// An `Authorization` header was present.
    pub auth: bool,
}

impl Request {
    /// `METHOD`, or `REPORT <root element>`: the key of [`FakeDav::count`].
    pub fn label(&self) -> String {
        match &self.report {
            Some(r) => format!("{} {r}", self.method),
            None => self.method.clone(),
        }
    }
}

/// The running fake server. Dropping it stops accepting connections;
/// connections a client already holds open are answered until it closes them.
pub struct FakeDav {
    inner: Arc<Inner>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for FakeDav {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

struct Inner {
    config: Config,
    bases: BTreeMap<Host, String>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    version: u64,
    password: String,
    latency: Duration,
    collections: BTreeMap<String, Collection>,
    faults: Vec<(Req, Fault, u32)>,
    requests: Vec<Request>,
}

struct Collection {
    kind: Kind,
    name: String,
    quirks: Vec<Quirk>,
    objects: BTreeMap<String, Obj>,
    /// Sync tokens below this version are invalid.
    min_sync: u64,
    /// Version of the last change (the ctag).
    changed: u64,
}

struct Obj {
    /// `None` is a tombstone, kept for `sync-collection`.
    body: Option<String>,
    version: u64,
}

impl Collection {
    fn has(&self, q: Quirk) -> bool {
        self.quirks.contains(&q)
    }

    fn syncs(&self) -> bool {
        matches!(self.kind, Kind::Calendar(_) | Kind::AddressBook) && !self.has(Quirk::NoSync)
    }

    fn etag(&self, o: &Obj) -> Option<String> {
        if self.has(Quirk::NoEtag) {
            None
        } else if self.has(Quirk::WeakEtag) {
            Some(format!("W/\"e{}\"", o.version))
        } else {
            Some(format!("\"e{}\"", o.version))
        }
    }

    fn data_prop(&self) -> (&'static str, &'static str) {
        if self.kind == Kind::AddressBook {
            (NS_CARDDAV, "address-data")
        } else {
            (NS_CALDAV, "calendar-data")
        }
    }
}

impl FakeDav {
    /// Binds one `127.0.0.1:0` listener per [`Host`] and starts serving.
    pub async fn start(config: Config) -> std::io::Result<Self> {
        let mut listeners = Vec::new();
        let mut bases = BTreeMap::new();
        for host in HOSTS {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            bases.insert(host, format!("http://{}", l.local_addr()?));
            listeners.push((host, l));
        }
        let mut state = State {
            password: config.password.clone(),
            latency: config.latency,
            ..State::default()
        };
        let cal_home = format!("/calendars/{}/", config.user);
        let mut seed = vec![
            ("inbox", Kind::ScheduleInbox),
            ("outbox", Kind::ScheduleOutbox),
        ];
        if config.profile == Profile::Icloud {
            seed.push(("notification", Kind::Notifications));
        }
        for (slug, kind) in seed {
            state.add(format!("{cal_home}{slug}/"), kind, slug);
        }
        let inner = Arc::new(Inner {
            config,
            bases,
            state: Mutex::new(state),
        });
        let mut tasks = Vec::new();
        for (host, listener) in listeners {
            let app = Router::new()
                .fallback(handle)
                .with_state((inner.clone(), host));
            tasks.push(tokio::spawn(async move {
                // shortcut: serve errors end the task silently; tests then see connection errors.
                let _ = axum::serve(listener, app).await;
            }));
        }
        Ok(Self { inner, tasks })
    }

    /// `http://127.0.0.1:<port>` of a listener.
    pub fn url(&self, host: Host) -> String {
        self.inner.bases[&host].clone()
    }

    /// The address a user enters: the CalDAV entry with an empty path.
    pub fn server_url(&self) -> String {
        format!("{}/", self.url(Host::Caldav))
    }

    /// `server_url`, or the CardDAV entry when split.
    pub fn carddav_url(&self) -> String {
        if self.inner.split() {
            format!("{}/", self.url(Host::Carddav))
        } else {
            self.server_url()
        }
    }

    /// Principal path (the same on both entries when split).
    pub fn principal(&self) -> String {
        self.inner.principal()
    }

    /// Calendar home path.
    pub fn calendar_home(&self) -> String {
        self.inner.home(true)
    }

    /// Address book home path.
    pub fn addressbook_home(&self) -> String {
        self.inner.home(false)
    }

    /// Creates a collection in its home; returns its path (with a trailing slash).
    ///
    /// # Panics
    /// If it exists.
    pub fn add_collection(&self, kind: Kind, slug: &str, displayname: &str) -> String {
        let path = format!("{}{slug}/", self.inner.home(kind != Kind::AddressBook));
        let mut s = self.inner.lock();
        assert!(!s.collections.contains_key(&path), "{path} exists");
        s.add(path.clone(), kind, displayname);
        path
    }

    /// Removes a collection with its objects.
    pub fn remove_collection(&self, path: &str) {
        self.inner.lock().collections.remove(path);
    }

    /// Creates or replaces an object; returns its path. The body is served
    /// byte for byte.
    ///
    /// # Panics
    /// If the collection does not exist, or (profile `icloud`) a vCard is
    /// not version 3.0.
    pub fn put(&self, collection: &str, name: &str, body: &str) -> String {
        let mut s = self.inner.lock();
        let v = s.bump();
        let c = s
            .collections
            .get_mut(collection)
            .expect("unknown collection");
        if c.kind == Kind::AddressBook && self.inner.config.profile == Profile::Icloud {
            assert!(body.contains("VERSION:3.0"), "iCloud serves vCard 3.0 only");
        }
        c.objects.insert(
            name.to_owned(),
            Obj {
                body: Some(body.to_owned()),
                version: v,
            },
        );
        c.changed = v;
        format!("{collection}{name}")
    }

    /// Deletes an object (a tombstone stays for `sync-collection`). Deleting
    /// a missing object does nothing.
    pub fn delete(&self, path: &str) {
        let mut s = self.inner.lock();
        let (col, name) = split_object(path);
        if s.collections
            .get(col)
            .and_then(|c| c.objects.get(name)?.body.as_ref())
            .is_some()
        {
            s.tombstone(col, name);
        }
    }

    /// Changes `collection`'s behaviour from now on. Unknown collections are ignored.
    pub fn quirk(&self, collection: &str, quirk: Quirk) {
        if let Some(c) = self.inner.lock().collections.get_mut(collection) {
            c.quirks.push(quirk);
        }
    }

    /// Invalidates every sync token of a collection issued so far: the next
    /// `sync-collection` with one answers `403` with `D:valid-sync-token`.
    pub fn expire_sync_tokens(&self, collection: &str) {
        let mut s = self.inner.lock();
        let v = s.version + 1;
        s.version = v;
        if let Some(c) = s.collections.get_mut(collection) {
            c.min_sync = v;
        }
    }

    /// The token `sync-collection` would return now for a full read.
    pub fn sync_token(&self, collection: &str) -> String {
        format!(
            "{SYNC_TOKEN_PREFIX}{collection}{}",
            self.inner.lock().version
        )
    }

    /// The `D:getetag` value served for an object, raw (quotes, `W/`).
    pub fn etag(&self, path: &str) -> Option<String> {
        let s = self.inner.lock();
        let (col, name) = split_object(path);
        let c = s.collections.get(col)?;
        let o = c.objects.get(name).filter(|o| o.body.is_some())?;
        c.etag(o)
    }

    /// Makes the next `times` matching requests misbehave as `fault`. Faults
    /// are checked in the order they were added; [`Fault::Truncate`] and the
    /// multiget faults skip requests they do not apply to.
    pub fn fail(&self, req: Req, fault: Fault, times: u32) {
        self.inner.lock().faults.push((req, fault, times));
    }

    /// Changes the password (the old one then gets `401`).
    pub fn set_password(&self, password: &str) {
        self.inner.lock().password = password.to_owned();
    }

    /// Changes the per-response delay.
    pub fn set_latency(&self, latency: Duration) {
        self.inner.lock().latency = latency;
    }

    /// Requests received so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.inner.lock().requests.clone()
    }

    /// Requests whose method or [`Request::label`] is `label` (`"PROPFIND"`,
    /// `"REPORT"`, `"REPORT calendar-multiget"`).
    pub fn count(&self, label: &str) -> usize {
        let s = self.inner.lock();
        s.requests
            .iter()
            .filter(|r| r.method == label || r.label() == label)
            .count()
    }

    /// Forgets the logged requests.
    pub fn clear_requests(&self) {
        self.inner.lock().requests.clear();
    }
}

impl State {
    fn bump(&mut self) -> u64 {
        self.version += 1;
        self.version
    }

    fn add(&mut self, path: String, kind: Kind, name: &str) {
        let v = self.bump();
        self.collections.insert(
            path,
            Collection {
                kind,
                name: name.to_owned(),
                quirks: Vec::new(),
                objects: BTreeMap::new(),
                min_sync: 0,
                changed: v,
            },
        );
    }

    fn tombstone(&mut self, col: &str, name: &str) {
        let v = self.bump();
        let c = self.collections.get_mut(col).expect("collection");
        c.objects.insert(
            name.to_owned(),
            Obj {
                body: None,
                version: v,
            },
        );
        c.changed = v;
    }

    fn touch(&mut self, col: &str, name: &str) {
        let v = self.bump();
        let c = self.collections.get_mut(col).expect("collection");
        if let Some(o) = c.objects.get_mut(name).filter(|o| o.body.is_some()) {
            o.version = v;
            c.changed = v;
        }
    }

    /// The first fault matching the request, consumed.
    fn fault(&mut self, method: &str, path: &str, report: Option<&str>) -> Option<Fault> {
        let i = self.faults.iter().position(|(req, fault, times)| {
            let hit = match req {
                Req::Any => true,
                Req::Method(m) => *m == method,
                Req::Report(r) => report == Some(r),
                Req::Path(p) => same_path(p, path),
            };
            let applies = match fault {
                Fault::Truncate(_) | Fault::InvalidSyncToken { .. } => {
                    report == Some("sync-collection")
                }
                Fault::ChangeBeforeMultiget | Fault::DeleteBeforeMultiget => {
                    report.is_some_and(|r| r.ends_with("-multiget"))
                }
                _ => true,
            };
            *times > 0 && hit && applies
        })?;
        let (_, fault, times) = &mut self.faults[i];
        *times -= 1;
        Some(fault.clone())
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Every mutation completes under the lock; a panicking test leaves it consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn split(&self) -> bool {
        self.config.split || self.config.profile == Profile::Icloud
    }

    fn principal(&self) -> String {
        format!("/principals/{}/", self.config.user)
    }

    fn home(&self, calendars: bool) -> String {
        let kind = if calendars {
            "calendars"
        } else {
            "addressbooks"
        };
        format!("/{kind}/{}/", self.config.user)
    }

    /// Home set href as served by the principal on `host`: a path, or an
    /// absolute URL on the partition host (profile `icloud`).
    fn home_href(&self, calendars: bool) -> String {
        let path = self.home(calendars);
        if self.config.profile != Profile::Icloud {
            return path;
        }
        let host = if calendars {
            Host::CaldavPartition
        } else {
            Host::CarddavPartition
        };
        format!("{}{path}", self.bases[&host])
    }

    fn authorized(&self, s: &State, headers: &HeaderMap) -> bool {
        let Some(v) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let Some(b64) = v.strip_prefix("Basic ") else {
            return false;
        };
        let Ok(raw) = STANDARD.decode(b64.trim()) else {
            return false;
        };
        raw == format!("{}:{}", self.config.user, s.password).as_bytes()
    }
}

/// One resource of the tree.
#[derive(Clone)]
enum Res {
    Root,
    Principal,
    Home(bool),
    Collection(String),
    Object(String, String),
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn split_object(path: &str) -> (&str, &str) {
    let i = path.rfind('/').map_or(0, |i| i + 1);
    path.split_at(i)
}

fn resolve(inner: &Inner, s: &State, path: &str) -> Option<Res> {
    if path == "/" {
        return Some(Res::Root);
    }
    if same_path(path, &inner.principal()) {
        return Some(Res::Principal);
    }
    for cal in [true, false] {
        if same_path(path, &inner.home(cal)) {
            return Some(Res::Home(cal));
        }
    }
    let as_col = format!("{}/", path.trim_end_matches('/'));
    if s.collections.contains_key(&as_col) {
        return Some(Res::Collection(as_col));
    }
    let (col, name) = split_object(path);
    let c = s.collections.get(col)?;
    c.objects.get(name)?.body.as_ref()?;
    Some(Res::Object(col.to_owned(), name.to_owned()))
}

// ---------------------------------------------------------------------------
// HTTP

async fn handle(AxState((inner, host)): AxState<(Arc<Inner>, Host)>, req: AxRequest) -> Response {
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str().to_owned();
    let path = pct_decode(parts.uri.path());
    let path_q = parts
        .uri
        .path_and_query()
        .map_or("/", |p| p.as_str())
        .to_owned();
    let headers = parts.headers;
    let body = match axum::body::to_bytes(body, 8 << 20).await {
        Ok(b) => b,
        Err(_) => return status(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let doc = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        match std::str::from_utf8(&body)
            .map_err(|e| e.to_string())
            .and_then(parse)
        {
            Ok(d) => Some(d),
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("unreadable XML: {e}")).into_response();
            }
        }
    };
    let report = (method == "REPORT")
        .then(|| doc.as_ref().map(|d| d.name.clone()))
        .flatten();
    let (latency, fault) = {
        let mut s = inner.lock();
        s.requests.push(Request {
            host,
            method: method.clone(),
            path: path_q.clone(),
            depth: headers
                .get("depth")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            report: report.clone(),
            auth: headers.contains_key(header::AUTHORIZATION),
        });
        (s.latency, s.fault(&method, &path, report.as_deref()))
    };
    tokio::time::sleep(latency).await;
    let mut truncate = None;
    let mut before_multiget = None;
    match fault {
        Some(Fault::Status {
            status: code,
            retry_after,
        }) => {
            let mut r = status(code_of(code));
            if let Some(s) = retry_after {
                r.headers_mut().insert(header::RETRY_AFTER, s.into());
            }
            if code == 401 {
                r.headers_mut()
                    .insert(header::WWW_AUTHENTICATE, BASIC.parse().expect("valid"));
            }
            return r;
        }
        Some(Fault::InvalidSyncToken { status: code }) => {
            return dav_error(code_of(code), "valid-sync-token");
        }
        Some(Fault::Redirect { status: code, to }) => {
            let mut r = status(code_of(code));
            let location = format!("{}{path_q}", inner.bases[&to]);
            r.headers_mut()
                .insert(header::LOCATION, location.parse().expect("valid"));
            return r;
        }
        Some(Fault::Garbage) => {
            return xml(
                StatusCode::MULTI_STATUS,
                "<D:multistatus xmlns:D=\"DAV:\"><D:resp".into(),
            );
        }
        Some(Fault::Delay(d)) => tokio::time::sleep(d).await,
        Some(Fault::Truncate(n)) => truncate = Some(n),
        Some(f @ (Fault::ChangeBeforeMultiget | Fault::DeleteBeforeMultiget)) => {
            before_multiget = Some(f)
        }
        None => {}
    }
    if host == Host::Outside {
        return status(StatusCode::NOT_FOUND);
    }
    if path == "/.well-known/caldav" || path == "/.well-known/carddav" {
        let mut r = status(StatusCode::MOVED_PERMANENTLY);
        r.headers_mut()
            .insert(header::LOCATION, "/".parse().expect("valid"));
        return r;
    }
    let mut s = inner.lock();
    if !inner.authorized(&s, &headers) {
        let mut r = status(StatusCode::UNAUTHORIZED);
        r.headers_mut()
            .insert(header::WWW_AUTHENTICATE, BASIC.parse().expect("valid"));
        return r;
    }
    let cx = Cx {
        inner: &inner,
        host,
    };
    match parts.method {
        Method::GET | Method::HEAD => get(&cx, &s, &path, parts.method == Method::HEAD),
        Method::OPTIONS => {
            let mut r = status(StatusCode::OK);
            let h = r.headers_mut();
            h.insert(
                "dav",
                "1, 2, 3, calendar-access, addressbook"
                    .parse()
                    .expect("valid"),
            );
            h.insert(
                header::ALLOW,
                "OPTIONS, GET, HEAD, PROPFIND, REPORT"
                    .parse()
                    .expect("valid"),
            );
            r
        }
        _ if method == "PROPFIND" => {
            let depth = headers
                .get("depth")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("infinity");
            propfind(&cx, &s, &path, depth != "0", doc.as_ref())
        }
        _ if method == "REPORT" => match doc {
            Some(d) => {
                let depth = headers.get("depth").and_then(|v| v.to_str().ok());
                report_(&cx, &mut s, &path, depth, &d, truncate, before_multiget)
            }
            None => (StatusCode::BAD_REQUEST, "REPORT needs a body").into_response(),
        },
        _ => status(StatusCode::METHOD_NOT_ALLOWED),
    }
}

const BASIC: &str = "Basic realm=\"musubi-fake-dav\"";

struct Cx<'a> {
    inner: &'a Inner,
    host: Host,
}

fn code_of(code: u16) -> StatusCode {
    StatusCode::from_u16(code).expect("valid status")
}

fn status(code: StatusCode) -> Response {
    (code, Body::empty()).into_response()
}

fn xml(code: StatusCode, body: String) -> Response {
    let mut r = (code, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/xml; charset=utf-8".parse().expect("valid"),
    );
    r
}

/// `<D:error><D:{condition}/></D:error>` (RFC 4918, 16).
fn dav_error(code: StatusCode, condition: &str) -> Response {
    xml(
        code,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\"><D:{condition}/></D:error>"
        ),
    )
}

fn get(cx: &Cx<'_>, s: &State, path: &str, head: bool) -> Response {
    let Some(Res::Object(col, name)) = resolve(cx.inner, s, path) else {
        return status(if resolve(cx.inner, s, path).is_some() {
            StatusCode::METHOD_NOT_ALLOWED
        } else {
            StatusCode::NOT_FOUND
        });
    };
    let c = &s.collections[&col];
    let o = &c.objects[&name];
    let body = if head {
        String::new()
    } else {
        o.body.clone().unwrap_or_default()
    };
    let mut r = (StatusCode::OK, body).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        content_type(c).parse().expect("valid"),
    );
    if let Some(e) = c.etag(o) {
        h.insert(header::ETAG, e.parse().expect("valid"));
    }
    r
}

fn content_type(c: &Collection) -> &'static str {
    if c.kind == Kind::AddressBook {
        "text/vcard; charset=utf-8"
    } else {
        "text/calendar; charset=utf-8"
    }
}

// ---------------------------------------------------------------------------
// PROPFIND

/// Properties `allprop` returns (where the resource has them).
const ALLPROP: [(&str, &str); 4] = [
    (NS_DAV, "resourcetype"),
    (NS_DAV, "displayname"),
    (NS_DAV, "getetag"),
    (NS_DAV, "getcontenttype"),
];

fn propfind(cx: &Cx<'_>, s: &State, path: &str, depth1: bool, doc: Option<&El>) -> Response {
    let Some(res) = resolve(cx.inner, s, path) else {
        return status(StatusCode::NOT_FOUND);
    };
    if let Some(d) = doc
        && !d.is(NS_DAV, "propfind")
    {
        return (StatusCode::BAD_REQUEST, "expected D:propfind").into_response();
    }
    // `None` = allprop (also an empty body and `propname`).
    let wanted: Option<Vec<(String, String)>> = doc.and_then(|d| d.kid(NS_DAV, "prop")).map(|p| {
        p.kids
            .iter()
            .map(|k| (k.ns.clone(), k.name.clone()))
            .collect()
    });
    let mut targets = vec![(path_of(cx.inner, &res), res.clone())];
    if depth1 {
        match &res {
            Res::Home(cal) => {
                let home = cx.inner.home(*cal);
                for p in s.collections.keys().filter(|p| p.starts_with(&home)) {
                    targets.push((p.clone(), Res::Collection(p.clone())));
                }
            }
            Res::Collection(col) => {
                for (name, o) in &s.collections[col].objects {
                    if o.body.is_some() {
                        targets.push((
                            format!("{col}{name}"),
                            Res::Object(col.clone(), name.clone()),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = multistatus_open();
    for (href, res) in &targets {
        let (found, missing) = match &wanted {
            Some(w) => {
                let mut found = String::new();
                let mut missing = String::new();
                for (ns, name) in w {
                    match prop(cx, s, res, ns, name) {
                        Some(v) => found.push_str(&v),
                        None => missing.push_str(&empty_el(ns, name)),
                    }
                }
                (found, missing)
            }
            None => (
                ALLPROP
                    .iter()
                    .filter_map(|(ns, n)| prop(cx, s, res, ns, n))
                    .collect(),
                String::new(),
            ),
        };
        response(&mut out, href, &found, &missing);
    }
    out.push_str("</D:multistatus>\n");
    xml(StatusCode::MULTI_STATUS, out)
}

fn path_of(inner: &Inner, res: &Res) -> String {
    match res {
        Res::Root => "/".into(),
        Res::Principal => inner.principal(),
        Res::Home(cal) => inner.home(*cal),
        Res::Collection(p) => p.clone(),
        Res::Object(c, n) => format!("{c}{n}"),
    }
}

/// One property of a resource as a complete element, or `None` (a `404` propstat).
fn prop(cx: &Cx<'_>, s: &State, res: &Res, ns: &str, name: &str) -> Option<String> {
    let inner = cx.inner;
    let icloud = inner.config.profile == Profile::Icloud;
    let col = match res {
        Res::Collection(p) | Res::Object(p, _) => s.collections.get(p),
        _ => None,
    };
    let obj = match res {
        Res::Object(p, n) => s.collections[p].objects.get(n),
        _ => None,
    };
    // Which home sets the principal on this host reports.
    let (cal_here, card_here) = if inner.split() {
        let card = matches!(cx.host, Host::Carddav | Host::CarddavPartition);
        (!card, card)
    } else {
        (true, true)
    };
    let v = match (ns, name) {
        (NS_DAV, "resourcetype") => {
            let inside = match res {
                Res::Root | Res::Home(_) => "<D:collection/>",
                Res::Principal => "<D:collection/><D:principal/>",
                Res::Object(..) => "",
                Res::Collection(_) => match col?.kind {
                    Kind::Calendar(_) => "<D:collection/><C:calendar/>",
                    Kind::AddressBook => "<D:collection/><CR:addressbook/>",
                    Kind::ScheduleInbox => "<D:collection/><C:schedule-inbox/>",
                    Kind::ScheduleOutbox => "<D:collection/><C:schedule-outbox/>",
                    Kind::Notifications => "<D:collection/><CS:notification/>",
                    Kind::Subscribed => "<D:collection/><CS:subscribed/>",
                },
            };
            format!("<D:resourcetype>{inside}</D:resourcetype>")
        }
        (NS_DAV, "displayname") => match res {
            Res::Collection(_) => format!("<D:displayname>{}</D:displayname>", esc(&col?.name)),
            Res::Principal => format!("<D:displayname>{}</D:displayname>", esc(&inner.config.user)),
            _ => return None,
        },
        (NS_DAV, "current-user-principal") => {
            format!(
                "<D:current-user-principal><D:href>{}</D:href></D:current-user-principal>",
                inner.principal()
            )
        }
        (NS_DAV, "principal-URL") if matches!(res, Res::Principal) => {
            format!(
                "<D:principal-URL><D:href>{}</D:href></D:principal-URL>",
                inner.principal()
            )
        }
        (NS_CALDAV, "calendar-home-set") if matches!(res, Res::Principal) && cal_here => format!(
            "<C:calendar-home-set><D:href>{}</D:href></C:calendar-home-set>",
            esc(&inner.home_href(true))
        ),
        (NS_CARDDAV, "addressbook-home-set") if matches!(res, Res::Principal) && card_here => {
            format!(
                "<CR:addressbook-home-set><D:href>{}</D:href></CR:addressbook-home-set>",
                esc(&inner.home_href(false))
            )
        }
        (NS_CALDAV, "supported-calendar-component-set") if obj.is_none() => match col?.kind {
            Kind::Calendar(comps) if !comps.is_empty() => {
                let mut v = String::from("<C:supported-calendar-component-set>");
                for c in comps {
                    let _ = write!(v, "<C:comp name=\"{c}\"/>");
                }
                v + "</C:supported-calendar-component-set>"
            }
            _ => return None,
        },
        (NS_DAV, "supported-report-set") if obj.is_none() => {
            let c = col?;
            let mut reports: Vec<(&str, &str)> = match c.kind {
                Kind::Calendar(_) => vec![("C", "calendar-query"), ("C", "calendar-multiget")],
                Kind::AddressBook => vec![("CR", "addressbook-multiget")],
                _ => return None,
            };
            if c.syncs() {
                reports.push(("D", "sync-collection"));
            }
            let mut v = String::from("<D:supported-report-set>");
            for (p, r) in reports {
                let _ = write!(
                    v,
                    "<D:supported-report><D:report><{p}:{r}/></D:report></D:supported-report>"
                );
            }
            v + "</D:supported-report-set>"
        }
        (NS_CS, "getctag") if obj.is_none() => {
            let c = col.filter(|c| matches!(c.kind, Kind::Calendar(_) | Kind::AddressBook))?;
            if c.has(Quirk::NoCtag) {
                return None;
            }
            format!("<CS:getctag>ctag-{}</CS:getctag>", c.changed)
        }
        (NS_DAV, "sync-token") if obj.is_none() => {
            let Res::Collection(p) = res else { return None };
            if !col?.syncs() {
                return None;
            }
            format!(
                "<D:sync-token>{SYNC_TOKEN_PREFIX}{p}{}</D:sync-token>",
                s.version
            )
        }
        (NS_DAV, "current-user-privilege-set") if !icloud && obj.is_none() => {
            let privs: &[&str] = if col?.has(Quirk::ReadOnly) {
                &["read", "read-current-user-privilege-set"]
            } else {
                &[
                    "read",
                    "write",
                    "write-properties",
                    "write-content",
                    "bind",
                    "unbind",
                    "read-current-user-privilege-set",
                ]
            };
            let mut v = String::from("<D:current-user-privilege-set>");
            for p in privs {
                let _ = write!(v, "<D:privilege><D:{p}/></D:privilege>");
            }
            v + "</D:current-user-privilege-set>"
        }
        (NS_CARDDAV, "supported-address-data")
            if obj.is_none() && col?.kind == Kind::AddressBook =>
        {
            let mut v = String::from("<CR:supported-address-data>");
            v.push_str("<CR:address-data-type content-type=\"text/vcard\" version=\"3.0\"/>");
            if !icloud {
                v.push_str("<CR:address-data-type content-type=\"text/vcard\" version=\"4.0\"/>");
            }
            v + "</CR:supported-address-data>"
        }
        (NS_DAV, "getetag") => format!("<D:getetag>{}</D:getetag>", esc(&col?.etag(obj?)?)),
        (NS_DAV, "getcontenttype") => {
            obj?;
            format!(
                "<D:getcontenttype>{}</D:getcontenttype>",
                content_type(col?)
            )
        }
        (NS_CALDAV, "calendar-data") | (NS_CARDDAV, "address-data") => {
            let c = col?;
            if c.data_prop() != (ns, name) {
                return None;
            }
            let tag = if ns == NS_CALDAV {
                "C:calendar-data"
            } else {
                "CR:address-data"
            };
            format!("<{tag}>{}</{tag}>", esc(obj?.body.as_ref()?))
        }
        _ => return None,
    };
    Some(v)
}

fn multistatus_open() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"{NS_DAV}\" xmlns:C=\"{NS_CALDAV}\" xmlns:CR=\"{NS_CARDDAV}\" xmlns:CS=\"{NS_CS}\">\n"
    )
}

fn response(out: &mut String, href: &str, found: &str, missing: &str) {
    let _ = write!(
        out,
        "<D:response><D:href>{}</D:href>",
        esc(&pct_encode(href))
    );
    // A response needs a propstat or a status (RFC 4918 14.24), also when no prop was asked.
    if !found.is_empty() || missing.is_empty() {
        let _ = write!(
            out,
            "<D:propstat><D:prop>{found}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"
        );
    }
    if !missing.is_empty() {
        let _ = write!(
            out,
            "<D:propstat><D:prop>{missing}</D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"
        );
    }
    out.push_str("</D:response>\n");
}

fn status_response(out: &mut String, href: &str, line: &str) {
    let _ = writeln!(
        out,
        "<D:response><D:href>{}</D:href><D:status>HTTP/1.1 {line}</D:status></D:response>",
        esc(&pct_encode(href))
    );
}

fn empty_el(ns: &str, name: &str) -> String {
    match ns {
        NS_DAV => format!("<D:{name}/>"),
        NS_CALDAV => format!("<C:{name}/>"),
        NS_CARDDAV => format!("<CR:{name}/>"),
        NS_CS => format!("<CS:{name}/>"),
        _ => format!("<X:{name} xmlns:X=\"{}\"/>", esc(ns)),
    }
}

fn esc(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

// ---------------------------------------------------------------------------
// REPORT

fn report_(
    cx: &Cx<'_>,
    s: &mut State,
    path: &str,
    depth: Option<&str>,
    doc: &El,
    truncate: Option<usize>,
    before: Option<Fault>,
) -> Response {
    let col = match resolve(cx.inner, s, path) {
        Some(Res::Collection(c)) => c,
        Some(_) => return dav_error(StatusCode::FORBIDDEN, "supported-report"),
        None => return status(StatusCode::NOT_FOUND),
    };
    let kind = s.collections[&col].kind;
    let props: Vec<(String, String)> = doc
        .kid(NS_DAV, "prop")
        .map(|p| {
            p.kids
                .iter()
                .map(|k| (k.ns.clone(), k.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    match (doc.ns.as_str(), doc.name.as_str(), kind) {
        (NS_DAV, "sync-collection", _) if s.collections[&col].syncs() => {
            // RFC 6578 3.2: Depth 0 only (REPORT defaults to 0), sync-level required.
            if depth.is_some_and(|d| d != "0") || doc.kid(NS_DAV, "sync-level").is_none() {
                return (
                    StatusCode::BAD_REQUEST,
                    "sync-collection needs Depth: 0 and DAV:sync-level",
                )
                    .into_response();
            }
            sync_collection(cx, s, &col, doc, &props, truncate)
        }
        (NS_CALDAV, "calendar-query", Kind::Calendar(_)) => {
            calendar_query(cx, s, &col, doc, &props)
        }
        (NS_CALDAV, "calendar-multiget", Kind::Calendar(_))
        | (NS_CARDDAV, "addressbook-multiget", Kind::AddressBook) => {
            multiget(cx, s, &col, doc, &props, before)
        }
        _ => dav_error(StatusCode::FORBIDDEN, "supported-report"),
    }
}

fn props_of(cx: &Cx<'_>, s: &State, res: &Res, props: &[(String, String)]) -> (String, String) {
    let mut found = String::new();
    let mut missing = String::new();
    for (ns, n) in props {
        match prop(cx, s, res, ns, n) {
            Some(v) => found.push_str(&v),
            None => missing.push_str(&empty_el(ns, n)),
        }
    }
    (found, missing)
}

fn sync_collection(
    cx: &Cx<'_>,
    s: &State,
    col: &str,
    doc: &El,
    props: &[(String, String)],
    truncate: Option<usize>,
) -> Response {
    let c = &s.collections[col];
    let token = doc.kid(NS_DAV, "sync-token").map_or("", |t| t.text.trim());
    let since = if token.is_empty() {
        None
    } else {
        let n = token
            .strip_prefix(SYNC_TOKEN_PREFIX)
            .and_then(|t| t.strip_prefix(col))
            .and_then(|n| n.parse::<u64>().ok());
        match n {
            Some(n) if n >= c.min_sync && n <= s.version => Some(n),
            _ => return dav_error(StatusCode::FORBIDDEN, "valid-sync-token"),
        }
    };
    let limit = doc
        .kid(NS_DAV, "limit")
        .and_then(|l| l.kid(NS_DAV, "nresults"))
        .and_then(|n| n.text.trim().parse::<usize>().ok());
    let limit = match (truncate, limit) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    // An initial read lists live members only; a delta lists every change since the token.
    let mut changed: Vec<(&String, &Obj)> = c
        .objects
        .iter()
        .filter(|(_, o)| match since {
            None => o.body.is_some(),
            Some(n) => o.version > n,
        })
        .collect();
    changed.sort_by_key(|(_, o)| o.version);
    let truncated = limit.is_some_and(|l| changed.len() > l);
    if let Some(l) = limit {
        changed.truncate(l);
    }
    let new_token = if truncated {
        changed
            .last()
            .map_or(since.unwrap_or(c.min_sync), |(_, o)| o.version)
    } else {
        s.version
    };
    let mut out = multistatus_open();
    for (name, o) in changed {
        let href = format!("{col}{name}");
        if o.body.is_some() {
            let (found, missing) =
                props_of(cx, s, &Res::Object(col.to_owned(), name.clone()), props);
            response(&mut out, &href, &found, &missing);
        } else {
            status_response(&mut out, &href, "404 Not Found");
        }
    }
    if truncated {
        status_response(&mut out, col, "507 Insufficient Storage");
    }
    let _ = writeln!(
        out,
        "<D:sync-token>{SYNC_TOKEN_PREFIX}{col}{new_token}</D:sync-token>"
    );
    out.push_str("</D:multistatus>\n");
    xml(StatusCode::MULTI_STATUS, out)
}

fn calendar_query(
    cx: &Cx<'_>,
    s: &State,
    col: &str,
    doc: &El,
    props: &[(String, String)],
) -> Response {
    // filter > comp-filter VCALENDAR > comp-filter VEVENT|VTODO > time-range?
    let inner_filter = doc
        .kid(NS_CALDAV, "filter")
        .and_then(|f| f.kid(NS_CALDAV, "comp-filter"))
        .and_then(|f| f.kid(NS_CALDAV, "comp-filter"));
    let comp = inner_filter.and_then(|f| f.attr("name"));
    let range = inner_filter
        .and_then(|f| f.kid(NS_CALDAV, "time-range"))
        .map(|r| {
            let at = |a| r.attr(a).and_then(|v| parse_time(v, None)).map(|(t, _)| t);
            (at("start"), at("end"))
        });
    let mut out = multistatus_open();
    for (name, o) in &s.collections[col].objects {
        let Some(body) = &o.body else { continue };
        let ok = match comp {
            None => true,
            Some(comp) => match &range {
                None => component(body, comp).is_some(),
                Some((start, end)) => component(body, comp)
                    .is_some_and(|lines| overlaps(&lines, start.as_ref(), end.as_ref())),
            },
        };
        if ok {
            let (found, missing) =
                props_of(cx, s, &Res::Object(col.to_owned(), name.clone()), props);
            response(&mut out, &format!("{col}{name}"), &found, &missing);
        }
    }
    out.push_str("</D:multistatus>\n");
    xml(StatusCode::MULTI_STATUS, out)
}

fn multiget(
    cx: &Cx<'_>,
    s: &mut State,
    col: &str,
    doc: &El,
    props: &[(String, String)],
    before: Option<Fault>,
) -> Response {
    let hrefs: Vec<String> = doc
        .kids
        .iter()
        .filter(|k| k.is(NS_DAV, "href"))
        .map(|k| href_path(k.text.trim()))
        .collect();
    for h in &hrefs {
        let (c, name) = split_object(h);
        if c != col {
            continue;
        }
        match before {
            Some(Fault::ChangeBeforeMultiget) => s.touch(col, name),
            Some(Fault::DeleteBeforeMultiget)
                if s.collections[col]
                    .objects
                    .get(name)
                    .is_some_and(|o| o.body.is_some()) =>
            {
                s.tombstone(col, name);
            }
            _ => {}
        }
    }
    let mut out = multistatus_open();
    for h in &hrefs {
        let (c, name) = split_object(h);
        let live = c == col
            && s.collections[col]
                .objects
                .get(name)
                .is_some_and(|o| o.body.is_some());
        if live {
            let (found, missing) =
                props_of(cx, s, &Res::Object(col.to_owned(), name.to_owned()), props);
            response(&mut out, h, &found, &missing);
        } else {
            status_response(&mut out, h, "404 Not Found");
        }
    }
    out.push_str("</D:multistatus>\n");
    xml(StatusCode::MULTI_STATUS, out)
}

/// The decoded path of an href that may be an absolute URL.
fn href_path(href: &str) -> String {
    pct_decode(match href.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => href,
    })
}

/// Percent-decodes a path; a `%` not followed by two hex digits stays as is.
fn pct_decode(path: &str) -> String {
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = path
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes everything but unreserved characters and `/`, as Radicale
/// does (`a@b c.ics` → `a%40b%20c.ics`).
fn pct_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &c in path.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-._~/".contains(&c) {
            out.push(c as char);
        } else {
            let _ = write!(out, "%{c:02X}");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// calendar-query time ranges (a deliberately small subset, FIDELITY.md)

/// The unfolded property lines of the first `comp` component of an iCalendar body.
fn component(body: &str, comp: &str) -> Option<Vec<String>> {
    let unfolded = body
        .replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "");
    let begin = format!("BEGIN:{comp}");
    let end = format!("END:{comp}");
    let mut lines = unfolded.lines().map(str::trim_end);
    lines.find(|l| *l == begin)?;
    Some(lines.take_while(|l| *l != end).map(str::to_owned).collect())
}

/// `(instant, is_date)`; TZID and floating times are read as UTC (callers widen).
fn parse_time(value: &str, params: Option<&str>) -> Option<(Zoned, bool)> {
    let utc = TimeZone::UTC;
    if params.is_some_and(|p| p.contains("VALUE=DATE")) || value.len() == 8 {
        let d = Date::strptime("%Y%m%d", value).ok()?;
        return Some((d.to_zoned(utc).ok()?, true));
    }
    let v = value.trim_end_matches('Z');
    let dt = DateTime::strptime("%Y%m%dT%H%M%S", v).ok()?;
    Some((dt.to_zoned(utc).ok()?, false))
}

fn prop_line<'a>(lines: &'a [String], name: &str) -> Option<(&'a str, &'a str)> {
    lines.iter().find_map(|l| {
        let (head, value) = l.split_once(':')?;
        let (n, params) = head.split_once(';').unwrap_or((head, ""));
        (n == name).then_some((params, value))
    })
}

/// RFC 4791 9.9, simplified: unparseable or open-ended means "overlaps".
fn overlaps(lines: &[String], start: Option<&Zoned>, end: Option<&Zoned>) -> bool {
    let Some((params, value)) = prop_line(lines, "DTSTART").or_else(|| prop_line(lines, "DUE"))
    else {
        return true; // a VTODO without DTSTART and DUE matches every range
    };
    let Some((mut s, is_date)) = parse_time(value, Some(params)) else {
        return true;
    };
    let floating = !value.ends_with('Z') && !is_date;
    let mut e = if prop_line(lines, "RRULE").is_some() {
        None
    } else if let Some((p, v)) = prop_line(lines, "DTEND").or_else(|| prop_line(lines, "DUE")) {
        parse_time(v, Some(p)).map(|(t, _)| t)
    } else if let Some((_, v)) = prop_line(lines, "DURATION") {
        v.parse::<Span>().ok().and_then(|d| s.checked_add(d).ok())
    } else if is_date {
        s.checked_add(Span::new().days(1)).ok()
    } else {
        Some(s.clone())
    };
    if floating {
        // shortcut: TZID and floating times are widened by 14 h instead of resolved; over-includes near the edges.
        let w = Span::new().hours(14);
        s = s.checked_sub(w).unwrap_or(s);
        e = e.and_then(|e| e.checked_add(w).ok());
    }
    let before_end = end.is_none_or(|end| s < *end);
    let after_start = match (start, e) {
        (None, _) | (_, None) => true,
        (Some(start), Some(e)) => e > *start || (e == s && s >= *start),
    };
    before_end && after_start
}

// ---------------------------------------------------------------------------
// XML: a tiny namespace-resolved tree (request bodies; tests read responses with it)

/// One XML element with resolved namespace.
#[derive(Debug, Default)]
pub struct El {
    /// Namespace URI (empty when unbound).
    pub ns: String,
    /// Local name.
    pub name: String,
    /// Attributes by local name.
    pub attrs: Vec<(String, String)>,
    /// Text content, entities resolved (child text concatenated).
    pub text: String,
    /// Child elements.
    pub kids: Vec<El>,
}

impl El {
    /// The element is `{ns}name`.
    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    /// The first child `{ns}name`.
    pub fn kid(&self, ns: &str, name: &str) -> Option<&El> {
        self.kids.iter().find(|k| k.is(ns, name))
    }

    /// Every child `{ns}name`.
    pub fn all<'a>(&'a self, ns: &'a str, name: &'a str) -> impl Iterator<Item = &'a El> {
        self.kids.iter().filter(move |k| k.is(ns, name))
    }

    /// An attribute by local name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Parses an XML document into its root element. No DTD processing, so no
/// external entities.
pub fn parse(xml: &str) -> Result<El, String> {
    let mut r = NsReader::from_str(xml);
    let mut stack = vec![El::default()];
    loop {
        let (res, ev) = r.read_resolved_event().map_err(|e| e.to_string())?;
        let ns = match res {
            ResolveResult::Bound(n) => n.as_ref().to_owned(),
            _ => String::new(),
        };
        let open = |e: &quick_xml::events::BytesStart<'_>| -> Result<El, String> {
            let mut attrs = Vec::new();
            for a in e.attributes() {
                let a = a.map_err(|e| e.to_string())?;
                let v = a
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|e| e.to_string())?;
                attrs.push((a.key.local_name().as_ref().to_owned(), v.into_owned()));
            }
            Ok(El {
                ns: ns.clone(),
                name: e.local_name().as_ref().to_owned(),
                attrs,
                ..El::default()
            })
        };
        let top = stack.len() - 1;
        match ev {
            Event::Start(e) => {
                let el = open(&e)?;
                stack.push(el);
            }
            Event::Empty(e) => {
                let el = open(&e)?;
                stack[top].kids.push(el);
            }
            Event::End(_) => {
                let el = stack.pop().ok_or("unbalanced end tag")?;
                stack.last_mut().ok_or("unbalanced end tag")?.kids.push(el);
            }
            Event::Text(t) => stack[top].text.push_str(&t.xml10_content()),
            Event::CData(t) => stack[top].text.push_str(&t.xml10_content()),
            Event::GeneralRef(g) => {
                if let Some(c) = g.resolve_char_ref().map_err(|e| e.to_string())? {
                    stack[top].text.push(c);
                } else {
                    let v = resolve_predefined_entity(&g).ok_or("unknown entity")?;
                    stack[top].text.push_str(v);
                }
            }
            Event::DocType(_) => return Err("DTD not allowed".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    let mut root = stack
        .pop()
        .filter(|_| stack.is_empty())
        .ok_or("unclosed element")?;
    root.kids
        .pop()
        .filter(|_| root.kids.is_empty())
        .ok_or_else(|| "expected one root element".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaces_and_entities() {
        let el = parse(
            "<?xml version=\"1.0\"?><d:sync-collection xmlns:d=\"DAV:\"><d:sync-token>a&amp;b&#33;</d:sync-token><d:prop><x:y xmlns:x=\"urn:x\"/></d:prop></d:sync-collection>",
        )
        .expect("parse");
        assert!(el.is(NS_DAV, "sync-collection"));
        assert_eq!(el.kid(NS_DAV, "sync-token").expect("token").text, "a&b!");
        assert!(el.kid(NS_DAV, "prop").expect("prop").kids[0].is("urn:x", "y"));
        assert!(parse("<a><b></a>").is_err());
        assert!(
            parse("<!DOCTYPE a [<!ENTITY e SYSTEM \"file:///etc/passwd\">]><a>&e;</a>").is_err()
        );
    }

    #[test]
    fn percent_coding_round_trips() {
        assert_eq!(pct_encode("/c/a@b c.ics"), "/c/a%40b%20c.ics");
        assert_eq!(pct_decode("/c/a%40b%20c.ics"), "/c/a@b c.ics");
        assert_eq!(pct_decode("/c/%C3%A9%zz%4"), "/c/é%zz%4");
        assert_eq!(
            pct_decode(&pct_encode("/c/žluť 100%.vcf")),
            "/c/žluť 100%.vcf"
        );
    }

    #[test]
    fn time_range_overlap() {
        let z = |s: &str| parse_time(s, None).expect("time").0;
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let (a, b) = (z("20261110T000000Z"), z("20261112T000000Z"));
        let in_range = lines(&["DTSTART:20261111T100000Z", "DTEND:20261111T110000Z"]);
        let before = lines(&["DTSTART:20261101T100000Z", "DURATION:PT1H"]);
        let all_day = lines(&["DTSTART;VALUE=DATE:20261109"]);
        let series = lines(&["DTSTART:20250101T100000Z", "RRULE:FREQ=WEEKLY"]);
        let floating = lines(&["DTSTART;TZID=Europe/Prague:20261112T080000"]);
        assert!(overlaps(&in_range, Some(&a), Some(&b)));
        assert!(!overlaps(&before, Some(&a), Some(&b)));
        assert!(!overlaps(&all_day, Some(&a), Some(&b)));
        assert!(overlaps(&series, Some(&a), Some(&b)));
        assert!(overlaps(&floating, Some(&a), Some(&b)), "widened by 14 h");
        assert!(overlaps(&lines(&["SUMMARY:no dates"]), Some(&a), Some(&b)));
    }
}
