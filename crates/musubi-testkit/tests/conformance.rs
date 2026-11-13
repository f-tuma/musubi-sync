//! The conformance runner (plan, chapter 8.1) and the Radicale harness
//! (chapter 8.3).
//!
//! Every scenario in `conformance/*.json` runs against every target through a
//! `musubi-syncd` process. The binary comes from `MUSUBI_SYNCD`, else from
//! next to this test's `deps` directory (`cargo build -p musubi-syncd`
//! puts it there). Radicale needs `MUSUBI_RADICALE` (see
//! `musubi_testkit::radicale`). The CI job `conformance` runs this file:
//!
//! ```text
//! cargo build -p musubi-syncd --locked
//! MUSUBI_RADICALE=/tmp/radicale-venv cargo test -p musubi-testkit --test conformance --locked
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Span, Timestamp};
use musubi_testkit::conformance::{Expect, Fault, Kind, Object, Scenario, Skip, Step, Target};
use musubi_testkit::google::{self, Change, Endpoint, FakeGoogle};
use musubi_testkit::harness::{self, Api, Daemon, free_addr};
use musubi_testkit::radicale::{self, Radicale};
use serde_json::{Value, json};

/// Target names, in report order. K4 adds the DAV targets here.
const TARGETS: &[&str] = &["google"];

async fn target(name: &str) -> Box<dyn Target> {
    match name {
        "google" => Box::new(GoogleTarget::start().await),
        other => panic!("unknown target {other}"),
    }
}

fn syncd() -> PathBuf {
    let bin = std::env::var_os("MUSUBI_SYNCD").map_or_else(
        || {
            // target/debug/deps/conformance-… → target/debug/musubi-syncd
            let exe = std::env::current_exe().expect("exe");
            exe.parent()
                .and_then(Path::parent)
                .expect("target dir")
                .join("musubi-syncd")
        },
        PathBuf::from,
    );
    assert!(
        bin.is_file(),
        "{} is missing: run `cargo build -p musubi-syncd` first or set MUSUBI_SYNCD",
        bin.display()
    );
    bin
}

fn scenarios() -> Vec<(PathBuf, Scenario)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("conformance dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).expect("read");
            let s = Scenario::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert_eq!(
                Some(s.name.as_str()),
                p.file_stem().and_then(|n| n.to_str()),
                "{}: name must match the file name",
                p.display()
            );
            (p, s)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn scenarios_pass_on_every_target() {
    let bin = syncd();
    let mut passed = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    for (_, s) in scenarios() {
        for &name in TARGETS {
            if s.only
                .as_ref()
                .is_some_and(|o| !o.iter().any(|t| t == name))
            {
                continue;
            }
            let id = format!("{} @ {name}", s.name);
            let mut t = target(name).await;
            if let Some(k) = s.kinds.iter().find(|k| !t.kinds().contains(k)) {
                skipped.push(format!("{id}: {name} has no {k:?}"));
                continue;
            }
            match run(&bin, &s, t.as_mut()).await {
                Ok(()) => passed.push(id),
                Err(Stop::Skip(r)) => skipped.push(format!("{id}: {r}")),
                Err(Stop::Fail(r)) => failed.push(format!("{id}: {r}")),
            }
        }
    }
    eprintln!("conformance: {} passed: {passed:?}", passed.len());
    eprintln!("conformance: {} skipped:", skipped.len());
    for s in &skipped {
        eprintln!("  skipped {s}");
    }
    for f in &failed {
        eprintln!("  FAILED {f}");
    }
    assert!(failed.is_empty(), "{} scenario(s) failed", failed.len());
    assert!(!passed.is_empty(), "no scenario ran");
}

enum Stop {
    Skip(String),
    Fail(String),
}

impl From<Skip> for Stop {
    fn from(s: Skip) -> Self {
        Stop::Skip(s.0)
    }
}

/// One node over one target.
struct Run<'a> {
    target: &'a mut dyn Target,
    bin: PathBuf,
    config: PathBuf,
    daemon: Daemon,
    api: Api,
    admin: String,
    read: String,
    account: Option<String>,
    /// Ref → target handle.
    handles: BTreeMap<String, String>,
    /// Ref → object, for refs not deleted.
    live: BTreeMap<String, Object>,
    /// Ref → label of its last content (also after deletion).
    labels: BTreeMap<String, String>,
    /// Collection name → handle.
    collections: BTreeMap<String, String>,
    /// Journal head and request counts at the previous `expect`.
    mark: u64,
    marked: BTreeMap<String, usize>,
}

