//! The conformance runner (plan, chapter 8.1) and the Radicale harness
//! (chapter 8.3).
//!
//! Every scenario in `conformance/*.json` runs against every target through a
//! `musubi-syncd` process: the fake Google, the fake DAV server in profiles
//! `generic` and `icloud`, and Radicale. Targets run in parallel, scenarios
//! one after another; each scenario gets a fresh target and node. The run
//! prints the scenario × target matrix and fails when a scenario fails or
//! when the matrix differs from [`TABLE`] (the plan's table): a skip that
//! appears or disappears is a failure, not a line in the log.
//!
//! The binary comes from `MUSUBI_SYNCD`, else from next to this test's `deps`
//! directory (`cargo build -p musubi-syncd` puts it there). Radicale needs
//! `MUSUBI_RADICALE` (see `musubi_testkit::radicale`).
//! `MUSUBI_CONFORMANCE_ONLY=<text>` runs only the `scenario@target` pairs
//! containing the text (and skips the table check). The CI job `conformance`
//! runs this file:
//!
//! ```text
//! cargo build -p musubi-syncd --locked
//! MUSUBI_RADICALE=/tmp/radicale-venv cargo test -p musubi-testkit --test conformance --locked
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::response::IntoResponse;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Span, Timestamp};
use musubi_testkit::conformance::{Expect, Fault, Kind, Object, Scenario, Skip, Step, Target};
use musubi_testkit::dav::{self, FakeDav, Host, Req};
use musubi_testkit::google::{self, Change, Endpoint, FakeGoogle};
use musubi_testkit::harness::{self, Api, Daemon, free_addr};
use musubi_testkit::radicale::{self, Radicale};
use serde_json::{Value, json};

/// Target names, in report order.
const TARGETS: &[&str] = &["google", "dav-generic", "dav-icloud", "radicale"];

/// The plan's table (chapter 8.1): per scenario one cell per target, in
/// [`TARGETS`] order. `y` runs and passes, `s` is skipped (the reason is
/// printed), `-` does not apply (the scenario's `only`).
const TABLE: &[(&str, &str)] = &[
    ("contact_group", "-yyy"),
    ("discovery", "yyyy"),
    ("expired_cursor", "yyyy"),
    ("increment", "yyyy"),
    ("initial_read", "yyyy"),
    ("kill_mid_page", "yyy-"),
    ("mixed_calendar", "-yyy"),
    ("pagination", "yyyy"),
    ("rate_limited", "yyys"),
    ("reauth", "yyyy"),
    ("unauthorized_kind", "y---"),
    ("unchanged", "yyyy"),
    ("unknown_fields", "yyyy"),
];

/// The node's `sync` settings in every run: fast polling and rediscovery so
/// a scenario takes seconds, and pages of 20 so 60 objects span pages.
fn sync_settings() -> Value {
    let fast = json!({ "min": 0.5, "max": 2 });
    json!({ "active": fast, "normal": fast, "sleeping": fast, "rediscover_every": 2, "page_size": 20 })
}

async fn target(name: &str) -> Box<dyn Target> {
    match name {
        "google" => Box::new(GoogleTarget::start().await),
        "dav-generic" => Box::new(DavTarget::start(dav::Profile::Generic).await),
        "dav-icloud" => Box::new(DavTarget::start(dav::Profile::Icloud).await),
        "radicale" => Box::new(RadicaleTarget::start().await),
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

fn scenarios() -> Vec<Scenario> {
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
            s
        })
        .collect()
}

enum Outcome {
    Pass,
    Skip(String),
    Fail(String),
    /// Not in the scenario's `only`.
    NotApplicable,
    /// Left out by `MUSUBI_CONFORMANCE_ONLY`.
    Filtered,
}

impl Outcome {
    fn cell(&self) -> char {
        match self {
            Outcome::Pass => 'y',
            Outcome::Skip(_) => 's',
            Outcome::Fail(_) => 'F',
            Outcome::NotApplicable => '-',
            Outcome::Filtered => '.',
        }
    }
}

