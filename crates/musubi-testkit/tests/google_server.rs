//! Behaviour and response shapes of the fake Google Calendar server (K3).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use common::{get, get_anon, keys, post_form, post_json, receiver, set};
use jiff::Timestamp;
use musubi_testkit::google::{Config, Endpoint, FakeGoogle, Fault, Seed, Step};
use serde_json::{Value, json};

async fn start(seed: Seed) -> FakeGoogle {
    FakeGoogle::start(Config {
        seed,
        ..Config::default()
    })
    .await
    .expect("start")
}

fn events_url(g: &FakeGoogle, cal: &str, query: &str) -> String {
    format!("{}/calendars/{cal}/events?{query}", g.calendar_api())
}

/// Reads every page from `query`; returns the items and the `nextSyncToken`.
async fn read_all(g: &FakeGoogle, token: &str, cal: &str, query: &str) -> (Vec<Value>, String) {
    let mut items = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let q = match &page {
            Some(p) => format!("{query}&pageToken={p}"),
            None => query.to_owned(),
        };
        let r = get(&events_url(g, cal, &q), token).await;
        assert_eq!(r.status, 200, "{}", r.json);
        items.extend(r.json["items"].as_array().expect("items").iter().cloned());
        match (r.json.get("nextPageToken"), r.json.get("nextSyncToken")) {
            (Some(p), None) => page = Some(p.as_str().expect("str").to_owned()),
            (None, Some(s)) => return (items, s.as_str().expect("str").to_owned()),
            other => panic!("exactly one of nextPageToken/nextSyncToken expected: {other:?}"),
        }
    }
}

fn ids(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|e| e["id"].as_str().expect("id").to_owned())
        .collect()
}

fn timed(summary: &str, start: &str, end: &str) -> Value {
    json!({ "summary": summary, "start": { "dateTime": start }, "end": { "dateTime": end } })
}

#[tokio::test]
async fn calendar_list_has_documented_shape_and_pages() {
    let g = start(Seed {
        seed: 1,
        calendars: 3,
        events: 0,
    })
    .await;
    let t = g.access_token();
    let url = format!("{}/users/me/calendarList", g.calendar_api());
    let r = get(&format!("{url}?maxResults=2"), &t).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        keys(&r.json),
        set(&["kind", "etag", "items", "nextPageToken"])
    );
    assert_eq!(r.json["kind"], "calendar#calendarList");
    let first = &r.json["items"][0];
    assert_eq!(first["primary"], true, "primary calendar is listed first");
    for k in [
        "kind",
        "etag",
        "id",
        "summary",
        "timeZone",
        "accessRole",
        "backgroundColor",
        "foregroundColor",
        "defaultReminders",
    ] {
        assert!(first.get(k).is_some(), "calendarListEntry lacks {k}");
    }
    assert_eq!(first["kind"], "calendar#calendarListEntry");
    let next = r.json["nextPageToken"].as_str().expect("token");
    let r2 = get(&format!("{url}?maxResults=2&pageToken={next}"), &t).await;
    assert_eq!(r2.json["items"].as_array().expect("items").len(), 2);
    assert!(r2.json.get("nextPageToken").is_none());
    let mut all = ids(r.json["items"].as_array().expect("a"));
    all.extend(ids(r2.json["items"].as_array().expect("a")));
    assert_eq!(all, g.calendars());
}

#[tokio::test]
async fn full_read_pages_by_max_results_and_matches_documented_fields() {
    let g = start(Seed {
        seed: 2,
        calendars: 0,
        events: 30,
    })
    .await;
    let t = g.access_token();
    let first = get(&events_url(&g, "primary", "maxResults=7"), &t).await;
    assert_eq!(
        keys(&first.json),
        set(&[
            "kind",
            "etag",
            "summary",
            "description",
            "updated",
            "timeZone",
            "accessRole",
            "defaultReminders",
            "items",
            "nextPageToken"
        ])
    );
    assert_eq!(first.json["kind"], "calendar#events");
    assert_eq!(first.json["items"].as_array().expect("items").len(), 7);

    let (items, _) = read_all(&g, &t, "primary", "maxResults=7").await;
    let snapshot = g.snapshot("primary");
    let visible: Vec<String> = snapshot
        .iter()
        .filter(|(_, e)| e["status"] != "cancelled" || e.get("recurringEventId").is_some())
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(
        ids(&items),
        visible,
        "every visible event exactly once, in id order"
    );
    for e in &items {
        if e["status"] == "cancelled" {
            assert_eq!(
                keys(e),
                set(&[
                    "kind",
                    "etag",
                    "id",
                    "status",
                    "recurringEventId",
                    "originalStartTime"
                ])
            );
            continue;
        }
        for k in [
            "kind",
            "etag",
            "id",
            "status",
            "htmlLink",
            "created",
            "updated",
            "summary",
            "creator",
            "organizer",
            "start",
            "end",
            "iCalUID",
            "sequence",
            "reminders",
            "eventType",
        ] {
            assert!(e.get(k).is_some(), "event lacks {k}: {e}");
        }
        assert_eq!(e["kind"], "calendar#event");
    }
    assert!(
        items.iter().any(|e| e.get("recurrence").is_some()),
        "seed has series"
    );
    assert!(
        items.iter().any(|e| e["start"].get("date").is_some()),
        "seed has all-day events"
    );
}

