// Phase 2 engine (plan ch. 3.4-3.7, 7, 10; K5).

/// An event collection without push: polled adaptively.
fn polled(name: &str) -> RemoteCollection {
    let mut c = meta(name);
    c.push = Some(PushSupport::None);
    c
}

/// A polled collection whose listing gives stubs (CalDAV-like).
fn dav(name: &str) -> RemoteCollection {
    let mut c = polled(name);
    c.delta = Some(DeltaSupport::CtagEtag);
    c
}

fn task_list(name: &str) -> RemoteCollection {
    let mut c = RemoteCollection::new(cal(name), EntityKind::Task, name, Access::Owner);
    c.delta = Some(DeltaSupport::SyncToken);
    c.push = Some(PushSupport::None);
    c
}

fn put_task(h: &H, col: &str, key: &str, validator: &str) {
    let raw = Raw {
        media_type: musubi_model::remote::MediaType::Json,
        body: serde_json::to_vec(&json!({ "title": key })).unwrap(),
    };
    let object = musubi_model::RemoteObject::new(
        RemoteKey(key.into()),
        EntityKind::Task,
        musubi_model::Validator(validator.into()),
        raw,
    );
    h.fake.put(&cal(col), FakeObject { object, span: None });
}

/// Local wall time `days` after today (UTC date), at `time`.
fn local(h: &H, days: i64, time: &str) -> String {
    let d = h.clock_now().to_zoned(TimeZone::UTC).date() + jiff::Span::new().days(days);
    format!("{d}T{time}")
}