#[test]
fn scenarios_pass_on_every_target_as_the_plan_table_says() {
    let bin = syncd();
    let all = scenarios();
    let only = std::env::var("MUSUBI_CONFORMANCE_ONLY").ok();
    let columns: Vec<Vec<Outcome>> = std::thread::scope(|sc| {
        let threads: Vec<_> = TARGETS
            .iter()
            .map(|&t| {
                let (bin, all, only) = (&bin, &all, only.as_deref());
                sc.spawn(move || all.iter().map(|s| run_one(bin, s, t, only)).collect())
            })
            .collect();
        threads
            .into_iter()
            .map(|h| h.join().expect("target thread"))
            .collect()
    });

    let mut matrix = format!("| scenario | {} |\n|---|", TARGETS.join(" | "));
    matrix += &"---|".repeat(TARGETS.len());
    let mut cells = BTreeMap::new();
    let mut failed = Vec::new();
    for (i, s) in all.iter().enumerate() {
        let row: String = columns.iter().map(|c| c[i].cell()).collect();
        let shown: Vec<&str> = row
            .chars()
            .map(|c| match c {
                'y' => "pass",
                's' => "skipped",
                '-' => "—",
                '.' => "filtered",
                _ => "FAILED",
            })
            .collect();
        matrix += &format!("\n| {} | {} |", s.name, shown.join(" | "));
        for (t, c) in TARGETS.iter().zip(&columns) {
            match &c[i] {
                Outcome::Skip(r) => eprintln!("skipped {} @ {t}: {r}", s.name),
                Outcome::Fail(r) => failed.push(format!("{} @ {t}: {r}", s.name)),
                _ => {}
            }
        }
        cells.insert(s.name.clone(), row);
    }
    eprintln!("\n{matrix}\n");
    for f in &failed {
        eprintln!("FAILED {f}\n");
    }
    assert!(failed.is_empty(), "{} scenario run(s) failed", failed.len());
    if only.is_none() {
        let want: BTreeMap<String, String> = TABLE
            .iter()
            .map(|(n, r)| ((*n).to_owned(), (*r).to_owned()))
            .collect();
        assert_eq!(
            cells, want,
            "the runs differ from TABLE (plan 8.1): a scenario or a skip was added or lost"
        );
    }
}

/// One scenario on one target, isolated: its own runtime, node and target;
/// a panic fails this run only.
fn run_one(bin: &Path, s: &Scenario, name: &str, only: Option<&str>) -> Outcome {
    if s.only
        .as_ref()
        .is_some_and(|o| !o.iter().any(|t| t == name))
    {
        return Outcome::NotApplicable;
    }
    if only.is_some_and(|f| !format!("{}@{name}", s.name).contains(f)) {
        return Outcome::Filtered;
    }
    let started = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let mut t = target(name).await;
            let Some(s) = restrict(s, t.kinds()) else {
                return Err(Stop::Skip(format!("{name} serves none of {:?}", s.kinds)));
            };
            run(bin, &s, t.as_mut()).await
        })
    }));
    let outcome = match result {
        Ok(Ok(())) => Outcome::Pass,
        Ok(Err(Stop::Skip(r))) => Outcome::Skip(r),
        Ok(Err(Stop::Fail(r))) => Outcome::Fail(r),
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| p.downcast_ref::<&str>().copied())
                .unwrap_or("?");
            Outcome::Fail(format!("panicked: {msg}"))
        }
    };
    eprintln!(
        "conformance: {} @ {name}: {} in {:.1?}",
        s.name,
        outcome.cell(),
        started.elapsed()
    );
    outcome
}

/// The scenario over the kinds a target serves: `put`s of other kinds and
/// every later use of their refs dropped, `copies` expanded. `None` when
/// the target serves none of the scenario's kinds.
fn restrict(s: &Scenario, kinds: &[Kind]) -> Option<Scenario> {
    let served: Vec<Kind> = s
        .kinds
        .iter()
        .copied()
        .filter(|k| kinds.contains(k))
        .collect();
    if served.is_empty() {
        return None;
    }
    let mut dropped = BTreeSet::new();
    let mut steps = Vec::new();
    for step in s.steps.iter().cloned() {
        match step {
            Step::Put {
                reference, object, ..
            } if !kinds.contains(&object.kind()) => {
                dropped.insert(reference);
            }
            Step::Put {
                reference,
                collection,
                mut object,
                copies,
            } => {
                if let Object::Group { members, .. } = &mut object {
                    members.retain(|m| !dropped.contains(m));
                }
                let Some(n) = copies else {
                    steps.push(Step::Put {
                        reference,
                        collection,
                        object,
                        copies: None,
                    });
                    continue;
                };
                for i in 1..=n {
                    let mut o = object.clone();
                    *o.label_mut() = format!("{} {i}", o.label());
                    steps.push(Step::Put {
                        reference: format!("{reference}.{i}"),
                        collection: collection.clone(),
                        object: o,
                        copies: None,
                    });
                }
            }
            Step::Delete { reference } if dropped.contains(&reference) => {}
            Step::Expect(mut e) => {
                e.window_first.retain(|r| !dropped.contains(r));
                if e.raw_contains
                    .as_ref()
                    .is_some_and(|r| dropped.contains(&r.reference))
                {
                    e.raw_contains = None;
                }
                e.missing.retain(|k| kinds.contains(k));
                steps.push(Step::Expect(e));
            }
            other => steps.push(other),
        }
    }
    Some(Scenario {
        kinds: served,
        steps,
        ..s.clone()
    })
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
    /// Ref → the named collection it was put in.
    placed: BTreeMap<String, String>,
    /// Collection name → handle.
    collections: BTreeMap<String, String>,
    /// Journal head and request counts at the previous `expect`.
    mark: u64,
    marked: BTreeMap<String, usize>,
}