#[tokio::test]
async fn empty_first_page_still_carries_next_page_token() {
    let g = FakeGoogle::start(Config {
        seed: Seed {
            seed: 3,
            calendars: 0,
            events: 5,
        },
        empty_first_page: true,
        ..Config::default()
    })
    .await
    .expect("start");
    let t = g.access_token();
    let r = get(&events_url(&g, "primary", ""), &t).await;
    assert_eq!(r.json["items"], json!([]));
    assert!(r.json["nextPageToken"].is_string());
    assert!(r.json.get("nextSyncToken").is_none());
    let (items, sync) = read_all(&g, &t, "primary", "").await;
    assert!(items.len() >= 5);
    // A delta pass starts with an empty page too.
    let r = get(&events_url(&g, "primary", &format!("syncToken={sync}")), &t).await;
    assert_eq!(r.json["items"], json!([]));
    assert!(r.json["nextPageToken"].is_string());
}

#[tokio::test]
async fn max_results_is_capped_at_2500_and_by_configured_page_size() {
    let g = start(Seed {
        seed: 4,
        calendars: 0,
        events: 2300,
    })
    .await;
    let t = g.access_token();
    let r = get(&events_url(&g, "primary", "maxResults=5000"), &t).await;
    assert_eq!(r.json["items"].as_array().expect("items").len(), 2500);
    let r = get(&events_url(&g, "primary", ""), &t).await;
    assert_eq!(
        r.json["items"].as_array().expect("items").len(),
        250,
        "default page size"
    );
    let r = get(&events_url(&g, "primary", "maxResults=0"), &t).await;
    assert_eq!(r.status, 400);

    let small = FakeGoogle::start(Config {
        seed: Seed {
            seed: 4,
            calendars: 0,
            events: 30,
        },
        max_page_size: 10,
        ..Config::default()
    })
    .await
    .expect("start");
    let t = small.access_token();
    let r = get(&events_url(&small, "primary", "maxResults=2500"), &t).await;
    assert_eq!(r.json["items"].as_array().expect("items").len(), 10);
}