/// Polls until a run with nothing due; settles the deltas queued after a full read.
async fn settle(h: &H) {
    h.advance(Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
}

/// Moves the clock to the queued sync of `name` and runs; returns the gap
/// to the next one.
async fn poll_once(h: &H, name: &str) -> Duration {
    let due = h.sync_due(name).unwrap();
    let now = h.clock_now();
    if due > now {
        h.advance(Duration::try_from(due - now).unwrap());
    }
    h.engine.run_until_idle().await.unwrap();
    let ran = h.clock_now();
    Duration::try_from(h.sync_due(name).unwrap() - ran).unwrap()
}

#[tokio::test]
async fn kind_without_a_codec_is_stored_raw_only() {
    let h = H::new();
    h.fake.add_collection(task_list("tasks"));
    put_task(&h, "tasks", "t1", "v1");
    h.connect_and_sync().await;

    let store = h.store();
    let id = h.item("tasks", "t1").id;
    let item = store
        .item(
            id,
            musubi_model::store::Include {
                raw: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(item.kind, EntityKind::Task);
    assert_eq!(item.mapping_version, 0);
    assert!(item.observed.is_none());
    assert_eq!(item.raw.unwrap().body, br#"{"title":"t1"}"#);
}

#[tokio::test]
async fn stubs_fetch_only_changed_validators_in_batches() {
    let h = H::with(|c| c.page_size = std::num::NonZeroU32::new(10));
    h.fake.add_collection(dav("dav"));
    let start = h.clock_now() + SignedDuration::from_hours(24);
    let keys = ["a", "b", "c", "d", "e"];
    for k in keys {
        h.put("dav", k, "v1", &single(k, start), None);
    }
    h.connect_and_sync().await;
    settle(&h).await;
    assert_eq!(h.live("dav", &keys), keys);
    let batch = musubi_model::Provider::descriptor(&h.fake).limits.max_batch.unwrap_or(100);

    // Three changed validators: fetched in batches, the rest not at all.
    for k in ["a", "c", "e"] {
        h.put("dav", k, "v2", &single(k, start), None);
    }
    let fetches = h.fake.calls().fetch;
    poll_once(&h, "dav").await;
    assert_eq!(h.fake.calls().fetch, fetches + 3u32.div_ceil(batch));
    for (k, v) in [("a", "v2"), ("b", "v1"), ("c", "v2"), ("e", "v2")] {
        assert_eq!(h.item("dav", k).validator.0, v);
    }

    // A full listing with equal validators fetches nothing.
    let fetches = h.fake.calls().fetch;
    h.fake.full_on_next_delta();
    poll_once(&h, "dav").await;
    assert_eq!(h.fake.calls().fetch, fetches);
    assert_eq!(h.live("dav", &keys), keys);
}

#[tokio::test]
async fn failed_fetch_after_listing_repeats_the_page_without_duplicates() {
    let h = H::new();
    h.fake.add_collection(dav("dav"));
    let start = h.clock_now() + SignedDuration::from_hours(24);
    h.put("dav", "a", "v1", &single("a", start), None);
    h.connect_and_sync().await;
    settle(&h).await;

    for k in ["b", "c"] {
        h.put("dav", k, "v1", &single(k, start), None);
    }
    h.fake
        .fail(Op::Fetch, SyncError::Transient("connection reset".into()));
    poll_once(&h, "dav").await;
    assert_eq!(h.live("dav", &["a", "b", "c"]), ["a"]);

    h.advance(Duration::from_secs(120));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.live("dav", &["a", "b", "c"]), ["a", "b", "c"]);
    let mut created = h.entity_changes(ChangeOp::Created);
    let n = created.len();
    created.sort();
    created.dedup();
    assert_eq!((created.len(), n), (3, 3), "one Created per entity");
}

#[tokio::test]
async fn object_missing_from_fetch_stays_in_a_delta_and_goes_in_a_full_read() {
    let h = H::new();
    h.fake.add_collection(dav("dav"));
    let start = h.clock_now() + SignedDuration::from_hours(24);
    for k in ["a", "b"] {
        h.put("dav", k, "v1", &single(k, start), None);
    }
    h.connect_and_sync().await;
    settle(&h).await;

    // Listed with a new validator but gone by the fetch: nothing changes.
    h.put("dav", "a", "v2", &single("a", start), None);
    h.fake.hide_from_fetch(&cal("dav"), &RemoteKey("a".into()));
    poll_once(&h, "dav").await;
    assert_eq!(h.live("dav", &["a", "b"]), ["a", "b"]);
    assert_eq!(h.item("dav", "a").validator.0, "v1");
    assert_eq!(h.entity_changes(ChangeOp::Deleted), []);

    // In a full read it is absent, so it is deleted.
    let a = h.entity_of("dav", "a");
    h.fake.full_on_next_delta();
    poll_once(&h, "dav").await;
    assert_eq!(h.live("dav", &["a", "b"]), ["b"]);
    assert_eq!(h.entity_changes(ChangeOp::Deleted), [a]);
}

#[tokio::test]
async fn full_pass_from_a_delta_cursor_deletes_absent_items() {
    let h = H::new();
    for k in ["a", "b", "c"] {
        h.put_at(k, 1);
    }
    h.connect_and_sync().await;
    settle(&h).await;
    let full_reads = h.fake.calls().full_reads;
    let b = h.entity_of("cal1", "b");

    h.fake.hide_from_reads(&cal("cal1"), &RemoteKey("b".into()));
    h.fake.delete(&cal("cal1"), &RemoteKey("b".into()));
    h.fake.full_on_next_delta();
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();

    assert_eq!(h.live("cal1", &["a", "b", "c"]), ["a", "c"]);
    assert_eq!(h.entity_changes(ChangeOp::Deleted), [b]);
    assert_eq!(h.fake.calls().full_reads, full_reads, "no read of our own");
}

#[tokio::test]
async fn rediscover_runs_a_discovery_and_defers_the_collection() {
    let h = H::new();
    h.connect_and_sync().await;
    settle(&h).await;
    let discover = h.fake.calls().discover;

    h.put_at("n", 1);
    // The adapter recorded a descent (DAV: 405 on sync-collection); the
    // rediscovery hands it back as the new endpoint state.
    let descended =
        musubi_model::provider::Endpoint::new("https://fake.test", serde_json::json!({ "delta": false }));
    h.fake.set_endpoint(Some(descended.clone()));
    let reads = h.fake.read_endpoints().len();
    h.fake.fail(Op::Page, SyncError::Rediscover);
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.fake.calls().discover, discover + 1);

    h.advance(Duration::from_secs(2));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.live("cal1", &["n"]), ["n"]);
    // The read that failed had the old endpoint; every read after the
    // discovery, the deferred one too, has the endpoint it stored.
    let seen = h.fake.read_endpoints();
    assert_ne!(seen[reads], Some(descended.clone()));
    assert!(seen.len() > reads + 1);
    assert!(seen[reads + 1..].iter().all(|e| *e == Some(descended.clone())));
}

#[tokio::test]
async fn reauth_without_oauth_does_not_refresh() {
    let h = H::build(|_| {}, FakeProvider::without_oauth);
    h.connect_and_sync().await;
    settle(&h).await;

    h.put_at("n", 1);
    h.fake.fail(Op::Page, SyncError::Unauthorized);
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.state(), Some(AccountState::ReauthRequired));
    assert_eq!(h.fake.calls().refresh, 0);
}