async fn run(bin: &Path, s: &Scenario, target: &mut dyn Target) -> Result<(), Stop> {
    let counted: BTreeSet<String> = s
        .steps
        .iter()
        .filter_map(|st| match st {
            Step::Expect(e) => Some(e.no_requests.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    // A target that cannot count a label skips now, not after an expect's timeout.
    for l in &counted {
        target.requests(l)?;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let listen = free_addr();
    let data = dir.path().join("data");
    let config = dir.path().join("config.json");
    let mut c = target.config(listen, &data);
    c["sync"] = sync_settings();
    std::fs::write(&config, c.to_string()).expect("config");
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
        placed: BTreeMap::new(),
        collections: BTreeMap::new(),
        mark: 0,
        marked: BTreeMap::new(),
    };
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
                ..
            } => {
                let col = self.collection(collection.as_deref())?;
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
                if let Some(c) = collection {
                    self.placed.entry(reference.clone()).or_insert(c.clone());
                }
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
                let col = self.collection(collection.as_deref())?;
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
                self.target.remove_collection(&h).await?;
                let placed = &self.placed;
                self.live.retain(|r, _| placed.get(r) != Some(name));
            }
            Step::Kill => {
                self.daemon.kill();
                self.daemon = Daemon::serve(&self.bin, &self.config, &[]);
            }
            Step::Wait { ms } => tokio::time::sleep(Duration::from_millis(*ms)).await,
            Step::Expect(e) => {
                let refs = e
                    .window_first
                    .iter()
                    .chain(e.raw_contains.as_ref().map(|r| &r.reference));
                if let Some(r) = refs.into_iter().find(|r| !self.labels.contains_key(*r)) {
                    return Err(Stop::Fail(format!("no ref {r}")));
                }
                self.expect(e).await.map_err(Stop::Fail)?;
                self.remark(counted).await;
            }
        }
        Ok(())
    }
    /// The handle of a named collection; `None` stays `None`, an unknown name fails.
    fn collection(&self, name: Option<&str>) -> Result<Option<String>, Stop> {
        name.map(|c| {
            self.collections
                .get(c)
                .cloned()
                .ok_or_else(|| Stop::Fail(format!("no collection {c}")))
        })
        .transpose()
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
                    // The node keeps the marker on a kind it may not read (plan 7.4).
                    let unreadable = !e.missing.is_empty()
                        && c["error"]["message"]
                            .as_str()
                            .is_some_and(|m| m.contains("missing_scope"));
                    if c["gone"] != true
                        && !unreadable
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
        if !e.missing.is_empty() {
            let a = self.account.as_ref().ok_or("not connected")?;
            let v = self.get(&format!("/v1/accounts/{a}")).await?;
            let mut got: Vec<&str> = v["missing"]
                .as_array()
                .map(|m| m.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut want: Vec<&str> = e.missing.iter().map(|k| k.api()).collect();
            got.sort_unstable();
            want.sort_unstable();
            if got != want {
                return Err(format!("account missing {got:?}, want {want:?}"));
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

/// Start and end of a timed event.
fn timed(d: i64, hour: u8, minutes: u32) -> (Timestamp, Timestamp) {
    let at = day(d)
        .at(i8::try_from(hour).expect("hour"), 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .expect("time")
        .timestamp();
    (at, at + SignedDuration::from_mins(i64::from(minutes)))
}

/// A UID for the `n`th object a DAV target creates; also its file stem.
fn uid(n: u64) -> String {
    format!("6d757375-6269-4000-8000-{n:012x}")
}

/// An object as iCalendar or vCard. `members` are the member UIDs of a
/// group; `apple` forces Apple's vCard 3 group form (iCloud).
fn render(uid: &str, object: &Object, members: &[String], apple: bool) -> String {
    let mut l = Vec::new();
    match object {
        Object::Event {
            title,
            day: d,
            hour,
            minutes,
            all_day,
            unknown,
        } => {
            l.extend([
                "BEGIN:VCALENDAR".to_owned(),
                "VERSION:2.0".into(),
                "PRODID:-//musubi//conformance//EN".into(),
                "BEGIN:VEVENT".into(),
                format!("UID:{uid}"),
                "DTSTAMP:20260101T000000Z".into(),
            ]);
            if *all_day {
                l.push(format!("DTSTART;VALUE=DATE:{}", day(*d).strftime("%Y%m%d")));
                l.push(format!(
                    "DTEND;VALUE=DATE:{}",
                    day(d + 1).strftime("%Y%m%d")
                ));
            } else {
                let (at, to) = timed(*d, *hour, *minutes);
                l.push(format!("DTSTART:{}", at.strftime("%Y%m%dT%H%M%SZ")));
                l.push(format!("DTEND:{}", to.strftime("%Y%m%dT%H%M%SZ")));
            }
            l.push(format!("SUMMARY:{title}"));
            l.extend(unknown.iter().map(|u| format!("X-MUSUBI-UNKNOWN:{u}")));
            l.extend(["END:VEVENT".into(), "END:VCALENDAR".into()]);
        }
        Object::Task {
            title,
            due_day,
            done,
            unknown,
        } => {
            l.extend([
                "BEGIN:VCALENDAR".to_owned(),
                "VERSION:2.0".into(),
                "PRODID:-//musubi//conformance//EN".into(),
                "BEGIN:VTODO".into(),
                format!("UID:{uid}"),
                "DTSTAMP:20260101T000000Z".into(),
                format!("SUMMARY:{title}"),
            ]);
            l.extend(due_day.map(|d| format!("DUE;VALUE=DATE:{}", day(d).strftime("%Y%m%d"))));
            l.push(format!(
                "STATUS:{}",
                if *done { "COMPLETED" } else { "NEEDS-ACTION" }
            ));
            l.extend(unknown.iter().map(|u| format!("X-MUSUBI-UNKNOWN:{u}")));
            l.extend(["END:VTODO".into(), "END:VCALENDAR".into()]);
        }
        Object::Contact {
            name,
            email,
            unknown,
        } => {
            l.extend([
                "BEGIN:VCARD".to_owned(),
                "VERSION:3.0".into(),
                format!("UID:{uid}"),
                format!("FN:{name}"),
                format!("N:{name};;;;"),
            ]);
            l.extend(email.iter().map(|e| format!("EMAIL;TYPE=INTERNET:{e}")));
            l.extend(unknown.iter().map(|u| format!("X-MUSUBI-UNKNOWN:{u}")));
            l.push("END:VCARD".into());
        }
        Object::Group { name, apple: a, .. } => {
            let apple = apple || *a;
            l.extend([
                "BEGIN:VCARD".to_owned(),
                format!("VERSION:{}", if apple { "3.0" } else { "4.0" }),
                format!("UID:{uid}"),
                format!("FN:{name}"),
            ]);
            if apple {
                l.push(format!("N:{name};;;;"));
                l.push("X-ADDRESSBOOKSERVER-KIND:group".into());
                l.extend(
                    members
                        .iter()
                        .map(|m| format!("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:{m}")),
                );
            } else {
                l.push("KIND:group".into());
                l.extend(members.iter().map(|m| format!("MEMBER:urn:uuid:{m}")));
            }
            l.push("END:VCARD".into());
        }
    }
    l.join("\r\n") + "\r\n"
}

/// The member UIDs of a group: the file stems of the members' handles.
fn member_uids(object: &Object, refs: &BTreeMap<String, String>) -> Vec<String> {
    let Object::Group { members, .. } = object else {
        return Vec::new();
    };
    members
        .iter()
        .map(|m| {
            let h = refs.get(m).unwrap_or_else(|| panic!("no member ref {m}"));
            let file = h.rsplit('/').next().unwrap_or(h);
            file.split('.').next().unwrap_or(file).to_owned()
        })
        .collect()
}

/// `collection/name` of an object path.
fn split_path(path: &str) -> (&str, &str) {
    let i = path.rfind('/').expect("object path") + 1;
    (&path[..i], &path[i..])
}

fn ext(object: &Object) -> &'static str {
    match object.kind() {
        Kind::Event | Kind::Task => "ics",
        Kind::Contact | Kind::Group => "vcf",
    }
}

/// `POST /v1/accounts/dav`; the account id.
async fn connect_dav(api: &Api, admin: &str, body: Value) -> String {
    let (s, v) = api
        .call("POST", "/v1/accounts/dav", Some(admin), Some(body))
        .await
        .expect("connect");
    assert!((200..300).contains(&s), "POST /v1/accounts/dav: {s} {v}");
    v["id"].as_str().expect("id").to_owned()
}

/// The fake Google: events, tasks and contacts; faults in Google's error
/// shape. Handles: `e\n<calendar>\n<id>`, `t\n<list>\n<id>`,
/// `c\n<resourceName>`.
struct GoogleTarget {
    g: FakeGoogle,
    tasks: String,
}

impl GoogleTarget {
    async fn start() -> Self {
        let g = FakeGoogle::start(google::Config::default())
            .await
            .expect("fake Google");
        let tasks = g.put_task_list("My Tasks");
        Self { g, tasks }
    }
}

#[async_trait]
impl Target for GoogleTarget {
    fn name(&self) -> &str {
        "google"
    }

    fn kinds(&self) -> &[Kind] {
        &[Kind::Event, Kind::Task, Kind::Contact]
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
        let old: Vec<&str> = handle.map(|h| h.split('\n').collect()).unwrap_or_default();
        match object {
            Object::Event {
                title,
                day: d,
                hour,
                minutes,
                all_day,
                unknown,
            } => {
                let (start, end) = if *all_day {
                    (
                        json!({ "date": day(*d).to_string() }),
                        json!({ "date": day(d + 1).to_string() }),
                    )
                } else {
                    let (at, to) = timed(*d, *hour, *minutes);
                    (
                        json!({ "dateTime": at.to_string(), "timeZone": "UTC" }),
                        json!({ "dateTime": to.to_string(), "timeZone": "UTC" }),
                    )
                };
                let mut ev = json!({ "summary": title, "start": start, "end": end });
                if let Some(u) = unknown {
                    ev["extendedProperties"] = json!({ "private": { "x-musubi-unknown": u } });
                }
                let cal = match old.as_slice() {
                    [_, c, id] => {
                        ev["id"] = json!(id);
                        (*c).to_owned()
                    }
                    _ => collection.map_or_else(|| self.g.primary(), str::to_owned),
                };
                let id = self.g.put_event(&cal, ev);
                Ok(format!("e\n{cal}\n{id}"))
            }
            Object::Task {
                title,
                due_day,
                done,
                unknown,
            } => {
                let mut t = json!({
                    "title": title,
                    "status": if *done { "completed" } else { "needsAction" },
                });
                if let Some(d) = due_day {
                    t["due"] = json!(format!("{}T00:00:00.000Z", day(*d)));
                }
                if let Some(u) = unknown {
                    t["x-musubi-unknown"] = json!(u);
                }
                let list = match old.as_slice() {
                    [_, l, id] => {
                        t["id"] = json!(id);
                        (*l).to_owned()
                    }
                    _ => collection.unwrap_or(&self.tasks).to_owned(),
                };
                let id = self.g.put_task(&list, t);
                Ok(format!("t\n{list}\n{id}"))
            }
            Object::Contact {
                name,
                email,
                unknown,
            } => {
                let mut p = json!({ "names": [{ "displayName": name, "givenName": name }] });
                if let Some(e) = email {
                    p["emailAddresses"] = json!([{ "value": e }]);
                }
                // The fake masks fields by `personFields`; client data is one of them.
                if let Some(u) = unknown {
                    p["clientData"] = json!([{ "key": "x-musubi-unknown", "value": u }]);
                }
                if let [_, rn] = old.as_slice() {
                    p["resourceName"] = json!(rn);
                }
                Ok(format!("c\n{}", self.g.put_contact(p)))
            }
            Object::Group { .. } => Err(Skip::because("google serves no contact groups")),
        }
    }

    async fn delete(&mut self, handle: &str) -> Result<(), Skip> {
        match handle.split('\n').collect::<Vec<_>>().as_slice() {
            ["e", cal, id] => self.g.delete_event(cal, id),
            ["t", list, id] => self.g.delete_task(list, id),
            ["c", rn] => self.g.delete_contact(rn),
            other => panic!("bad handle {other:?}"),
        }
        Ok(())
    }

    async fn expire_cursor(&mut self, collection: Option<&str>) -> Result<(), Skip> {
        let cals = collection.map_or_else(|| self.g.calendars(), |c| vec![c.to_owned()]);
        for calendar in cals {
            self.g.apply(&Change::ExpireSyncTokens { calendar });
        }
        if collection.is_none() {
            self.g.expire_people_sync_tokens();
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
                self.g.revoke_refresh_token(&self.g.refresh_token());
                self.g.expire_access_tokens();
            }
            Fault::UnauthorizedKind { kind: Kind::Task } => {
                self.g.fail(
                    Endpoint::TaskLists,
                    google::Fault::scope_insufficient(),
                    u32::MAX,
                );
                self.g.fail(
                    Endpoint::TasksList,
                    google::Fault::scope_insufficient(),
                    u32::MAX,
                );
            }
            Fault::UnauthorizedKind {
                kind: Kind::Contact,
            } => self.g.fail(
                Endpoint::PeopleConnections,
                google::Fault::insufficient_permissions(),
                u32::MAX,
            ),
            Fault::UnauthorizedKind { kind } => {
                return Err(Skip::because(format!("google has no scope for {kind:?}")));
            }
        }
        Ok(())
    }

    async fn reconnect(&mut self, api: &Api, admin: &str) -> Result<(), Skip> {
        // The user consents again: the sign-in hands out a new refresh token
        // and the node puts it on the existing account.
        self.g.grant_refresh_token("1//fake-refresh-again");
        api.connect(admin).await;
        Ok(())
    }

    async fn add_collection(&mut self, name: &str, kinds: &[Kind]) -> Result<String, Skip> {
        if kinds.contains(&Kind::Event) {
            self.g.apply(&Change::AddCalendar {
                entry: json!({ "id": name, "summary": name }),
            });
            Ok(name.to_owned())
        } else if kinds.contains(&Kind::Task) {
            Ok(self.g.put_task_list(name))
        } else {
            Err(Skip::because("google has no contact collections to add"))
        }
    }

    async fn remove_collection(&mut self, handle: &str) -> Result<(), Skip> {
        if !self.g.calendars().iter().any(|c| c == handle) {
            return Err(Skip::because("the fake Google removes only calendars"));
        }
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

/// The fake DAV server, profile `generic` (one host, a calendar, a task
/// calendar, an address book) or `icloud` (T11's hosts and shapes; no
/// tasks, vCard 3 only). Handles are object and collection paths.
struct DavTarget {
    d: FakeDav,
    /// Only for the node configuration, which needs a `google` section.
    g: FakeGoogle,
    icloud: bool,
    /// Default collection per kind.
    defaults: BTreeMap<Kind, String>,
    collections: Vec<String>,
    password: String,
    account: Option<String>,
    next: u64,
}

impl DavTarget {
    async fn start(profile: dav::Profile) -> Self {
        let d = FakeDav::start(dav::Config {
            profile,
            ..Default::default()
        })
        .await
        .expect("fake DAV");
        let icloud = profile == dav::Profile::Icloud;
        let cal = d.add_collection(dav::Kind::Calendar(&["VEVENT"]), "calendar", "Calendar");
        let book = d.add_collection(dav::Kind::AddressBook, "contacts", "Contacts");
        let mut defaults = BTreeMap::from([
            (Kind::Event, cal.clone()),
            (Kind::Contact, book.clone()),
            (Kind::Group, book.clone()),
        ]);
        let mut collections = vec![cal, book];
        if !icloud {
            let tasks = d.add_collection(dav::Kind::Calendar(&["VTODO"]), "tasks", "Tasks");
            defaults.insert(Kind::Task, tasks.clone());
            collections.push(tasks);
        }
        Self {
            d,
            g: FakeGoogle::start(google::Config::default())
                .await
                .expect("fake Google"),
            icloud,
            defaults,
            collections,
            password: dav::Config::default().password,
            account: None,
            next: 0,
        }
    }

    fn body(&self) -> Value {
        let mut b = json!({ "username": "alice", "password": self.password });
        if self.icloud {
            b["profile"] = json!("icloud");
        } else {
            b["profile"] = json!("generic");
            b["server_url"] = json!(self.d.server_url());
        }
        if let Some(a) = &self.account {
            b["account"] = json!(a);
        }
        b
    }
}

#[async_trait]
impl Target for DavTarget {
    fn name(&self) -> &str {
        if self.icloud {
            "dav-icloud"
        } else {
            "dav-generic"
        }
    }

    fn kinds(&self) -> &[Kind] {
        if self.icloud {
            &[Kind::Event, Kind::Contact, Kind::Group]
        } else {
            &[Kind::Event, Kind::Task, Kind::Contact, Kind::Group]
        }
    }

    fn config(&self, listen: SocketAddr, data: &Path) -> Value {
        let mut c = harness::config(listen, data, &self.g);
        c["dav"] = json!({ "allow_private": true });
        if self.icloud {
            c["dav"]["icloud_test_entries"] = json!({
                "caldav": format!("{}/", self.d.url(Host::Caldav)),
                "carddav": format!("{}/", self.d.url(Host::Carddav)),
                "allow_origins": [self.d.url(Host::CaldavPartition), self.d.url(Host::CarddavPartition)],
            });
        }
        c
    }

    async fn connect(&mut self, api: &Api, admin: &str) -> Result<String, Skip> {
        let id = connect_dav(api, admin, self.body()).await;
        self.account = Some(id.clone());
        Ok(id)
    }

    async fn put(
        &mut self,
        handle: Option<&str>,
        collection: Option<&str>,
        object: &Object,
        refs: &BTreeMap<String, String>,
    ) -> Result<String, Skip> {
        let (col, name) = match handle {
            Some(h) => {
                let (c, n) = split_path(h);
                (c.to_owned(), n.to_owned())
            }
            None => {
                self.next += 1;
                let col =
                    collection.map_or_else(|| self.defaults[&object.kind()].clone(), str::to_owned);
                (col, format!("{}.{}", uid(self.next), ext(object)))
            }
        };
        let u = name.split('.').next().unwrap_or_default().to_owned();
        let body = render(&u, object, &member_uids(object, refs), self.icloud);
        Ok(self.d.put(&col, &name, &body))
    }

    async fn delete(&mut self, handle: &str) -> Result<(), Skip> {
        self.d.delete(handle);
        Ok(())
    }

    async fn expire_cursor(&mut self, collection: Option<&str>) -> Result<(), Skip> {
        for c in collection.map_or_else(|| self.collections.clone(), |c| vec![c.to_owned()]) {
            self.d.expire_sync_tokens(&c);
        }
        Ok(())
    }

    async fn inject(&mut self, fault: &Fault, times: u32) -> Result<(), Skip> {
        match fault {
            Fault::RateLimited {
                status,
                retry_after,
            } => self.d.fail(
                Req::Any,
                dav::Fault::Status {
                    status: *status,
                    retry_after: *retry_after,
                },
                times,
            ),
            Fault::Unauthorized => {
                self.password = "changed".into();
                self.d.set_password(&self.password);
            }
            Fault::Slow { ms } => self.d.set_latency(Duration::from_millis(*ms)),
            Fault::UnauthorizedKind { .. } => {
                return Err(Skip::because("DAV has no per-kind authorization"));
            }
        }
        Ok(())
    }

    async fn reconnect(&mut self, api: &Api, admin: &str) -> Result<(), Skip> {
        connect_dav(api, admin, self.body()).await;
        Ok(())
    }

    async fn add_collection(&mut self, name: &str, kinds: &[Kind]) -> Result<String, Skip> {
        let kind = match (kinds.contains(&Kind::Event), kinds.contains(&Kind::Task)) {
            (true, true) => dav::Kind::Calendar(&["VEVENT", "VTODO"]),
            (true, false) => dav::Kind::Calendar(&["VEVENT"]),
            (false, true) => dav::Kind::Calendar(&["VTODO"]),
            (false, false) => dav::Kind::AddressBook,
        };
        let path = self.d.add_collection(kind, name, name);
        self.collections.push(path.clone());
        Ok(path)
    }

    async fn remove_collection(&mut self, handle: &str) -> Result<(), Skip> {
        self.d.remove_collection(handle);
        self.collections.retain(|c| c != handle);
        Ok(())
    }

    fn requests(&self, label: &str) -> Result<usize, Skip> {
        Ok(self.d.count(label))
    }
}

/// Radicale (chapter 8.3) behind a counting reverse proxy in this process,
/// so `no_requests` sees what the node sent. Radicale has one calendar for
/// events and tasks and one address book. It cannot inject faults.
struct RadicaleTarget {
    r: Radicale,
    g: FakeGoogle,
    proxy: String,
    log: Arc<Mutex<Vec<String>>>,
    serve: tokio::task::JoinHandle<()>,
    password: String,
    account: Option<String>,
    next: u64,
}

impl Drop for RadicaleTarget {
    fn drop(&mut self) {
        self.serve.abort();
    }
}

#[derive(Clone)]
struct Proxy {
    upstream: String,
    http: reqwest::Client,
    log: Arc<Mutex<Vec<String>>>,
}

/// Forwards one request to Radicale and logs its label (`METHOD`, or
/// `REPORT <root element>`).
async fn forward(
    axum::extract::State(p): axum::extract::State<Proxy>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::http::header;
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let report = (parts.method.as_str() == "REPORT")
        .then(|| dav::parse(std::str::from_utf8(&body).unwrap_or_default()).ok())
        .flatten();
    let label = match report {
        Some(el) => format!("REPORT {}", el.name),
        None => parts.method.to_string(),
    };
    p.log.lock().expect("log").push(label);
    let path = parts.uri.path_and_query().map_or("/", |q| q.as_str());
    let mut headers = parts.headers.clone();
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    let sent = p
        .http
        .request(parts.method, format!("{}{path}", p.upstream))
        .headers(headers)
        .body(body)
        .send()
        .await;
    let Ok(resp) = sent else {
        return axum::http::StatusCode::BAD_GATEWAY.into_response();
    };
    let status = resp.status();
    let mut headers = resp.headers().clone();
    headers.remove(header::TRANSFER_ENCODING);
    headers.remove(header::CONTENT_LENGTH);
    let bytes = resp.bytes().await.unwrap_or_default();
    let mut out = axum::response::Response::new(axum::body::Body::from(bytes));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

impl RadicaleTarget {
    async fn start() -> Self {
        let r = Radicale::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let proxy = Proxy {
            upstream: r.url().trim_end_matches('/').to_owned(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client"),
            log: log.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = axum::Router::new().fallback(forward).with_state(proxy);
        let serve = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            r,
            g: FakeGoogle::start(google::Config::default())
                .await
                .expect("fake Google"),
            proxy: format!("http://{addr}/"),
            log,
            serve,
            password: radicale::PASSWORD.to_owned(),
            account: None,
            next: 0,
        }
    }

    fn body(&self) -> Value {
        let mut b = json!({
            "profile": "generic",
            "server_url": self.proxy,
            "username": radicale::USER,
            "password": self.password,
        });
        if let Some(a) = &self.account {
            b["account"] = json!(a);
        }
        b
    }
}

#[async_trait]
impl Target for RadicaleTarget {
    fn name(&self) -> &str {
        "radicale"
    }

    fn kinds(&self) -> &[Kind] {
        &[Kind::Event, Kind::Task, Kind::Contact, Kind::Group]
    }

    fn config(&self, listen: SocketAddr, data: &Path) -> Value {
        let mut c = harness::config(listen, data, &self.g);
        c["dav"] = json!({ "allow_private": true });
        c
    }

    async fn connect(&mut self, api: &Api, admin: &str) -> Result<String, Skip> {
        let id = connect_dav(api, admin, self.body()).await;
        self.account = Some(id.clone());
        Ok(id)
    }

    async fn put(
        &mut self,
        handle: Option<&str>,
        collection: Option<&str>,
        object: &Object,
        refs: &BTreeMap<String, String>,
    ) -> Result<String, Skip> {
        let (col, name) = match handle {
            Some(h) => {
                let (c, n) = split_path(h);
                (c.to_owned(), n.to_owned())
            }
            None => {
                self.next += 1;
                let col = collection.map_or_else(
                    || match object.kind() {
                        Kind::Event | Kind::Task => self.r.calendar(),
                        Kind::Contact | Kind::Group => self.r.addressbook(),
                    },
                    str::to_owned,
                );
                (col, format!("{}.{}", uid(self.next), ext(object)))
            }
        };
        let u = name.split('.').next().unwrap_or_default().to_owned();
        let body = render(&u, object, &member_uids(object, refs), false);
        Ok(self.r.put(&col, &name, &body).await)
    }

    async fn delete(&mut self, handle: &str) -> Result<(), Skip> {
        self.r.delete(handle).await;
        Ok(())
    }

    async fn expire_cursor(&mut self, collection: Option<&str>) -> Result<(), Skip> {
        // Radicale keeps every token it issued under the collection's cache;
        // a token it no longer finds answers `403 valid-sync-token`.
        let root = self.r.dir().join("collections/collection-root");
        let cols: Vec<PathBuf> = match collection {
            Some(c) => vec![root.join(c.trim_matches('/'))],
            None => std::fs::read_dir(root.join(radicale::USER))
                .expect("collections")
                .map(|e| e.expect("entry").path())
                .filter(|p| p.is_dir())
                .collect(),
        };
        for c in cols {
            let tokens = c.join(".Radicale.cache/sync-token");
            if tokens.exists() {
                std::fs::remove_dir_all(&tokens).expect("remove tokens");
            }
        }
        Ok(())
    }

    async fn inject(&mut self, fault: &Fault, _times: u32) -> Result<(), Skip> {
        if *fault == Fault::Unauthorized {
            self.password = "changed".into();
            self.r.set_password(&self.password);
            return Ok(());
        }
        Err(Skip::because(format!("Radicale cannot inject {fault:?}")))
    }

    async fn reconnect(&mut self, api: &Api, admin: &str) -> Result<(), Skip> {
        connect_dav(api, admin, self.body()).await;
        Ok(())
    }

    async fn add_collection(&mut self, name: &str, kinds: &[Kind]) -> Result<String, Skip> {
        let comps: String = [(Kind::Event, "VEVENT"), (Kind::Task, "VTODO")]
            .iter()
            .filter(|(k, _)| kinds.contains(k))
            .map(|(_, c)| format!("<C:comp name=\"{c}\"/>"))
            .collect();
        let body = if comps.is_empty() {
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:mkcol xmlns:D=\"DAV:\" \
                 xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><D:set><D:prop><D:resourcetype>\
                 <D:collection/><CR:addressbook/></D:resourcetype><D:displayname>{name}\
                 </D:displayname></D:prop></D:set></D:mkcol>"
            )
        } else {
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:mkcol xmlns:D=\"DAV:\" \
                 xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:set><D:prop><D:resourcetype>\
                 <D:collection/><C:calendar/></D:resourcetype><D:displayname>{name}\
                 </D:displayname><C:supported-calendar-component-set>{comps}\
                 </C:supported-calendar-component-set></D:prop></D:set></D:mkcol>"
            )
        };
        Ok(self
            .r
            .mkcol(&format!("/{}/{name}/", radicale::USER), &body)
            .await)
    }

    async fn remove_collection(&mut self, handle: &str) -> Result<(), Skip> {
        self.r.delete(handle).await;
        Ok(())
    }

    fn requests(&self, label: &str) -> Result<usize, Skip> {
        Ok(self
            .log
            .lock()
            .expect("log")
            .iter()
            .filter(|l| *l == label)
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