#[tokio::test]
async fn sync_token_returns_changes_with_minimal_tombstones() {
    let g = start(Seed {
        seed: 5,
        calendars: 0,
        events: 10,
    })
    .await;
    let t = g.access_token();
    let (items, sync) = read_all(&g, &t, "primary", "").await;
    let victim = items
        .iter()
        .find(|e| {
            e["status"] == "confirmed"
                && e.get("recurrence").is_none()
                && e.get("recurringEventId").is_none()
        })
        .expect("event");
    let victim = victim["id"].as_str().expect("id").to_owned();

    let added = g.put_event(
        "primary",
        timed("new", "2026-11-12T10:00:00Z", "2026-11-12T11:00:00Z"),
    );
    g.delete_event("primary", &victim);
    let (delta, sync2) = read_all(&g, &t, "primary", &format!("syncToken={sync}")).await;
    let by_id: BTreeMap<String, Value> = delta
        .iter()
        .map(|e| (e["id"].as_str().expect("id").to_owned(), e.clone()))
        .collect();
    assert_eq!(
        by_id.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([added.clone(), victim.clone()])
    );
    assert_eq!(
        keys(&by_id[&victim]),
        set(&["kind", "etag", "id", "status"])
    );
    assert_eq!(by_id[&victim]["status"], "cancelled");
    assert_eq!(by_id[&added]["summary"], "new");

    let (none, sync3) = read_all(&g, &t, "primary", &format!("syncToken={sync2}")).await;
    assert!(none.is_empty());
    assert_eq!(sync2, sync3, "no change, same token");

    // A deleted event still answers events.get with status cancelled.
    let r = get(
        &format!("{}/calendars/primary/events/{victim}", g.calendar_api()),
        &t,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json["status"], "cancelled");
    let r = get(
        &format!("{}/calendars/primary/events/{added}", g.calendar_api()),
        &t,
    )
    .await;
    assert_eq!(r.json["summary"], "new");
    let r = get(
        &format!("{}/calendars/primary/events/nope", g.calendar_api()),
        &t,
    )
    .await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn delta_spanning_pages_is_consistent_under_concurrent_change() {
    let g = start(Seed {
        seed: 6,
        calendars: 0,
        events: 0,
    })
    .await;
    let t = g.access_token();
    let (_, sync) = read_all(&g, &t, "primary", "").await;
    for i in 0..5 {
        g.put_event("primary", json!({ "id": format!("e{i}"), "summary": "x", "start": { "date": "2026-11-12" }, "end": { "date": "2026-11-13" } }));
    }
    let r = get(
        &events_url(&g, "primary", &format!("syncToken={sync}&maxResults=2")),
        &t,
    )
    .await;
    assert_eq!(
        ids(r.json["items"].as_array().expect("items")),
        ["e0", "e1"]
    );
    // Changes during the pass: e0 (already served) and e4 (not yet served).
    g.put_event("primary", json!({ "id": "e0", "summary": "y", "start": { "date": "2026-11-12" }, "end": { "date": "2026-11-13" } }));
    g.delete_event("primary", "e4");
    let page = r.json["nextPageToken"].as_str().expect("page");
    let mut rest = Vec::new();
    let mut q = format!("pageToken={page}");
    let sync2 = loop {
        let r = get(&events_url(&g, "primary", &q), &t).await;
        rest.extend(r.json["items"].as_array().expect("items").iter().cloned());
        if let Some(p) = r.json["nextPageToken"].as_str() {
            q = format!("pageToken={p}");
        } else {
            break r.json["nextSyncToken"].as_str().expect("sync").to_owned();
        }
    };
    assert_eq!(
        ids(&rest),
        ["e2", "e3"],
        "events changed after the pass began move to the next delta"
    );
    let (next, _) = read_all(&g, &t, "primary", &format!("syncToken={sync2}")).await;
    assert_eq!(ids(&next), ["e0", "e4"]);
}

#[tokio::test]
async fn expired_sync_token_is_410_and_sync_token_rejects_time_bounds() {
    let g = start(Seed {
        seed: 7,
        calendars: 0,
        events: 3,
    })
    .await;
    let t = g.access_token();
    let (_, sync) = read_all(&g, &t, "primary", "").await;
    let r = get(
        &events_url(
            &g,
            "primary",
            &format!("syncToken={sync}&timeMin=2026-01-01T00:00:00Z"),
        ),
        &t,
    )
    .await;
    assert_eq!(r.status, 400);

    g.apply(&musubi_testkit::google::Change::ExpireSyncTokens {
        calendar: "primary".into(),
    });
    let r = get(&events_url(&g, "primary", &format!("syncToken={sync}")), &t).await;
    assert_eq!(r.status, 410);
    assert_eq!(keys(&r.json["error"]), set(&["errors", "code", "message"]));
    assert_eq!(r.json["error"]["code"], 410);
    assert_eq!(r.json["error"]["errors"][0]["reason"], "fullSyncRequired");
    let r = get(&events_url(&g, "primary", "syncToken=garbage"), &t).await;
    assert_eq!(
        r.status, 410,
        "an unreadable sync token is treated as expired"
    );
    // After a full re-read the new token works.
    let (_, fresh) = read_all(&g, &t, "primary", "").await;
    let r = get(
        &events_url(&g, "primary", &format!("syncToken={fresh}")),
        &t,
    )
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn time_window_bounds_end_by_time_min_and_start_by_time_max() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    let before = g.put_event(
        "primary",
        timed("before", "2026-11-01T08:00:00Z", "2026-11-01T09:00:00Z"),
    );
    let touching = g.put_event(
        "primary",
        timed(
            "ends at timeMin",
            "2026-11-01T09:00:00Z",
            "2026-11-02T00:00:00Z",
        ),
    );
    let inside = g.put_event(
        "primary",
        timed("inside", "2026-11-05T08:00:00Z", "2026-11-05T09:00:00Z"),
    );
    let across = g.put_event(
        "primary",
        timed(
            "across timeMax",
            "2026-11-09T23:00:00Z",
            "2026-11-10T01:00:00Z",
        ),
    );
    let after = g.put_event(
        "primary",
        timed(
            "starts at timeMax",
            "2026-11-10T00:00:00Z",
            "2026-11-10T01:00:00Z",
        ),
    );
    let series = g.put_event(
        "primary",
        json!({ "summary": "weekly since 2020", "start": { "dateTime": "2020-01-01T10:00:00Z" }, "end": { "dateTime": "2020-01-01T11:00:00Z" }, "recurrence": ["RRULE:FREQ=WEEKLY"] }),
    );
    let all_day = g.put_event("primary", json!({ "summary": "day", "start": { "date": "2026-11-03" }, "end": { "date": "2026-11-04" } }));
    let q = "timeMin=2026-11-02T00:00:00Z&timeMax=2026-11-10T00:00:00Z";
    let (items, _) = read_all(&g, &t, "primary", q).await;
    let got: BTreeSet<String> = ids(&items).into_iter().collect();
    assert_eq!(got, BTreeSet::from([inside, across, series, all_day]));
    assert!(!got.contains(&before) && !got.contains(&touching) && !got.contains(&after));

    let r = get(&events_url(&g, "primary", "timeMin=2026-11-02"), &t).await;
    assert_eq!(
        r.status, 400,
        "timeMin needs an RFC 3339 timestamp with offset"
    );
    let r = get(
        &events_url(
            &g,
            "primary",
            "timeMin=2026-11-10T00:00:00Z&timeMax=2026-11-02T00:00:00Z",
        ),
        &t,
    )
    .await;
    assert_eq!(r.json["error"]["errors"][0]["reason"], "timeRangeEmpty");
}

#[tokio::test]
async fn show_deleted_hides_deleted_events_but_not_cancelled_instances() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    let plain = g.put_event(
        "primary",
        timed("plain", "2026-11-12T10:00:00Z", "2026-11-12T11:00:00Z"),
    );
    let master = g.put_event(
        "primary",
        json!({ "summary": "series", "start": { "dateTime": "2026-11-12T10:00:00Z" }, "end": { "dateTime": "2026-11-12T11:00:00Z" }, "recurrence": ["RRULE:FREQ=DAILY;COUNT=5"] }),
    );
    let instance = g.put_event(
        "primary",
        json!({ "id": format!("{master}_20261113T100000Z"), "status": "cancelled", "recurringEventId": master, "originalStartTime": { "dateTime": "2026-11-13T10:00:00Z" } }),
    );
    g.delete_event("primary", &plain);

    let (default, _) = read_all(&g, &t, "primary", "").await;
    assert_eq!(ids(&default), {
        let mut v = vec![master.clone(), instance.clone()];
        v.sort();
        v
    });
    let (shown, _) = read_all(&g, &t, "primary", "showDeleted=true").await;
    assert_eq!(shown.len(), 3);
    assert!(ids(&shown).contains(&plain));
    let inst = shown
        .iter()
        .find(|e| e["id"] == instance.as_str())
        .expect("instance");
    assert_eq!(
        keys(inst),
        set(&[
            "kind",
            "etag",
            "id",
            "status",
            "recurringEventId",
            "originalStartTime"
        ])
    );
}