async fn run(bin: &Path, s: &Scenario, target: &mut dyn Target) -> Result<(), Stop> {
    let dir = tempfile::tempdir().expect("tempdir");
    let listen = free_addr();
    let data = dir.path().join("data");
    let config = dir.path().join("config.json");
    std::fs::write(&config, target.config(listen, &data).to_string()).expect("config");
    let daemon = Daemon::serve(bin, &config, &[]);
    let api = Api::new(listen);
    let tenant = api.tenant().await;
    let admin = api.key(&tenant, "admin").await;
    let read = api.key(&tenant, "read").await;
    let mut r = Run {
        target,
        bin: bin.to_owned(),
        config,
        daemon,
        api,
        admin,
        read,
        account: None,
        handles: BTreeMap::new(),
        live: BTreeMap::new(),
        labels: BTreeMap::new(),
        collections: BTreeMap::new(),
        mark: 0,
        marked: BTreeMap::new(),
    };
    let counted: BTreeSet<String> = s
        .steps
        .iter()
        .filter_map(|st| match st {
            Step::Expect(e) => Some(e.no_requests.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    for (i, step) in s.steps.iter().enumerate() {
        r.step(step, &counted).await.map_err(|e| match e {
            Stop::Fail(m) => Stop::Fail(format!("step {i} ({step:?}): {m}\n{}", r.daemon.tail(30))),
            skip => skip,
        })?;
    }
    Ok(())
}

impl Run<'_> {
    async fn step(&mut self, step: &Step, counted: &BTreeSet<String>) -> Result<(), Stop> {
        match step {
            Step::Put {
                reference,
                collection,
                object,
            } => {
                let col = match collection {
                    Some(c) => Some(
                        self.collections
                            .get(c)
                            .ok_or_else(|| Stop::Fail(format!("no collection {c}")))?
                            .clone(),
                    ),
                    None => None,
                };
                let handle = self
                    .target
                    .put(
                        self.handles.get(reference).map(String::as_str),
                        col.as_deref(),
                        object,
                        &self.handles,
                    )
                    .await?;
                self.handles.insert(reference.clone(), handle);
                self.labels
                    .insert(reference.clone(), object.label().to_owned());
                self.live.insert(reference.clone(), object.clone());
            }
            Step::Delete { reference } => {
                let h = self
                    .handles
                    .get(reference)
                    .ok_or_else(|| Stop::Fail(format!("no ref {reference}")))?;
                self.target.delete(h).await?;
                self.live.remove(reference);
            }
            Step::Connect => {
                self.account = Some(self.target.connect(&self.api, &self.admin).await?);
                self.remark(counted).await;
            }
            Step::Reconnect => self.target.reconnect(&self.api, &self.admin).await?,
            Step::ExpireCursor { collection } => {
                let col = collection
                    .as_ref()
                    .and_then(|c| self.collections.get(c))
                    .cloned();
                self.target.expire_cursor(col.as_deref()).await?;
            }
            Step::Inject { fault, times } => self.target.inject(fault, *times).await?,
            Step::AddCollection { name, kinds } => {
                let h = self.target.add_collection(name, kinds).await?;
                self.collections.insert(name.clone(), h);
            }
            Step::RemoveCollection { name } => {
                let h = self
                    .collections
                    .remove(name)
                    .ok_or_else(|| Stop::Fail(format!("no collection {name}")))?;
                // shortcut: the runner does not track which refs lived there; a
                // scenario removing a collection deletes its refs first.
                self.target.remove_collection(&h).await?;
            }
            Step::Kill => {
                self.daemon.kill();
                self.daemon = Daemon::serve(&self.bin, &self.config, &[]);
            }
            Step::Wait { ms } => tokio::time::sleep(Duration::from_millis(*ms)).await,
            Step::Expect(e) => {
                self.expect(e).await.map_err(Stop::Fail)?;
                self.remark(counted).await;
            }
        }
        Ok(())
    }

    async fn remark(&mut self, counted: &BTreeSet<String>) {
        self.mark = self.journal().await.map(|j| j.head).unwrap_or(self.mark);
        self.marked = counted
            .iter()
            .filter_map(|l| Some((l.clone(), self.target.requests(l).ok()?)))
            .collect();
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let (s, v) = self
            .api
            .call("GET", path, Some(&self.read), None)
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        if s == 200 {
            Ok(v)
        } else {
            Err(format!("GET {path}: {s} {v}"))
        }
    }

    /// The whole journal: entity ids by first seq, and the head.
    async fn journal(&self) -> Result<Journal, String> {
        let mut j = Journal::default();
        loop {
            let v = self
                .get(&format!("/v1/changes?since={}&limit=1000", j.head))
                .await?;
            for c in v["changes"].as_array().ok_or("changes")? {
                let seq = c["seq"].as_u64().ok_or("seq")?;
                if c["target"]["type"] == "entity" {
                    let id = c["target"]["id"].as_str().ok_or("id")?.to_owned();
                    j.first.entry(id).or_insert(seq);
                    j.last_entity = j.last_entity.max(seq);
                }
            }
            j.head = v["next"].as_u64().ok_or("next")?;
            if v["more"] != true {
                return Ok(j);
            }
        }
    }

    /// Live entities: id → (kind, label).
    async fn entities(&self, j: &Journal) -> Result<BTreeMap<String, (String, String)>, String> {
        let mut out = BTreeMap::new();
        for id in j.first.keys() {
            let e = self.get(&format!("/v1/entities/{id}")).await?;
            if e["tombstone"] == true {
                continue;
            }
            let c = &e["canonical"];
            let label = [&c["title"], &c["name"]["full"], &c["name"]]
                .into_iter()
                .find_map(Value::as_str)
                .unwrap_or_default();
            let kind = e["kind"].as_str().unwrap_or_default();
            out.insert(id.clone(), (kind.to_owned(), label.to_owned()));
        }
        Ok(out)
    }

    async fn expect(&mut self, e: &Expect) -> Result<(), String> {
        let timeout = Duration::from_millis(e.timeout_ms.unwrap_or(20_000));
        let quiet = Duration::from_millis(e.quiet_ms.unwrap_or(2_000));
        if e.unchanged || !e.no_requests.is_empty() {
            tokio::time::sleep(quiet).await;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let why = match self.check(e).await {
                Ok(()) => return Ok(()),
                Err(why) => why,
            };
            if Instant::now() > deadline {
                return Err(format!("not met after {timeout:?}: {why}"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn check(&self, e: &Expect) -> Result<(), String> {
        let j = self.journal().await?;
        if e.converged || !e.window_first.is_empty() || e.raw_contains.is_some() {
            let ents = self.entities(&j).await?;
            if e.converged {
                let mut got: Vec<_> = ents.values().cloned().collect();
                let mut want: Vec<_> = self
                    .live
                    .values()
                    .map(|o| (o.kind().api().to_owned(), o.label().to_owned()))
                    .collect();
                got.sort();
                want.sort();
                if got != want {
                    return Err(format!("entities {got:?}, want {want:?}"));
                }
                let cols = self.get("/v1/collections").await?;
                for c in cols["collections"].as_array().ok_or("collections")? {
                    if c["gone"] != true
                        && (c["coverage"]["type"] != "full" || !c["error"].is_null())
                    {
                        return Err(format!("collection not fully read: {c}"));
                    }
                }
            }
            if !e.window_first.is_empty() {
                let seq_of = |label: &str| {
                    j.first
                        .iter()
                        .find(|(id, _)| ents.get(*id).is_some_and(|(_, l)| l == label))
                        .map(|(_, s)| *s)
                };
                let mut window = Vec::new();
                let mut rest = Vec::new();
                for (r, label) in &self.labels {
                    let seq = seq_of(label).ok_or_else(|| format!("{r} not in the journal"))?;
                    if e.window_first.contains(r) {
                        window.push(seq);
                    } else {
                        rest.push(seq);
                    }
                }
                if window.iter().max() >= rest.iter().min() && !rest.is_empty() {
                    return Err(format!(
                        "window seqs {window:?} not before the rest {rest:?}"
                    ));
                }
            }
            if let Some(rc) = &e.raw_contains {
                let label = self.labels.get(&rc.reference).ok_or("unknown ref")?;
                let id = ents
                    .iter()
                    .find(|(_, (_, l))| l == label)
                    .map(|(id, _)| id)
                    .ok_or_else(|| format!("{} not synced", rc.reference))?;
                let ent = self
                    .get(&format!("/v1/entities/{id}?include=placements"))
                    .await?;
                let item = ent["placements"][0]["item"]
                    .as_str()
                    .ok_or("no placement")?;
                let raw = self.get(&format!("/v1/items/{item}?include=raw")).await?;
                let body = match &raw["raw"]["body"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if !body.contains(&rc.text) {
                    return Err(format!("raw body lacks {:?}: {body}", rc.text));
                }
            }
        }
        if let Some(want) = &e.account_state {
            let a = self.account.as_ref().ok_or("not connected")?;
            let v = self.get(&format!("/v1/accounts/{a}")).await?;
            if v["state"] != want.as_str() {
                return Err(format!("account {v}, want state {want}"));
            }
        }
        if e.unchanged && j.last_entity > self.mark {
            return Err(format!(
                "journal moved past {}: last entity entry {}",
                self.mark, j.last_entity
            ));
        }
        for l in &e.no_requests {
            let now = self.target.requests(l).map_err(|s| s.0)?;
            let then = self.marked.get(l).copied().unwrap_or(0);
            if now != then {
                return Err(format!(
                    "{} {l} request(s) since the previous expect",
                    now - then
                ));
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Journal {
    head: u64,
    last_entity: u64,
    first: BTreeMap<String, u64>,
}

/// Today (UTC) plus `days`.
fn day(days: i64) -> Date {
    Timestamp::now()
        .to_zoned(TimeZone::UTC)
        .date()
        .checked_add(Span::new().days(days))
        .expect("date")
}

/// The fake Google: events only, faults in Google's error shape.
struct GoogleTarget {
    g: FakeGoogle,
}

impl GoogleTarget {
    async fn start() -> Self {
        Self {
            g: FakeGoogle::start(google::Config::default())
                .await
                .expect("fake Google"),
        }
    }
}

#[async_trait]
impl Target for GoogleTarget {
    fn name(&self) -> &str {
        "google"
    }

    fn kinds(&self) -> &[Kind] {
        &[Kind::Event]
    }

    fn config(&self, listen: SocketAddr, data: &Path) -> Value {
        harness::config(listen, data, &self.g)
    }

    async fn connect(&mut self, api: &Api, admin: &str) -> Result<String, Skip> {
        Ok(api.connect(admin).await)
    }

    async fn put(
        &mut self,
        handle: Option<&str>,
        collection: Option<&str>,
        object: &Object,
        _refs: &BTreeMap<String, String>,
    ) -> Result<String, Skip> {
        let Object::Event {
            title,
            day: d,
            hour,
            minutes,
            all_day,
            unknown,
        } = object
        else {
            return Err(Skip::because(format!("google has no {:?}", object.kind())));
        };
        let date = day(*d);
        let (start, end) = if *all_day {
            (
                json!({ "date": date.to_string() }),
                json!({ "date": day(d + 1).to_string() }),
            )
        } else {
            let at = date
                .at(i8::try_from(*hour).expect("hour"), 0, 0, 0)
                .to_zoned(TimeZone::UTC)
                .expect("time")
                .timestamp();
            let to = at + SignedDuration::from_mins(i64::from(*minutes));
            (
                json!({ "dateTime": at.to_string(), "timeZone": "UTC" }),
                json!({ "dateTime": to.to_string(), "timeZone": "UTC" }),
            )
        };
        let mut ev = json!({ "summary": title, "start": start, "end": end });
        if let Some(u) = unknown {
            ev["extendedProperties"] = json!({ "private": { "x-musubi-unknown": u } });
        }
        let cal = match handle {
            Some(h) => {
                let (c, id) = h.split_once('\n').expect("handle");
                ev["id"] = json!(id);
                c.to_owned()
            }
            None => collection.map_or_else(|| self.g.primary(), str::to_owned),
        };
        let id = self.g.put_event(&cal, ev);
        Ok(format!("{cal}\n{id}"))
    }

    async fn delete(&mut self, handle: &str) -> Result<(), Skip> {
        let (cal, id) = handle.split_once('\n').expect("handle");
        self.g.delete_event(cal, id);
        Ok(())
    }

    async fn expire_cursor(&mut self, collection: Option<&str>) -> Result<(), Skip> {
        let cals = collection.map_or_else(|| self.g.calendars(), |c| vec![c.to_owned()]);
        for calendar in cals {
            self.g.apply(&Change::ExpireSyncTokens { calendar });
        }
        Ok(())
    }

    async fn inject(&mut self, fault: &Fault, times: u32) -> Result<(), Skip> {
        match fault {
            Fault::RateLimited {
                status,
                retry_after,
            } => {
                let f = match status {
                    429 => google::Fault::rate_limited(*retry_after),
                    503 => google::Fault {
                        retry_after: *retry_after,
                        ..google::Fault::backend_error()
                    },
                    s => {
                        return Err(Skip::because(format!(
                            "google has no rate limit status {s}"
                        )));
                    }
                };
                self.g.fail(Endpoint::Any, f, times);
            }
            Fault::Slow { ms } => self.g.set_latency(Duration::from_millis(*ms)),
            Fault::Unauthorized => {
                return Err(Skip::because(
                    "the fake Google's 401 and reconnect come with K6",
                ));
            }
        }
        Ok(())
    }

    async fn add_collection(&mut self, name: &str, _kinds: &[Kind]) -> Result<String, Skip> {
        self.g.apply(&Change::AddCalendar {
            entry: json!({ "id": name, "summary": name }),
        });
        Ok(name.to_owned())
    }

    async fn remove_collection(&mut self, handle: &str) -> Result<(), Skip> {
        self.g.apply(&Change::RemoveCalendar {
            id: handle.to_owned(),
        });
        Ok(())
    }

    fn requests(&self, label: &str) -> Result<usize, Skip> {
        Ok(self
            .g
            .requests()
            .iter()
            .filter(|r| r.starts_with(label))
            .count())
    }
}

/// Chapter 8.3: Radicale starts with the user, a calendar and an address
/// book, takes objects, answers `401` after a password change and leaves no
/// process, port or directory behind.
#[tokio::test(flavor = "multi_thread")]
async fn radicale_harness_creates_its_collections_and_leaves_nothing() {
    let r = Radicale::start().await;
    let (s, body) = r.request("PROPFIND", &r.calendar(), Some("0"), "").await;
    assert_eq!(s, 207, "{body}");
    assert!(
        body.contains("calendar") && body.contains("urn:ietf:params:xml:ns:caldav"),
        "{body}"
    );
    let (s, body) = r.request("PROPFIND", &r.addressbook(), Some("0"), "").await;
    assert_eq!(s, 207, "{body}");
    assert!(
        body.contains("addressbook") && body.contains("urn:ietf:params:xml:ns:carddav"),
        "{body}"
    );

    let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//test//EN\r\nBEGIN:VEVENT\r\n\
               UID:a@musubi.test\r\nDTSTAMP:20261101T000000Z\r\nDTSTART:20261201T100000Z\r\n\
               DTEND:20261201T110000Z\r\nSUMMARY:hello\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let path = r.put(&r.calendar(), "a.ics", ics).await;
    let (s, body) = r.request("GET", &path, None, "").await;
    assert!(s == 200 && body.contains("SUMMARY:hello"), "{s} {body}");

    // The old password stops working at once.
    let http = reqwest::Client::new();
    let old = |http: &reqwest::Client| {
        http.request(
            reqwest::Method::from_bytes(b"PROPFIND").expect("m"),
            format!("{}{}", r.url().trim_end_matches('/'), r.calendar()),
        )
        .basic_auth(radicale::USER, Some(radicale::PASSWORD))
        .header("depth", "0")
        .send()
    };
    assert_eq!(old(&http).await.expect("send").status(), 207);
    r.set_password("changed");
    assert_eq!(old(&http).await.expect("send").status(), 401);
    assert_eq!(
        r.request("PROPFIND", &r.calendar(), Some("0"), "").await.0,
        207
    );
    r.delete(&path).await;
    assert_eq!(r.request("GET", &path, None, "").await.0, 404);

    let dir = r.dir().to_owned();
    let pid = r.pid();
    let addr: SocketAddr = r.url()["http://".len()..]
        .trim_end_matches('/')
        .parse()
        .expect("addr");
    assert!(dir.exists());
    drop(r);
    assert!(!dir.exists(), "{} left behind", dir.display());
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "process {pid} left behind"
    );
    assert!(
        std::net::TcpStream::connect(addr).is_err(),
        "{addr} still listening"
    );
}