#[tokio::test]
async fn missing_scope_parks_the_kind_until_discovery_permits_it() {
    let h = H::new();
    h.fake.add_collection(task_list("tasks"));
    put_task(&h, "tasks", "t1", "v1");
    h.connect_and_sync().await;
    settle(&h).await;
    assert_eq!(h.live("tasks", &["t1"]), ["t1"]);
    let discover = h.fake.calls().discover;

    // The grant lost the task scope.
    h.fake.set_unauthorized(vec![EntityKind::Task]);
    h.fake.fail(
        Op::Page,
        SyncError::MissingScope {
            kind: EntityKind::Task,
        },
    );
    h.advance(Duration::from_secs(1000));
    h.engine.run_until_idle().await.unwrap();

    let col = h.collection("tasks").unwrap();
    let error = col.status.error.unwrap();
    assert_eq!(error.message, musubi_engine::MISSING_SCOPE);
    assert!(error.permanent);
    assert!(!col.gone, "collections of the kind stay");
    assert_eq!(h.fake.calls().discover, discover + 1);
    assert_eq!(h.control.missing(ACCOUNT).unwrap(), [EntityKind::Task]);
    assert_eq!(h.sync_due("tasks"), None, "not retried");
    assert_eq!(h.live("tasks", &["t1"]), ["t1"]);
    // Neither by interest.
    h.engine.touch(TENANT).await.unwrap();
    assert_eq!(h.sync_due("tasks"), None);

    // The scope is back: the next discovery clears it and syncs again.
    h.fake.set_unauthorized(vec![]);
    put_task(&h, "tasks", "t2", "v1");
    h.advance(h.config.rediscover_every + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert!(h.collection("tasks").unwrap().status.error.is_none());
    assert!(h.control.missing(ACCOUNT).unwrap().is_empty());
    assert_eq!(h.live("tasks", &["t1", "t2"]), ["t1", "t2"]);
}

#[tokio::test]
async fn unresolved_zone_is_unindexed_for_single_series_and_exception() {
    let h = H::new();
    let nowhere = |v: serde_json::Value| {
        let mut v = v;
        v["timeZone"] = json!("/Nowhere");
        v["duration"] = json!("PT1H");
        ev(v)
    };
    h.put("cal1", "zs", "v1", &nowhere(json!({"uid": "zs", "start": local(&h, 2, "09:00:00")})), None);
    h.put(
        "cal1",
        "zr",
        "v1",
        &nowhere(json!({"uid": "zr", "start": local(&h, 2, "09:00:00"),
            "recurrenceRules": ["FREQ=DAILY"]})),
        None,
    );
    // A master in a known zone and its exception in an unknown one.
    let master = ev(json!({
        "uid": "zm", "start": local(&h, 2, "09:00:00"), "timeZone": "Europe/Prague",
        "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY;COUNT=5"]
    }));
    h.put("cal1", "zm", "v1", &master, None);
    h.put(
        "cal1",
        "zm_x",
        "v1",
        &nowhere(json!({"uid": "zm", "start": local(&h, 3, "15:00:00"),
            "recurrenceId": local(&h, 3, "09:00:00"), "recurrenceIdTimeZone": "Europe/Prague"})),
        Some("zm"),
    );
    h.connect_and_sync().await;
    let [zs, zr, zm, zm_x] = ["zs", "zr", "zm", "zm_x"].map(|k| h.entity_of("cal1", k));

    let now = h.clock_now();
    let week = h.range(now, now + SignedDuration::from_hours(24 * 7));
    let unindexed: Vec<_> = week.unindexed.iter().map(|s| s.entity).collect();
    for e in [zs, zr, zm_x] {
        assert!(week.occurrences.iter().all(|o| o.entity != e));
        assert!(unindexed.contains(&e), "{e:?} reported unindexed");
    }
    // The exception still replaces its master's occurrence.
    assert_eq!(week.occurrences.iter().filter(|o| o.entity == zm).count(), 4);
    assert!(!unindexed.contains(&zm));

    // A week later only the series can occur.
    let later = h.range(
        now + SignedDuration::from_hours(24 * 9),
        now + SignedDuration::from_hours(24 * 10),
    );
    let unindexed: Vec<_> = later.unindexed.iter().map(|s| s.entity).collect();
    assert!(unindexed.contains(&zr));
    assert!(!unindexed.contains(&zs) && !unindexed.contains(&zm_x));

    // Extending the index makes no rows for them either.
    h.advance(h.config.extend_every + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    let now = h.clock_now();
    let year = h.range(now, now + SignedDuration::from_hours(24 * 400));
    for e in [zs, zr, zm_x] {
        assert!(year.occurrences.iter().all(|o| o.entity != e));
    }
}

#[tokio::test]
async fn custom_posix_zone_gives_the_iana_occurrences() {
    let h = H::new();
    let start = local(&h, 1, "09:00:00");
    let custom = ev(json!({
        "uid": "cp", "start": start, "timeZone": "/Prague",
        "timeZones": {"/Prague": "CET-1CEST,M3.5.0,M10.5.0/3"},
        "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY;COUNT=400"]
    }));
    let iana = ev(json!({
        "uid": "ip", "start": start, "timeZone": "Europe/Prague",
        "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY;COUNT=400"]
    }));
    h.put("cal1", "cp", "v1", &custom, None);
    h.put("cal1", "ip", "v1", &iana, None);
    h.connect_and_sync().await;
    let (cp, ip) = (h.entity_of("cal1", "cp"), h.entity_of("cal1", "ip"));

    let now = h.clock_now();
    let page = h.range(now, now + SignedDuration::from_hours(24 * 401));
    assert!(page.next.is_none());
    let of = |e| -> Vec<Timestamp> {
        page.occurrences
            .iter()
            .filter(|o| o.entity == e)
            .map(|o| o.start)
            .collect()
    };
    assert_eq!(of(cp).len(), 400);
    assert_eq!(of(cp), of(ip));
}

