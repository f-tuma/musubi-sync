//! Google Tasks and People against the fake Google server (plan 6, K6).

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use jiff::Timestamp;
use musubi_google::{GoogleConfig, GoogleProvider};
use musubi_model::provider::{Clock, Credential, FetchHint, Pass, Start};
use musubi_model::remote::RemoteRef;
use musubi_model::{
    AccountCx, AccountId, ChangePage, CollectionRef, Cursor, EntityKind, Provider, RemoteKey,
    SyncError,
};
use musubi_testkit::account_cx;
use musubi_testkit::google::{Config, Endpoint, FakeGoogle, Fault};
use secrecy::SecretString;
use serde_json::{Value, json};

const CONTACTS: &str = "people/me/connections";

struct WallClock;

impl Clock for WallClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

struct Env {
    g: FakeGoogle,
    p: GoogleProvider,
    cx: AccountCx,
}

async fn env() -> Env {
    let g = FakeGoogle::start(Config::default()).await.unwrap();
    let mut google = GoogleConfig::new("client", SecretString::from("secret"));
    google.api_base = g.calendar_api().parse().unwrap();
    google.tasks_base = g.tasks_api().parse().unwrap();
    google.people_base = g.people_api().parse().unwrap();
    google.token_url = g.token_url().parse().unwrap();
    google.discover_tasks_people = true;
    let p = GoogleProvider::new(google, Arc::new(WallClock)).unwrap();
    let mut cx = account_cx(AccountId::from_u128(1), Timestamp::now());
    cx.credential = Credential::Bearer(g.access_token().into());
    cx.clock = Arc::new(WallClock);
    Env { g, p, cx }
}

fn hint(page: u32) -> FetchHint {
    let mut h = FetchHint::new(Start::Full);
    h.page_size = NonZeroU32::new(page);
    h
}

fn tasks(list: &str) -> CollectionRef {
    CollectionRef(format!("tasks/{list}"))
}

fn contacts() -> CollectionRef {
    CollectionRef(CONTACTS.into())
}