#[tokio::test]
async fn unsupported_parameters_are_rejected_not_ignored() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    for q in [
        "singleEvents=true",
        "q=x",
        "orderBy=startTime",
        "updatedMin=2026-01-01T00:00:00Z",
    ] {
        let r = get(&events_url(&g, "primary", q), &t).await;
        assert_eq!(r.status, 400, "{q}");
    }
    let r = get(&events_url(&g, "nobody@example.com", ""), &t).await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn token_endpoint_refreshes_and_reports_invalid_grant() {
    let g = start(Seed::default()).await;
    let refresh = g.refresh_token();
    let r = post_form(
        &g.token_url(),
        &format!("grant_type=refresh_token&refresh_token={refresh}&client_id=c&client_secret=s"),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        keys(&r.json),
        set(&["access_token", "expires_in", "scope", "token_type"])
    );
    assert_eq!(r.json["token_type"], "Bearer");
    let access = r.json["access_token"].as_str().expect("token").to_owned();
    assert_eq!(
        get(&events_url(&g, "primary", ""), &access).await.status,
        200
    );

    let r = post_form(&g.token_url(), "grant_type=password&username=x").await;
    assert_eq!(r.json["error"], "unsupported_grant_type");

    g.revoke_refresh_token(&refresh);
    let r = post_form(
        &g.token_url(),
        &format!("grant_type=refresh_token&refresh_token={refresh}"),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json,
        json!({ "error": "invalid_grant", "error_description": "Token has been expired or revoked." })
    );
}