#[tokio::test]
async fn multi_component_overrides_move_and_cancel_occurrences() {
    let h = H::new();
    let master = ev(json!({
        "uid": "mc", "start": "2027-01-04T09:00:00", "timeZone": "Europe/Prague",
        "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY;COUNT=4"],
        "recurrenceOverrides": {
            "2027-01-05T09:00:00": {"excluded": true},
            "2027-01-06T09:00:00": {"start": "2027-01-06T14:00:00"},
            "2027-01-08T09:00:00": {},
            "2027-01-09T09:00:00": {"duration": "PT3H"}
        }
    }));
    h.put("cal1", "mc", "v1", &master, None);
    h.connect_and_sync().await;
    assert_eq!(
        starts(&h),
        [
            "2027-01-04T08:00:00Z",
            "2027-01-06T13:00:00Z",
            "2027-01-07T08:00:00Z",
            "2027-01-08T08:00:00Z",
            "2027-01-09T08:00:00Z"
        ]
    );
    let page = h.range(ts("2027-01-09T00:00:00Z"), ts("2027-01-10T00:00:00Z"));
    assert_eq!(page.occurrences[0].end.to_string(), "2027-01-09T11:00:00Z");
}

/// Asserts `gap` is `base` ± 10 % (the jitter).
fn about(gap: Duration, base: Duration) {
    let (g, b) = (gap.as_secs_f64(), base.as_secs_f64());
    assert!((b * 0.9 - 0.01..=b * 1.1 + 0.01).contains(&g), "{gap:?} not about {base:?}");
}