impl Env {
    async fn read(
        &self,
        col: &CollectionRef,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> Vec<Result<ChangePage, SyncError>> {
        self.p.changes(&self.cx, col, cursor, hint).collect().await
    }

    async fn read_ok(
        &self,
        col: &CollectionRef,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> Vec<ChangePage> {
        let pages: Vec<ChangePage> = self
            .read(col, cursor, hint)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        let last = pages.last().unwrap();
        assert!(last.last && pages.iter().rev().skip(1).all(|p| !p.last));
        pages
    }

    async fn first_error(&self, col: &CollectionRef, cursor: Option<Cursor>) -> SyncError {
        let out = self.read(col, cursor, hint(100)).await;
        assert_eq!(out.len(), 1, "the stream ends at the error");
        out.into_iter().next().unwrap().unwrap_err()
    }
}

/// Upserts by key with their validators, removals, and whether a key
/// repeated within the pass.
struct Pass_ {
    upserts: BTreeMap<String, (String, Value)>,
    removals: Vec<String>,
    repeated: bool,
}

fn pass(pages: &[ChangePage]) -> Pass_ {
    let mut out = Pass_ {
        upserts: BTreeMap::new(),
        removals: Vec::new(),
        repeated: false,
    };
    let mut seen = BTreeSet::new();
    for p in pages {
        for o in &p.upserts {
            out.repeated |= !seen.insert(o.key.0.clone());
            let body = serde_json::from_slice(&o.raw.body).unwrap();
            out.upserts
                .insert(o.key.0.clone(), (o.validator.0.clone(), body));
        }
        for k in &p.removals {
            out.repeated |= !seen.insert(k.0.clone());
            out.removals.push(k.0.clone());
        }
    }
    out
}

fn cursor(pages: &[ChangePage]) -> Option<Cursor> {
    pages.last().unwrap().cursor.clone()
}

#[tokio::test]
async fn discovery_finds_calendars_task_lists_and_contacts() {
    let e = env().await;
    let lists = [e.g.put_task_list("Inbox"), e.g.put_task_list("Nákup")];
    let d = e.p.discover(&e.cx).await.unwrap();
    assert!(d.unauthorized.is_empty());
    let by_kind = |k: EntityKind| -> Vec<(String, String)> {
        d.collections
            .iter()
            .filter(|c| c.kind == k)
            .map(|c| (c.remote.0.clone(), c.name.clone()))
            .collect()
    };
    assert_eq!(by_kind(EntityKind::Event).len(), 1);
    let mut want = vec![
        (format!("tasks/{}", lists[0]), "Inbox".to_owned()),
        (format!("tasks/{}", lists[1]), "Nákup".to_owned()),
    ];
    want.sort();
    let mut got = by_kind(EntityKind::Task);
    got.sort();
    assert_eq!(got, want);
    assert_eq!(
        by_kind(EntityKind::Contact),
        [(CONTACTS.to_owned(), "Kontakty".to_owned())]
    );
    let probe =
        e.g.requests()
            .into_iter()
            .find(|r| r.contains("/connections"))
            .unwrap();
    assert!(probe.contains("pageSize=1"), "{probe}");
}

#[tokio::test]
async fn missing_scope_at_discovery_is_unauthorized_in_either_shape() {
    for (tasks_fault, people_fault) in [
        (
            Fault::insufficient_permissions(),
            Fault::scope_insufficient(),
        ),
        (
            Fault::scope_insufficient(),
            Fault::insufficient_permissions(),
        ),
    ] {
        let e = env().await;
        e.g.put_task_list("Inbox");
        e.g.fail(Endpoint::TaskLists, tasks_fault, 1);
        e.g.fail(Endpoint::PeopleConnections, people_fault, 1);
        let d = e.p.discover(&e.cx).await.unwrap();
        assert_eq!(d.unauthorized, [EntityKind::Task, EntityKind::Contact]);
        assert!(d.collections.iter().all(|c| c.kind == EntityKind::Event));
        assert_eq!(d.collections.len(), 1);
    }
}

#[tokio::test]
async fn disabled_api_at_discovery_is_unauthorized_and_missing_scope_in_changes() {
    let classic = Fault {
        domain: "usageLimits",
        reason: "accessNotConfigured",
        rpc_status: None,
        ..Fault::api_disabled()
    };
    for (tasks_fault, people_fault) in [
        (classic.clone(), Fault::api_disabled()),
        (Fault::api_disabled(), classic.clone()),
    ] {
        let e = env().await;
        let list = e.g.put_task_list("Inbox");
        e.g.fail(Endpoint::TaskLists, tasks_fault.clone(), 1);
        e.g.fail(Endpoint::PeopleConnections, people_fault.clone(), 1);
        let d = e.p.discover(&e.cx).await.unwrap();
        assert_eq!(d.unauthorized, [EntityKind::Task, EntityKind::Contact]);
        assert_eq!(d.collections.len(), 1, "calendars stay complete");
        e.g.fail(Endpoint::TasksList, tasks_fault, 1);
        let err = e.first_error(&tasks(&list), None).await;
        assert!(
            matches!(
                err,
                SyncError::MissingScope {
                    kind: EntityKind::Task
                }
            ),
            "{err:?}"
        );
    }
}

/// A throttled, failing or refused probe makes its kind incomplete for the
/// round: the calendars and the other kind are complete, never an
/// account-wide error, and the kind is not unauthorised.
#[tokio::test]
async fn throttled_failing_or_refused_probe_skips_its_kind() {
    let refused = Fault {
        status: 401,
        domain: "global",
        reason: "authError",
        message: "Invalid Credentials",
        retry_after: None,
        rpc_status: None,
    };
    for (endpoint, skipped) in [
        (Endpoint::TaskLists, EntityKind::Task),
        (Endpoint::PeopleConnections, EntityKind::Contact),
    ] {
        for fault in [
            Fault::rate_limited(Some(7)),
            Fault::backend_error(),
            refused.clone(),
        ] {
            let e = env().await;
            e.g.put_task_list("Inbox");
            e.g.fail(endpoint, fault.clone(), 1);
            let d = e.p.discover(&e.cx).await.unwrap();
            let kinds: BTreeSet<_> = d
                .collections
                .iter()
                .map(|c| format!("{:?}", c.kind))
                .collect();
            let want: BTreeSet<_> = [EntityKind::Event, EntityKind::Task, EntityKind::Contact]
                .into_iter()
                .filter(|k| *k != skipped)
                .map(|k| format!("{k:?}"))
                .collect();
            assert_eq!(kinds, want, "{fault:?}");
            assert_eq!(d.incomplete, vec![skipped], "{fault:?}");
            assert!(d.unauthorized.is_empty(), "{fault:?}");
        }
    }
}

/// People unauthorised while Tasks is throttled: both are reported, each in
/// its own list.
#[tokio::test]
async fn unauthorized_people_is_recorded_while_tasks_is_incomplete() {
    let e = env().await;
    e.g.fail(Endpoint::TaskLists, Fault::rate_limited(None), 1);
    e.g.fail(Endpoint::PeopleConnections, Fault::scope_insufficient(), 1);
    let d = e.p.discover(&e.cx).await.unwrap();
    assert_eq!(d.incomplete, vec![EntityKind::Task]);
    assert_eq!(d.unauthorized, vec![EntityKind::Contact]);
}

#[tokio::test]
async fn discovery_without_the_flag_is_phase_1() {
    let e = env().await;
    e.g.put_task_list("Inbox");
    let mut google = GoogleConfig::new("client", SecretString::from("secret"));
    google.api_base = e.g.calendar_api().parse().unwrap();
    google.tasks_base = e.g.tasks_api().parse().unwrap();
    google.people_base = e.g.people_api().parse().unwrap();
    google.token_url = e.g.token_url().parse().unwrap();
    let p = GoogleProvider::new(google, Arc::new(WallClock)).unwrap();
    let d = p.discover(&e.cx).await.unwrap();
    assert!(d.collections.iter().all(|c| c.kind == EntityKind::Event));
    assert!(d.unauthorized.is_empty());
    let reqs = e.g.requests();
    assert!(
        reqs.iter()
            .all(|r| !r.contains("/tasks/v1") && !r.contains("/people/v1")),
        "{reqs:?}"
    );
}

#[tokio::test]
async fn other_403_at_discovery_makes_the_kind_incomplete() {
    let e = env().await;
    e.g.fail(Endpoint::PeopleConnections, Fault::forbidden(), 1);
    let d = e.p.discover(&e.cx).await.unwrap();
    assert!(d.collections.iter().all(|c| c.kind != EntityKind::Contact));
    assert_eq!(d.incomplete, vec![EntityKind::Contact]);
    assert!(d.unauthorized.is_empty());
}

#[tokio::test]
async fn missing_scope_in_changes_is_missing_scope_and_other_403_permanent() {
    let e = env().await;
    let list = e.g.put_task_list("Inbox");
    for (endpoint, col, kind) in [
        (Endpoint::TasksList, tasks(&list), EntityKind::Task),
        (Endpoint::PeopleConnections, contacts(), EntityKind::Contact),
    ] {
        for fault in [
            Fault::insufficient_permissions(),
            Fault::scope_insufficient(),
        ] {
            e.g.fail(endpoint, fault, 1);
            let err = e.first_error(&col, None).await;
            assert!(
                matches!(err, SyncError::MissingScope { kind: k } if k == kind),
                "{err:?}"
            );
        }
        e.g.fail(endpoint, Fault::forbidden(), 1);
        assert!(matches!(
            e.first_error(&col, None).await,
            SyncError::Permanent(_)
        ));
    }
}

#[tokio::test]
async fn tasks_full_read_then_overlapping_delta_without_duplicates() {
    let e = env().await;
    let list = e.g.put_task_list("Inbox");
    let col = tasks(&list);
    let old = e.g.put_task(&list, json!({ "title": "old" }));
    // Two minutes on: `old` falls before the delta's 60 s overlap.
    e.g.advance(Duration::from_secs(120));
    let a = e.g.put_task(
        &list,
        json!({ "title": "a", "due": "2026-11-20T00:00:00.000Z" }),
    );
    let hidden = e.g.put_task(
        &list,
        json!({ "title": "cleared", "status": "completed", "completed": "2026-11-10T08:00:00.000Z", "hidden": true }),
    );
    let b = e.g.put_task(&list, json!({ "title": "b", "parent": a }));

    let pages = e.read_ok(&col, None, hint(2)).await;
    assert_eq!(pages.len(), 2);
    assert!(
        pages
            .iter()
            .all(|p| p.pass == Pass::Full && p.removals.is_empty())
    );
    let full = pass(&pages);
    assert!(!full.repeated);
    assert_eq!(
        full.upserts.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([old.clone(), a.clone(), b.clone(), hidden.clone()])
    );
    assert_eq!(
        full.upserts[&hidden].1["hidden"],
        json!(true),
        "hidden tasks sync as live items"
    );
    for r in e.g.requests().iter().filter(|r| r.contains("/tasks?")) {
        for p in [
            "showCompleted=true",
            "showHidden=true",
            "showDeleted=false",
            "maxResults=2",
        ] {
            assert!(r.contains(p), "{r}");
        }
        assert!(!r.contains("updatedMin"), "{r}");
    }

    // Nothing changed: the 60 s overlap brings tasks again with the same
    // validator, which the engine skips; nothing is removed or repeated.
    let quiet = e.read_ok(&col, cursor(&pages), hint(100)).await;
    assert!(quiet.iter().all(|p| p.pass == Pass::Delta));
    let again = pass(&quiet);
    assert!(!again.repeated && again.removals.is_empty());
    assert_eq!(
        again.upserts.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([a.clone(), b.clone(), hidden.clone()]),
        "updatedMin filters out `old`"
    );
    for (k, (validator, _)) in &again.upserts {
        assert_eq!(validator, &full.upserts[k].0);
    }
    let last = e.g.requests().pop().unwrap();
    assert!(last.contains("showDeleted=true"), "{last}");
    let url = reqwest::Url::parse(&format!("http://x{}", last.split(' ').nth(1).unwrap())).unwrap();
    let (_, min) = url.query_pairs().find(|(k, _)| k == "updatedMin").unwrap();
    let newest = full
        .upserts
        .values()
        .map(|(_, t)| t["updated"].as_str().unwrap().parse::<Timestamp>().unwrap())
        .max()
        .unwrap();
    assert_eq!(
        min.parse::<Timestamp>().unwrap(),
        newest - jiff::SignedDuration::from_secs(60)
    );

    e.g.put_task(
        &list,
        json!({ "id": a, "title": "a2", "status": "completed" }),
    );
    e.g.delete_task(&list, &b);
    let c = e.g.put_task(&list, json!({ "title": "c" }));
    let delta = e.read_ok(&col, cursor(&quiet), hint(1)).await;
    let changed = pass(&delta);
    assert!(!changed.repeated);
    assert_eq!(changed.removals, std::slice::from_ref(&b));
    assert_eq!(changed.upserts[&a].1["title"], "a2");
    assert_ne!(changed.upserts[&a].0, full.upserts[&a].0);
    assert!(changed.upserts.contains_key(&c));

    // Once the deletion is older than the overlap it no longer comes back;
    // a cursor without results keeps its watermark.
    let after = e.read_ok(&col, cursor(&delta), hint(100)).await;
    assert_eq!(pass(&after).removals, [b], "still inside the 60 s overlap");
    assert_eq!(cursor(&after), cursor(&delta));
    // A write two minutes later moves the watermark past the deletion.
    e.g.advance(Duration::from_secs(120));
    let d = e.g.put_task(&list, json!({ "title": "d" }));
    let moved = e.read_ok(&col, cursor(&after), hint(100)).await;
    let later = pass(&e.read_ok(&col, cursor(&moved), hint(100)).await);
    assert!(later.removals.is_empty(), "{:?}", later.removals);
    assert_eq!(later.upserts.keys().collect::<Vec<_>>(), [&d]);
}

#[tokio::test]
async fn forbidden_rate_limit_in_task_changes_is_rate_limited() {
    let e = env().await;
    let list = e.g.put_task_list("Inbox");
    let user = Fault {
        reason: "userRateLimitExceeded",
        ..Fault::forbidden_rate_limited(None)
    };
    for fault in [Fault::forbidden_rate_limited(Some(5)), user] {
        e.g.fail(Endpoint::TasksList, fault, 1);
        let err = e.first_error(&tasks(&list), None).await;
        assert!(matches!(err, SyncError::RateLimited { .. }), "{err:?}");
    }
}

#[tokio::test]
async fn tasks_rejected_page_token_is_cursor_expired() {
    let e = env().await;
    let list = e.g.put_task_list("Inbox");
    for stored in [
        r#"{"full":{"page":"bogus"}}"#,
        r#"{"updated_min":{"ts":"2026-11-11T00:00:00Z","page":"bogus"}}"#,
        r#"not json"#,
    ] {
        let err = e
            .first_error(&tasks(&list), Some(Cursor(stored.as_bytes().to_vec())))
            .await;
        assert!(matches!(err, SyncError::CursorExpired), "{stored}: {err:?}");
    }
}

#[tokio::test]
async fn tasks_and_people_fetch_returns_live_objects_only() {
    let e = env().await;
    let list = e.g.put_task_list("Inbox");
    let live = e.g.put_task(&list, json!({ "title": "live" }));
    let gone = e.g.put_task(&list, json!({ "title": "gone" }));
    e.g.delete_task(&list, &gone);
    let refs: Vec<RemoteRef> = [live.as_str(), gone.as_str(), "missing"]
        .into_iter()
        .map(|k| RemoteRef {
            collection: tasks(&list),
            key: RemoteKey(k.into()),
        })
        .collect();
    let out = e.p.fetch(&e.cx, &refs).await.unwrap();
    assert_eq!(
        out.iter().map(|o| o.key.0.as_str()).collect::<Vec<_>>(),
        [live.as_str()]
    );
    assert_eq!(out[0].kind, EntityKind::Task);

    let people = e.g.seed_contacts(2);
    e.g.delete_contact(&people[1]);
    let refs: Vec<RemoteRef> = [people[0].as_str(), people[1].as_str(), "people/c404"]
        .into_iter()
        .map(|k| RemoteRef {
            collection: contacts(),
            key: RemoteKey(k.into()),
        })
        .collect();
    let out = e.p.fetch(&e.cx, &refs).await.unwrap();
    assert_eq!(
        out.iter().map(|o| o.key.0.as_str()).collect::<Vec<_>>(),
        [people[0].as_str()]
    );
    let body: Value = serde_json::from_slice(&out[0].raw.body).unwrap();
    assert!(
        body["emailAddresses"].is_array(),
        "fetch asks for every person field"
    );
}

#[tokio::test]
async fn people_full_read_with_sync_token_then_delta_and_deletion() {
    let e = env().await;
    let col = contacts();
    let people = e.g.seed_contacts(5);
    let pages = e.read_ok(&col, None, hint(2)).await;
    assert_eq!(pages.len(), 3);
    assert!(
        pages
            .iter()
            .all(|p| p.pass == Pass::Full && p.removals.is_empty())
    );
    let full = pass(&pages);
    assert_eq!(full.upserts.len(), 5);
    assert!(!full.repeated);
    let lists: Vec<String> =
        e.g.requests()
            .into_iter()
            .filter(|r| r.contains("/connections?"))
            .collect();
    assert_eq!(lists.len(), 3);
    for r in &lists {
        for p in [
            "requestSyncToken=true",
            "pageSize=2",
            "personFields=addresses%2Cbiographies",
        ] {
            assert!(r.contains(p), "every page repeats every parameter: {r}");
        }
    }

    e.g.put_contact(json!({ "resourceName": people[0], "names": [{ "displayName": "Renamed" }] }));
    e.g.delete_contact(&people[1]);
    let added =
        e.g.put_contact(json!({ "names": [{ "displayName": "New" }] }));
    let delta = e.read_ok(&col, cursor(&pages), hint(100)).await;
    assert!(delta.iter().all(|p| p.pass == Pass::Delta));
    let changed = pass(&delta);
    assert_eq!(changed.removals, std::slice::from_ref(&people[1]));
    assert_eq!(
        changed.upserts.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([people[0].clone(), added])
    );
    assert!(e.g.requests().pop().unwrap().contains("syncToken="));

    let quiet = e.read_ok(&col, cursor(&delta), hint(100)).await;
    let nothing = pass(&quiet);
    assert!(nothing.upserts.is_empty() && nothing.removals.is_empty());
}

#[tokio::test]
async fn people_expired_sync_token_is_cursor_expired() {
    let e = env().await;
    let col = contacts();
    e.g.seed_contacts(1);
    let token = cursor(&e.read_ok(&col, None, hint(100)).await);
    // The server's own answer, and both statuses the plan allows (T10).
    e.g.expire_people_sync_tokens();
    assert!(matches!(
        e.first_error(&col, token.clone()).await,
        SyncError::CursorExpired
    ));
    let gone = Fault {
        status: 410,
        ..Fault::expired_sync_token()
    };
    for fault in [Fault::expired_sync_token(), gone] {
        e.g.fail(Endpoint::PeopleConnections, fault, 1);
        assert!(matches!(
            e.first_error(&col, token.clone()).await,
            SyncError::CursorExpired
        ));
    }
    let bogus = r#"{"full":{"page":"bogus","fields":"addresses,biographies,birthdays,clientData,emailAddresses,memberships,metadata,names,nicknames,organizations,phoneNumbers,photos,urls"}}"#;
    let err = e
        .first_error(&col, Some(Cursor(bogus.as_bytes().to_vec())))
        .await;
    assert!(
        matches!(err, SyncError::CursorExpired),
        "rejected page token: {err:?}"
    );
}

/// Plan 6.3: Google's exact error for a stale token is unverified (T10), so
/// any 400 on a request with a sync token restarts the pass; without one it
/// is a real error.
#[tokio::test]
async fn any_400_with_a_sync_token_is_cursor_expired() {
    let e = env().await;
    let col = contacts();
    e.g.seed_contacts(1);
    let token = cursor(&e.read_ok(&col, None, hint(100)).await);
    let bad = Fault {
        status: 400,
        domain: "global",
        reason: "badRequest",
        message: "Bad Request",
        retry_after: None,
        rpc_status: None,
    };
    e.g.fail(Endpoint::PeopleConnections, bad.clone(), 1);
    let err = e.first_error(&col, token).await;
    assert!(matches!(err, SyncError::CursorExpired), "{err:?}");
    e.g.fail(Endpoint::PeopleConnections, bad, 1);
    let err = e.first_error(&col, None).await;
    assert!(matches!(err, SyncError::Permanent(_)), "{err:?}");
}

#[tokio::test]
async fn people_429_on_the_first_page_is_rate_limited() {
    let e = env().await;
    e.g.seed_contacts(3);
    e.g.fail(
        Endpoint::PeopleConnections,
        Fault::rate_limited(Some(30)),
        1,
    );
    let err = e.first_error(&contacts(), None).await;
    assert!(
        matches!(err, SyncError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(30)),
        "{err:?}"
    );
}

/// Plan 6.3: the weekly expiry must not trip the mass-deletion guard. After an
/// expired token the full read lists every contact, so absence deletes
/// nothing, and its new cursor works.
#[tokio::test]
async fn expired_token_with_5000_contacts_full_read_deletes_nothing() {
    let e = env().await;
    let col = contacts();
    let people: BTreeSet<String> = e.g.seed_contacts(5000).into_iter().collect();
    let token = cursor(&e.read_ok(&col, None, FetchHint::new(Start::Full)).await);
    e.g.expire_people_sync_tokens();
    assert!(matches!(
        e.first_error(&col, token).await,
        SyncError::CursorExpired
    ));

    let pages = e.read_ok(&col, None, FetchHint::new(Start::Full)).await;
    assert_eq!(pages.len(), 5, "ceil(5000 / 1000) requests");
    assert!(pages.iter().all(|p| p.pass == Pass::Full));
    let full = pass(&pages);
    assert!(!full.repeated);
    assert!(full.removals.is_empty());
    assert_eq!(
        full.upserts.keys().cloned().collect::<BTreeSet<_>>(),
        people,
        "absent from the read: none"
    );

    let quiet = e
        .read_ok(&col, cursor(&pages), FetchHint::new(Start::Full))
        .await;
    assert!(quiet.iter().all(|p| p.pass == Pass::Delta));
    let nothing = pass(&quiet);
    assert!(nothing.upserts.is_empty() && nothing.removals.is_empty());
}