#[tokio::test]
async fn rotating_refresh_tokens_revoke_the_previous_one() {
    let g = FakeGoogle::start(Config {
        rotate_refresh_tokens: true,
        ..Config::default()
    })
    .await
    .expect("start");
    let old = g.refresh_token();
    let r = post_form(
        &g.token_url(),
        &format!("grant_type=refresh_token&refresh_token={old}"),
    )
    .await;
    let new = r.json["refresh_token"]
        .as_str()
        .expect("rotated")
        .to_owned();
    assert_eq!(new, g.refresh_token());
    let r = post_form(
        &g.token_url(),
        &format!("grant_type=refresh_token&refresh_token={old}"),
    )
    .await;
    assert_eq!(r.json["error"], "invalid_grant");
}

#[tokio::test]
async fn api_requires_a_valid_bearer_token() {
    let g = start(Seed::default()).await;
    let r = get_anon(&events_url(&g, "primary", "")).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json["error"]["errors"][0]["reason"], "authError");
    let t = g.access_token();
    assert_eq!(get(&events_url(&g, "primary", ""), &t).await.status, 200);
    g.expire_access_tokens();
    assert_eq!(get(&events_url(&g, "primary", ""), &t).await.status, 401);
}

#[tokio::test]
async fn scripted_faults_return_google_errors_with_retry_after() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    g.fail(Endpoint::EventsList, Fault::rate_limited(Some(7)), 2);
    g.fail(Endpoint::Any, Fault::forbidden_rate_limited(None), 1);
    for _ in 0..2 {
        let r = get(&events_url(&g, "primary", ""), &t).await;
        assert_eq!(r.status, 429);
        assert_eq!(r.headers["retry-after"], "7");
        assert_eq!(r.json["error"]["errors"][0]["reason"], "rateLimitExceeded");
    }
    let r = get(&format!("{}/users/me/calendarList", g.calendar_api()), &t).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.json["error"]["errors"][0]["reason"], "rateLimitExceeded");
    assert_eq!(
        get(&events_url(&g, "primary", ""), &t).await.status,
        200,
        "faults are used up"
    );
    // The token endpoint answers faults in the OAuth error shape, not Calendar's.
    g.fail(Endpoint::Token, Fault::backend_error(), 1);
    g.fail(Endpoint::Token, Fault::rate_limited(Some(3)), 1);
    let r = post_form(&g.token_url(), "grant_type=refresh_token").await;
    assert_eq!(r.status, 503);
    assert_eq!(keys(&r.json), set(&["error", "error_description"]));
    let r = post_form(&g.token_url(), "grant_type=refresh_token").await;
    assert_eq!(r.status, 429);
    assert_eq!(r.headers["retry-after"], "3");
    assert_eq!(r.json["error"], "rateLimitExceeded");
}

/// Waits until the fake has attempted `n` webhook deliveries.
async fn deliveries(g: &FakeGoogle, n: usize) -> Vec<musubi_testkit::google::Delivery> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let d = g.deliveries();
            if d.len() >= n {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deliveries in time")
}

#[tokio::test]
async fn webhooks_are_delivered_only_to_loopback() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    let watch = format!("{}/calendars/primary/events/watch", g.calendar_api());
    for (id, address) in [
        ("dns", "http://example.com/hook"),
        ("ip", "http://192.0.2.1/hook"),
    ] {
        let r = post_json(
            &watch,
            &t,
            &json!({ "id": id, "type": "web_hook", "address": address }),
        )
        .await;
        assert_eq!(r.status, 200);
    }
    for d in deliveries(&g, 2).await {
        let err = d
            .result
            .expect_err("non-loopback callback must not be reached");
        assert!(err.contains("loopback"), "{err}");
    }
}

#[tokio::test]
async fn local_date_time_is_resolved_in_the_event_time_zone() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    // 10:00-11:00 in Prague (UTC+1 in November) is 09:00-10:00Z.
    let id = g.put_event(
        "primary",
        json!({ "summary": "local", "start": { "dateTime": "2026-11-12T10:00:00", "timeZone": "Europe/Prague" }, "end": { "dateTime": "2026-11-12T11:00:00", "timeZone": "Europe/Prague" } }),
    );
    let (inside, _) = read_all(
        &g,
        &t,
        "primary",
        "timeMin=2026-11-12T09:30:00Z&timeMax=2026-11-12T09:45:00Z",
    )
    .await;
    assert_eq!(ids(&inside), [id]);
    let (after, _) = read_all(
        &g,
        &t,
        "primary",
        "timeMin=2026-11-12T10:00:00Z&timeMax=2026-11-13T00:00:00Z",
    )
    .await;
    assert!(after.is_empty(), "the event ends at 10:00Z");
}

#[tokio::test]
#[should_panic(expected = "start needs a date")]
async fn an_event_with_an_unreadable_start_is_rejected() {
    let g = start(Seed::default()).await;
    g.put_event(
        "primary",
        json!({ "summary": "floating", "start": { "dateTime": "2026-11-12T10:00:00" }, "end": { "dateTime": "2026-11-12T11:00:00" } }),
    );
}