#[tokio::test]
async fn adaptive_poll_backs_off_and_a_change_resets_it() {
    let h = H::new();
    h.fake.add_collection(polled("poll"));
    h.connect_and_sync().await;
    settle(&h).await;
    let normal = h.config.poll_normal;

    let gaps: Vec<_> = {
        let mut v = Vec::new();
        for _ in 0..6 {
            v.push(poll_once(&h, "poll").await);
        }
        v
    };
    // The first gap is some `min · 2^q`; each quiet poll doubles it up to the maximum.
    let q = (0..4)
        .min_by_key(|q| (gaps[0].as_secs_f64() - normal.min.as_secs_f64() * 2f64.powi(*q)).abs() as u64)
        .unwrap();
    for (i, gap) in gaps.iter().enumerate() {
        let base = normal.min.saturating_mul(1 << (q as u32 + i as u32)).min(normal.max);
        about(*gap, base);
    }
    about(*gaps.last().unwrap(), normal.max);

    // A change resets it to the minimum.
    let start = h.clock_now() + SignedDuration::from_hours(24);
    h.put("poll", "x", "v1", &single("x", start), None);
    about(poll_once(&h, "poll").await, normal.min);

    // Interest: a sync due now, then the active bounds.
    h.engine.touch(TENANT).await.unwrap();
    assert!(h.sync_due("poll").unwrap() <= h.clock_now());
    h.engine.run_until_idle().await.unwrap();
    let due = h.sync_due("poll").unwrap();
    assert!(due.duration_since(h.clock_now()) <= SignedDuration::try_from(h.config.poll_active.max.mul_f64(1.1)).unwrap());
    // A second touch while active queues nothing.
    h.advance(Duration::from_secs(10));
    h.engine.touch(TENANT).await.unwrap();
    assert_eq!(h.sync_due("poll"), Some(due));
}

#[tokio::test]
async fn collection_with_an_open_channel_keeps_the_push_poll() {
    let h = H::new();
    h.connect_and_sync().await;
    settle(&h).await;
    assert!(!channel_rows(&h).1.is_empty(), "channel open");
    for _ in 0..3 {
        assert_eq!(poll_once(&h, "cal1").await, h.config.poll_every_push);
    }
    let due = h.sync_due("cal1");
    h.engine.touch(TENANT).await.unwrap();
    assert_eq!(h.sync_due("cal1"), due);
}

#[tokio::test]
async fn per_account_concurrency_limit_blocks_a_third_job() {
    let h = H::build(
        |c| c.sync_concurrency = 4,
        |f| {
            let mut d = musubi_model::Provider::descriptor(&f);
            d.limits.max_concurrency_per_account = Some(2);
            f.with_descriptor(d)
        },
    );
    h.fake.add_collection(meta("cal2"));
    h.fake.add_collection(meta("cal3"));
    h.fake.hold_full_reads();
    h.engine.connect(ACCOUNT).await.unwrap();
    let _running = h.engine.start();

    // The clock moves on, so a job turned away (also while the discovery
    // ran) comes back: it waits for the limit, not for the clock.
    let tick = || h.advance(Duration::from_millis(200));
    until(|| {
        tick();
        h.fake.calls().full_reads >= 2
    })
    .await;
    for _ in 0..10 {
        tick();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(h.fake.calls().full_reads, 2, "a third job waits");

    h.fake.release_full_reads();
    until(|| {
        tick();
        h.fake.calls().full_reads == 3
    })
    .await;
}

/// [`CanonicalCodec`] under a new mapping version.
struct CanonicalV2;

impl Codec<Event> for CanonicalV2 {
    const MAPPING_VERSION: u32 = 2;

    fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
        CanonicalCodec::decode(raw)
    }
}

#[tokio::test]
async fn new_mapping_version_redecodes_stored_items_once_offline() {
    let h = H::new();
    h.put("cal1", "g", "v1", &series("g@google.com"), None);
    h.put(
        "cal1",
        "g_20270105T080000Z",
        "v1",
        &exception("g@google.com", "confirmed", "2027-01-05T11:00:00"),
        Some("g"),
    );
    h.put_at("a", 1);
    h.connect_and_sync().await;
    settle(&h).await;
    let before = starts(&h);
    let journal = h.journal().len();
    let calls = h.fake.calls();

    let engine = h.engine_with::<CanonicalV2>(h.vault.clone());
    assert_eq!(engine.redecode(TENANT).await.unwrap(), 3);
    for k in ["g", "g_20270105T080000Z", "a"] {
        assert_eq!(h.item("cal1", k).mapping_version, 2);
    }
    assert_eq!(h.fake.calls(), calls, "no network call");
    assert_eq!(h.journal().len(), journal, "same canonical form, nothing journaled");
    // No duplicate master occurrence: the moved exception still replaces it.
    assert_eq!(starts(&h), before);
    assert_eq!(
        h.store().redecoded(&h.provider, EntityKind::Event).unwrap(),
        Some((<Event as musubi_model::kind::Kind>::SCHEMA_VERSION, 2))
    );

    // Once only: again, and after a restart.
    assert_eq!(engine.redecode(TENANT).await.unwrap(), 0);
    let engine = h.engine_with::<CanonicalV2>(h.vault.clone());
    assert_eq!(engine.redecode(TENANT).await.unwrap(), 0);
}

