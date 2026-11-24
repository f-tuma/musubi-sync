// Task occurrences in the index (docs/plan/phase2.md 12.2, 12.3, 12.5; K12b).

/// A [`Codec<Task>`] whose raw body is the canonical task as JSON.
struct TaskJson;

impl Codec<musubi_model::Task> for TaskJson {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<musubi_model::CalendarTask> {
        match serde_json::from_slice(&raw.body) {
            Ok(t) => Decoded::Canonical(t),
            Err(e) => Decoded::Unsupported {
                reason: e.to_string(),
            },
        }
    }
}

fn task_codecs() -> Codecs {
    Codecs {
        event: Some(DecodeFn::of::<CanonicalCodec>()),
        task: Some(DecodeFn::of::<TaskJson>()),
        ..Codecs::default()
    }
}

/// A harness whose provider also serves a task list `tasks` and whose engine
/// decodes events and tasks.
fn task_harness(tune: impl FnOnce(&mut Config)) -> H {
    let mut h = H::build(tune, |f| {
        use musubi_model::Provider as _;
        let mut d = f.descriptor();
        if !d.kinds.iter().any(|k| k.kind == EntityKind::Task) {
            d.kinds.push(KindCapabilities::new(
                EntityKind::Task,
                DeltaSupport::CtagEtag,
                PushSupport::None,
            ));
        }
        f.with_descriptor(d)
    });
    h.fake.add_collection(RemoteCollection::new(
        cal("tasks"),
        EntityKind::Task,
        "tasks",
        Access::Owner,
    ));
    h.engine = h.engine_codecs(h.vault.clone(), task_codecs());
    h
}

impl H {
    fn put_task(&self, key: &str, validator: &str, task: serde_json::Value) {
        let raw = Raw {
            media_type: musubi_model::remote::MediaType::Json,
            body: serde_json::to_vec(&task).unwrap(),
        };
        let object = musubi_model::RemoteObject::new(
            RemoteKey(key.into()),
            EntityKind::Task,
            musubi_model::Validator(validator.into()),
            raw,
        );
        self.fake.put(&cal("tasks"), FakeObject { object, span: None });
    }

    /// `(start, done)` of the task occurrences in `[start, end)`.
    fn task_rows(&self, start: Timestamp, end: Timestamp) -> Vec<(Timestamp, bool)> {
        let page = self.range_of(&[EntityKind::Task], start, end);
        assert!(page.occurrences.iter().all(|o| o.kind == EntityKind::Task));
        page.occurrences.iter().map(|o| (o.start, o.done)).collect()
    }
}

/// Local wall time (UTC) `days` from now at `time`, e.g. `09:00:00`.
fn day_at(h: &H, days: i64, time: &str) -> String {
    let day = (h.clock_now() + SignedDuration::from_hours(24 * days))
        .to_zoned(TimeZone::UTC)
        .date();
    format!("{day}T{time}")
}

fn utc(local: &str) -> Timestamp {
    format!("{local}Z").parse().unwrap()
}

/// A daily task from tomorrow at 09:00 UTC, its third occurrence completed.
fn daily_task(h: &H) -> serde_json::Value {
    json!({
        "uid": "daily", "title": "water plants", "start": day_at(h, 1, "09:00:00"),
        "due": day_at(h, 1, "09:30:00"), "timeZone": "Etc/UTC",
        "recurrenceRules": ["FREQ=DAILY"],
        "recurrenceOverrides": {day_at(h, 3, "09:00:00"): {"progress": "completed"}}
    })
}

#[tokio::test]
async fn range_returns_task_occurrences_with_their_done_state() {
    let h = task_harness(|_| {});
    h.put_task("daily", "v1", daily_task(&h));
    h.put_task(
        "google",
        "v1",
        json!({"uid": "google", "due": day_at(&h, 2, "00:00:00"), "showWithoutTime": true,
            "progress": "completed", "hidden": true}),
    );
    h.put_task("someday", "v1", json!({"uid": "someday", "title": "no date"}));
    h.put_at("event", 1);
    h.connect_and_sync().await;

    let (from, to) = (utc(&day_at(&h, 1, "00:00:00")), utc(&day_at(&h, 4, "00:00:00")));
    assert_eq!(
        h.task_rows(from, to),
        [
            (utc(&day_at(&h, 1, "09:00:00")), false),
            (utc(&day_at(&h, 2, "00:00:00")), true),
            (utc(&day_at(&h, 2, "09:00:00")), false),
            (utc(&day_at(&h, 3, "09:00:00")), true),
        ]
    );
    // Events only: no task; both kinds: the event too. A task without a date is
    // never unindexed.
    let events = h.range(from, to);
    assert!(events.occurrences.iter().all(|o| o.kind == EntityKind::Event));
    assert_eq!(events.occurrences.len(), 1);
    let both = h.range_of(&[EntityKind::Event, EntityKind::Task], from, to);
    assert_eq!(both.occurrences.len(), 5);
    assert!(both.unindexed.is_empty());
    assert_eq!(h.live("tasks", &["someday"]), ["someday"]);
}