#[tokio::test]
async fn seeded_events_come_in_the_calendar_offset_and_all_resolve() {
    let seed = Seed {
        seed: 9,
        calendars: 0,
        events: 30,
    };
    let g = start(seed).await;
    let t = g.access_token();
    let starts: Vec<String> = g
        .snapshot("primary")
        .values()
        .filter_map(|e| e["start"]["dateTime"].as_str().map(str::to_owned))
        .collect();
    // Written in UTC, Prague and New York offsets or local time, all are
    // served in the calendar's (Prague) offset, as live Google does.
    assert!(!starts.is_empty());
    for s in &starts {
        assert!(s.ends_with("+01:00") || s.ends_with("+02:00"), "{s}");
    }
    // Seeded events lie within 30 days of the epoch; a window around it shows them all.
    let (all, _) = read_all(&g, &t, "primary", "").await;
    let (window, _) = read_all(
        &g,
        &t,
        "primary",
        "timeMin=2026-09-01T00:00:00Z&timeMax=2027-02-01T00:00:00Z",
    )
    .await;
    assert_eq!(ids(&window), ids(&all));
}

#[tokio::test]
async fn empty_pages_can_appear_in_the_middle_of_a_read() {
    let seed = Seed {
        seed: 10,
        calendars: 0,
        events: 12,
    };
    let g = FakeGoogle::start(Config {
        seed,
        max_page_size: 3,
        empty_page_every: 2,
        ..Config::default()
    })
    .await
    .expect("start");
    let t = g.access_token();
    let mut sizes = Vec::new();
    let mut q = String::new();
    loop {
        let r = get(&events_url(&g, "primary", &q), &t).await;
        sizes.push(r.json["items"].as_array().expect("items").len());
        match r.json["nextPageToken"].as_str() {
            Some(p) => q = format!("pageToken={p}"),
            None => break,
        }
    }
    assert_eq!(&sizes[..4], [3, 3, 0, 3], "{sizes:?}");
    let (items, _) = read_all(&g, &t, "primary", "").await;
    let reference = start(seed).await;
    let rt = reference.access_token();
    let (expected, _) = read_all(&reference, &rt, "primary", "").await;
    assert_eq!(ids(&items), ids(&expected), "empty pages lose nothing");
}

#[tokio::test]
async fn continuing_a_paged_delta_after_expiry_is_410() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    let (_, sync) = read_all(&g, &t, "primary", "").await;
    for i in 0..5 {
        g.put_event(
            "primary",
            timed(
                &format!("e{i}"),
                "2026-11-12T10:00:00Z",
                "2026-11-12T11:00:00Z",
            ),
        );
    }
    let r = get(
        &events_url(&g, "primary", &format!("syncToken={sync}&maxResults=2")),
        &t,
    )
    .await;
    let page = r.json["nextPageToken"].as_str().expect("more pages");
    g.apply(&musubi_testkit::google::Change::ExpireSyncTokens {
        calendar: "primary".into(),
    });
    let r = get(&events_url(&g, "primary", &format!("pageToken={page}")), &t).await;
    assert_eq!(r.status, 410);
    assert_eq!(r.json["error"]["errors"][0]["reason"], "fullSyncRequired");
}

#[tokio::test]
async fn expired_channels_are_forgotten() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    let watch = format!("{}/calendars/primary/events/watch", g.calendar_api());
    let soon = (Timestamp::now().as_millisecond() + 200).to_string();
    let body = json!({ "id": "short", "type": "web_hook", "address": "http://127.0.0.1:1/", "expiration": soon });
    assert_eq!(post_json(&watch, &t, &body).await.status, 200);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(g.channels().is_empty());
    let again = json!({ "id": "short", "type": "web_hook", "address": "http://127.0.0.1:1/" });
    assert_eq!(
        post_json(&watch, &t, &again).await.status,
        200,
        "an expired id is free again"
    );
}

#[tokio::test]
async fn unknown_routes_answer_json_404_and_are_logged() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    let r = get(&format!("{}/calendars/primary/acl", g.calendar_api()), &t).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json["error"]["errors"][0]["reason"], "notFound");
    let r = post_json(&events_url(&g, "primary", ""), &t, &json!({})).await;
    assert_eq!(
        r.json["error"]["errors"][0]["reason"], "notFound",
        "wrong method"
    );
    assert!(
        g.requests()
            .contains(&"GET /calendar/v3/calendars/primary/acl".to_owned())
    );
}