static GATE_ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static GATE_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [`CanonicalV2`] that waits for [`GATE_OPEN`] on a body with "gate-old".
struct GatedCodec;

impl Codec<Event> for GatedCodec {
    const MAPPING_VERSION: u32 = 2;

    fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
        if String::from_utf8_lossy(&raw.body).contains("gate-old") {
            GATE_ENTERED.store(true, Ordering::SeqCst);
            while !GATE_OPEN.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        CanonicalCodec::decode(raw)
    }
}

#[tokio::test]
async fn redecode_does_not_overwrite_a_concurrent_sync() {
    let h = H::new();
    let start = h.clock_now() + SignedDuration::from_hours(24);
    let mut old = single("r", start);
    old.title = "gate-old".into();
    h.put("cal1", "r", "v1", &old, None);
    h.connect_and_sync().await;
    settle(&h).await;

    let engine = h.engine_with::<GatedCodec>(h.vault.clone());
    let e = engine.clone();
    let redecode = tokio::spawn(async move { e.redecode(TENANT).await });
    until(|| GATE_ENTERED.load(Ordering::SeqCst)).await;

    // While it decodes the old body, a sync stores a new one.
    let mut fresh = single("r", start);
    fresh.title = "fresh".into();
    h.put("cal1", "r", "v2", &fresh, None);
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    engine.run_until_idle().await.unwrap();
    assert_eq!(h.item("cal1", "r").validator.0, "v2");

    GATE_OPEN.store(true, Ordering::SeqCst);
    redecode.await.unwrap().unwrap();
    let item = h.item("cal1", "r");
    assert_eq!(item.validator.0, "v2");
    assert!(item.observed.unwrap().get().contains("fresh"), "the newer canonical survives");
}

/// Panics on every body: a re-decode that fails midway.
struct PanickingV3;

impl Codec<Event> for PanickingV3 {
    const MAPPING_VERSION: u32 = 3;

    fn decode(_: &Raw) -> Decoded<CalendarEvent> {
        panic!("codec bug");
    }
}

#[tokio::test]
async fn failed_redecode_leaves_the_versions_unset() {
    let h = H::new();
    h.put_at("a", 1);
    h.connect_and_sync().await;

    let engine = h.engine_with::<PanickingV3>(h.vault.clone());
    let failed = tokio::spawn(async move { engine.redecode(TENANT).await }).await;
    assert!(!matches!(failed, Ok(Ok(_))));
    let done = h.store().redecoded(&h.provider, EntityKind::Event).unwrap();
    assert!(done.is_none_or(|(_, m)| m != 3), "{done:?}");
    assert_eq!(h.item("cal1", "a").mapping_version, 1);

    // The next start runs it.
    let engine = h.engine_with::<CanonicalV2>(h.vault.clone());
    assert_eq!(engine.redecode(TENANT).await.unwrap(), 1);
}

#[tokio::test]
async fn removal_stops_a_lone_pending_channel_by_the_stored_resource() {
    let h = H::new();
    h.put_at("a", 1);
    h.connect_and_sync().await;
    assert_eq!(
        h.collection("cal1").unwrap().push_resource.as_deref(),
        Some("resource-cal1")
    );

    // A crash between storing a pending channel and subscribing, after the
    // open one was gone: only the pending one is left.
    let open = channel_rows(&h).1;
    for id in &open {
        h.control.delete_channel(id).unwrap();
    }
    let col = h.col("cal1");
    h.control
        .put_channel(&ChannelRecord {
            tenant: TENANT,
            account: ACCOUNT,
            collection: col,
            state: musubi_model::control::ChannelState::Pending(ChannelId("pending-1".into())),
        })
        .unwrap();

    let engine = h.engine_with::<CanonicalCodec>(h.account_vault());
    engine.remove_account(ACCOUNT).await.unwrap();
    assert!(
        h.fake
            .unsubscribed()
            .contains(&(ChannelId("pending-1".into()), "resource-cal1".into())),
        "{:?}",
        h.fake.unsubscribed()
    );
    assert_eq!(channel_rows(&h), (vec![], vec![]));
}