#[tokio::test]
async fn progress_change_rewrites_done() {
    let h = task_harness(|_| {});
    let task = |progress: &str| {
        json!({"uid": "t", "due": day_at(&h, 1, "12:00:00"), "timeZone": "Etc/UTC",
            "progress": progress})
    };
    h.put_task("t", "v1", task("needs-action"));
    h.connect_and_sync().await;
    let (from, to) = (utc(&day_at(&h, 1, "00:00:00")), utc(&day_at(&h, 2, "00:00:00")));
    let at = utc(&day_at(&h, 1, "12:00:00"));
    assert_eq!(h.task_rows(from, to), [(at, false)]);

    h.put_task("t", "v2", task("completed"));
    h.advance(h.config.poll_every + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.item("tasks", "t").validator.0, "v2");
    assert_eq!(h.task_rows(from, to), [(at, true)]);

    h.put_task("t", "v3", task("cancelled"));
    h.advance(h.config.poll_every + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert!(h.task_rows(from, to).is_empty());
}

#[tokio::test]
async fn index_extension_extends_task_and_event_series() {
    let h = task_harness(|c| c.index_forward = Duration::from_secs(30 * 86_400));
    h.put_task("daily", "v1", daily_task(&h));
    let event = ev(json!({
        "uid": "standup", "start": day_at(&h, 1, "10:00:00"), "timeZone": "Etc/UTC",
        "duration": "PT15M", "recurrenceRules": ["FREQ=DAILY"]
    }));
    h.put("cal1", "standup", "v1", &event, None);
    h.connect_and_sync().await;

    let task = h.entity_of("tasks", "daily");
    let standup = h.entity_of("cal1", "standup");
    let horizon = |h: &H, e: EntityId| {
        h.store()
            .series_to_extend(Timestamp::MAX, 10)
            .unwrap()
            .into_iter()
            .find(|s| s.entity == e)
            .unwrap()
            .indexed_until
    };
    let (task_before, event_before) = (horizon(&h, task), horizon(&h, standup));
    // Day 35 lies past the 30 days indexed at the first sync.
    let later = (utc(&day_at(&h, 35, "00:00:00")), utc(&day_at(&h, 36, "00:00:00")));
    let nine = utc(&day_at(&h, 35, "09:00:00"));
    assert!(h.task_rows(later.0, later.1).is_empty());
    assert!(h.range(later.0, later.1).occurrences.is_empty());

    h.advance(Duration::from_secs(10 * 86_400));
    h.engine.run_until_idle().await.unwrap();
    assert!(horizon(&h, task) > task_before);
    assert!(horizon(&h, standup) > event_before);
    assert_eq!(h.task_rows(later.0, later.1), [(nine, false)]);
    let events = h.range(later.0, later.1);
    assert_eq!(events.occurrences.len(), 1);
    assert_eq!(events.occurrences[0].entity, standup);
}

#[tokio::test]
async fn main_v3_migration_redecodes_tasks_once_offline() {
    let mut h = task_harness(|_| {});
    h.put_task("daily", "v1", daily_task(&h));
    h.put_task(
        "once",
        "v1",
        json!({"uid": "once", "due": day_at(&h, 2, "12:00:00"), "timeZone": "Etc/UTC"}),
    );
    h.put_at("event", 1);
    h.connect_and_sync().await;
    h.engine.redecode(TENANT).await.unwrap();
    let (from, to) = (utc(&day_at(&h, 1, "00:00:00")), utc(&day_at(&h, 4, "00:00:00")));
    let tasks = h.task_rows(from, to);
    assert_eq!(tasks.len(), 4);
    let event_rows = |h: &H| -> Vec<_> {
        let page = h.range(from, to);
        page.occurrences.iter().map(|o| (o.entity, o.start, o.end)).collect()
    };
    let events = event_rows(&h);
    let journal = h.journal().len();

    // A process restart on a file of the previous binary; the path is the
    // store's layout (`SqliteStorage::new`, storage.rs).
    let dir = h._dir.0.join("tenants");
    let main_db = dir.join("tenants").join(TENANT.to_string()).join("main.db");
    musubi_testkit::store::downgrade_to_main_v2(&main_db);
    h.storage = Arc::new(SqliteStorage::new(&dir, 4).unwrap());
    h.engine = h.engine_codecs(h.vault.clone(), task_codecs());
    assert!(h.task_rows(from, to).is_empty(), "migrated, not yet re-decoded");
    let calls = h.fake.calls();
    assert_eq!(h.engine.redecode(TENANT).await.unwrap(), 2);
    assert_eq!(h.fake.calls(), calls, "no network call");
    assert_eq!(h.task_rows(from, to), tasks);
    assert_eq!(event_rows(&h), events);
    assert_eq!(h.journal().len(), journal, "same canonical form, nothing journaled");

    // Once only: again, and after another restart.
    assert_eq!(h.engine.redecode(TENANT).await.unwrap(), 0);
    let engine = h.engine_codecs(h.vault.clone(), task_codecs());
    assert_eq!(engine.redecode(TENANT).await.unwrap(), 0);
    assert_eq!(h.task_rows(from, to), tasks);
}