#[tokio::test]
async fn latency_delays_every_response() {
    let g = FakeGoogle::start(Config {
        latency: Duration::from_millis(150),
        ..Config::default()
    })
    .await
    .expect("start");
    let t = g.access_token();
    let at = Instant::now();
    get(&events_url(&g, "primary", ""), &t).await;
    assert!(at.elapsed() >= Duration::from_millis(150));
    g.set_latency(Duration::ZERO);
    let at = Instant::now();
    get(&events_url(&g, "primary", ""), &t).await;
    assert!(at.elapsed() < Duration::from_millis(150));
}

async fn next_hook(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<axum::http::HeaderMap>,
) -> axum::http::HeaderMap {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("notification in time")
        .expect("open")
}

#[tokio::test]
async fn watch_delivers_sync_then_change_notifications_until_stopped() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    let (hook, mut rx) = receiver().await;
    let watch = format!("{}/calendars/primary/events/watch", g.calendar_api());
    let r = post_json(&watch, &t, &json!({ "id": "ch-1", "type": "web_hook", "address": hook, "token": "secret", "params": { "ttl": "3600" } })).await;
    assert_eq!(r.status, 200, "{}", r.json);
    assert_eq!(
        keys(&r.json),
        set(&[
            "kind",
            "id",
            "resourceId",
            "resourceUri",
            "token",
            "expiration"
        ])
    );
    assert_eq!(r.json["kind"], "api#channel");
    let expiration: i64 = r.json["expiration"]
        .as_str()
        .expect("ms as string")
        .parse()
        .expect("ms");
    let ttl = expiration - Timestamp::now().as_millisecond();
    assert!(
        (3_590_000..=3_600_000).contains(&ttl),
        "params.ttl honoured: {ttl}"
    );
    let resource = r.json["resourceId"].as_str().expect("id").to_owned();

    let h = next_hook(&mut rx).await;
    assert_eq!(h["x-goog-resource-state"], "sync");
    assert_eq!(h["x-goog-message-number"], "1");
    assert_eq!(h["x-goog-channel-id"], "ch-1");
    assert_eq!(h["x-goog-channel-token"], "secret");
    assert_eq!(h["x-goog-resource-id"], resource.as_str());
    assert!(
        h["x-goog-resource-uri"]
            .to_str()
            .expect("ascii")
            .contains("/calendars/")
    );
    assert!(
        h["x-goog-channel-expiration"]
            .to_str()
            .expect("ascii")
            .ends_with(" GMT")
    );

    g.put_event(
        "primary",
        timed("x", "2026-11-12T10:00:00Z", "2026-11-12T11:00:00Z"),
    );
    let h = next_hook(&mut rx).await;
    assert_eq!(h["x-goog-resource-state"], "exists");
    assert_eq!(h["x-goog-message-number"], "2");

    let dup = post_json(
        &watch,
        &t,
        &json!({ "id": "ch-1", "type": "web_hook", "address": "http://127.0.0.1:1/" }),
    )
    .await;
    assert_eq!(dup.status, 400, "channel ids are unique");

    let stop = format!("{}/channels/stop", g.calendar_api());
    let r = post_json(&stop, &t, &json!({ "id": "ch-1", "resourceId": "wrong" })).await;
    assert_eq!(r.status, 404);
    let r = post_json(&stop, &t, &json!({ "id": "ch-1", "resourceId": resource })).await;
    assert_eq!(r.status, 204);
    assert!(g.channels().is_empty());
    g.put_event(
        "primary",
        timed("y", "2026-11-12T10:00:00Z", "2026-11-12T11:00:00Z"),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rx.try_recv().is_err(), "a stopped channel stays quiet");
    assert_eq!(g.deliveries().len(), 2);
    assert!(g.deliveries().iter().all(|d| d.result == Ok(200)));
}

#[tokio::test]
async fn watch_expiration_is_capped_by_the_configured_maximum() {
    let g = FakeGoogle::start(Config {
        max_watch_ttl: Duration::from_secs(60),
        ..Config::default()
    })
    .await
    .expect("start");
    let t = g.access_token();
    let watch = format!("{}/calendars/primary/events/watch", g.calendar_api());
    let far = (Timestamp::now().as_millisecond() + 86_400_000).to_string();
    let r = post_json(&watch, &t, &json!({ "id": "a", "type": "web_hook", "address": "http://127.0.0.1:1/", "expiration": far })).await;
    let exp: i64 = r.json["expiration"]
        .as_str()
        .expect("ms")
        .parse()
        .expect("ms");
    assert!(exp - Timestamp::now().as_millisecond() <= 60_000);
}