/// Runs ten minutes of simulated time in ten-second steps.
async fn ten_minutes(h: &H) {
    for _ in 0..60 {
        h.advance(Duration::from_secs(10));
        h.engine.run_until_idle().await.unwrap();
    }
}

#[tokio::test]
async fn rediscover_of_a_still_listed_collection_backs_off() {
    // Reads answer as if the collection moved (404), while discovery keeps
    // listing it (a stale calendar list, ACL drift).
    let h = H::new();
    h.fake.add_collection(polled("poll"));
    h.connect_and_sync().await;
    settle(&h).await;
    let before = h.fake.calls();
    for _ in 0..10_000 {
        h.fake.fail(Op::Page, SyncError::Rediscover);
    }
    ten_minutes(&h).await;
    let after = h.fake.calls();
    let (discover, pages) = (after.discover - before.discover, after.pages - before.pages);
    assert!((1..=2).contains(&discover), "discoveries: {discover}");
    assert!(pages <= 25, "reads: {pages}");
    let error = h.collection("poll").unwrap().status.error.unwrap();
    assert!(!error.permanent, "a transient error, backed off");
}

#[tokio::test]
async fn missing_scope_from_an_inconsistent_provider_stays_parked() {
    // `changes` says the scope is missing; discovery reports every kind
    // permitted.
    let h = H::new();
    h.fake.add_collection(task_list("tasks"));
    put_task(&h, "tasks", "t1", "v1");
    h.connect_and_sync().await;
    settle(&h).await;
    let before = h.fake.calls();
    for _ in 0..10_000 {
        h.fake.fail(
            Op::Page,
            SyncError::MissingScope {
                kind: EntityKind::Task,
            },
        );
    }
    h.engine.touch(TENANT).await.unwrap();
    ten_minutes(&h).await;
    let after = h.fake.calls();
    let (discover, pages) = (after.discover - before.discover, after.pages - before.pages);
    assert!(discover <= 3, "discoveries: {discover}");
    assert!(pages <= 6, "reads: {pages}");
    let col = h.collection("tasks").unwrap();
    assert_eq!(col.status.error.unwrap().message, musubi_engine::MISSING_SCOPE);
    assert_eq!(h.sync_due("tasks"), None, "parked");
}

/// A polled collection whose validators are synthetic body hashes.
async fn hashed(h: &H) {
    h.fake.add_collection(polled("hashed"));
    let start = h.clock_now() + SignedDuration::from_hours(24);
    h.put("hashed", "x", "musubi:sha256:00", &single("x", start), None);
    h.connect_and_sync().await;
    settle(h).await;
}

#[tokio::test]
async fn hashed_collection_is_not_read_faster_while_active() {
    let h = H::with(|c| c.active_for = Duration::from_secs(86_400));
    hashed(&h).await;
    let due = h.sync_due("hashed");
    h.engine.touch(TENANT).await.unwrap();
    assert_eq!(h.sync_due("hashed"), due, "a touch does not read it now");
    let min = h.config.poll_normal.min.mul_f64(0.9);
    for _ in 0..3 {
        let gap = poll_once(&h, "hashed").await;
        assert!(gap >= min, "{gap:?} faster than the normal poll");
    }
}

#[tokio::test]
async fn hashed_collection_sleeps_with_its_tenant() {
    let h = H::new();
    hashed(&h).await;
    h.advance(h.config.sleep_after + Duration::from_secs(3600));
    h.engine.run_until_idle().await.unwrap();
    let min = h.config.poll_sleeping.min.mul_f64(0.9);
    for _ in 0..3 {
        let gap = poll_once(&h, "hashed").await;
        assert!(gap >= min, "{gap:?} faster than the sleeping poll");
    }
}