#[tokio::test]
async fn scripted_change_from_json_lands_and_notifies() {
    let g = start(Seed::default()).await;
    let t = g.access_token();
    let (_, sync) = read_all(&g, &t, "primary", "").await;
    let (hook, mut rx) = receiver().await;
    let watch = format!("{}/calendars/primary/events/watch", g.calendar_api());
    post_json(
        &watch,
        &t,
        &json!({ "id": "ch", "type": "web_hook", "address": hook }),
    )
    .await;
    assert_eq!(next_hook(&mut rx).await["x-goog-resource-state"], "sync");

    // Scenario-neutral script (plan 6.2): in 300 ms, change event X and notify.
    let steps: Vec<Step> = serde_json::from_value(json!([
        { "at_ms": 300, "op": "put", "calendar": "primary", "event": { "id": "x", "summary": "scripted", "start": { "date": "2026-11-12" }, "end": { "date": "2026-11-13" } } }
    ]))
    .expect("script JSON");
    let at = Instant::now();
    let done = g.run_script(steps);
    let h = next_hook(&mut rx).await;
    assert!(at.elapsed() >= Duration::from_millis(300));
    assert_eq!(h["x-goog-resource-state"], "exists");
    done.await.expect("script ran");
    let (delta, _) = read_all(&g, &t, "primary", &format!("syncToken={sync}")).await;
    assert_eq!(ids(&delta), ["x"]);
    assert_eq!(delta[0]["summary"], "scripted");
}

#[tokio::test]
async fn same_seed_gives_identical_state_and_responses() {
    let seed = Seed {
        seed: 42,
        calendars: 2,
        events: 40,
    };
    let (a, b) = (start(seed).await, start(seed).await);
    assert_eq!(a.calendars(), b.calendars());
    for cal in a.calendars() {
        assert_eq!(a.snapshot(&cal), b.snapshot(&cal), "calendar {cal}");
        let (ta, tb) = (a.access_token(), b.access_token());
        let ra = get(&events_url(&a, &cal, "maxResults=13"), &ta).await;
        let rb = get(&events_url(&b, &cal, "maxResults=13"), &tb).await;
        assert_eq!(ra.json, rb.json, "byte-identical bodies including tokens");
    }
    let c = start(Seed { seed: 43, ..seed }).await;
    assert_ne!(a.snapshot("primary"), c.snapshot("primary"));
}

#[tokio::test]
async fn authorization_code_grant_checks_pkce_and_is_single_use() {
    use base64::Engine as _;
    use sha2::Digest as _;
    let g = start(Seed::default()).await;
    let verifier = "a-verifier-of-at-least-forty-three-characters-xyz";
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let cb = "http://127.0.0.1:1/cb";
    let authorize = async || {
        let r = get_anon(&format!(
            "{}?response_type=code&client_id=c&redirect_uri={cb}&state=s%201\
             &code_challenge={challenge}&code_challenge_method=S256&scope=x",
            g.auth_url()
        ))
        .await;
        assert_eq!(r.status, 302);
        let to = r.headers["location"].to_str().expect("ascii").to_owned();
        let (base, query) = to.split_once('?').expect("query");
        assert_eq!(base, cb);
        assert!(query.ends_with("&state=s%201"), "{query}");
        query["code=".len()..query.find('&').expect("&")].to_owned()
    };
    let redeem = async |code: &str, verifier: &str| {
        post_form(
            &g.token_url(),
            &format!(
                "grant_type=authorization_code&code={code}&client_id=c&client_secret=s\
                 &redirect_uri={cb}&code_verifier={verifier}"
            ),
        )
        .await
    };

    let code = authorize().await;
    let r = redeem(&code, "wrong-verifier").await;
    assert_eq!(r.json["error"], "invalid_grant");
    let r = redeem(&code, verifier).await;
    assert_eq!(
        r.json["error"], "invalid_grant",
        "a failed redemption spends the code"
    );

    let code = authorize().await;
    let r = redeem(&code, verifier).await;
    assert_eq!(r.status, 200, "{}", r.json);
    assert_eq!(r.json["refresh_token"], g.refresh_token());
    let claims = r.json["id_token"]
        .as_str()
        .expect("id_token")
        .split('.')
        .nth(1)
        .expect("payload");
    let claims: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claims)
            .expect("b64"),
    )
    .expect("json");
    assert_eq!(claims["sub"], Config::default().subject);
    assert_eq!(claims["aud"], "c");
    let access = r.json["access_token"].as_str().expect("token");
    assert_eq!(
        get(&events_url(&g, "primary", ""), access).await.status,
        200
    );
    assert_eq!(
        redeem(&code, verifier).await.json["error"],
        "invalid_grant",
        "single use"
    );
}
