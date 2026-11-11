//! Transport against the fake Google server (`musubi_testkit::google`, K3).

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, Uri};
use jiff::{SignedDuration, Timestamp};
use musubi_google::{GoogleConfig, GoogleProvider};
use musubi_model::provider::{
    Access, Clock, FetchHint, Pass, PushHint, PushSource, Start, Subscription,
};
use musubi_model::remote::RemoteRef;
use musubi_model::{
    AccountCx, AccountId, ChangePage, CollectionRef, Cursor, Provider, PushRejected, RemoteKey,
    SyncError, TimeWindow,
};
use musubi_testkit::account_cx;
use musubi_testkit::google::{Change, Config, Endpoint, FakeGoogle, Fault, Seed};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// The fake stamps channel expirations with the wall clock.
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
    cal: CollectionRef,
}

async fn env(config: Config) -> Env {
    let g = FakeGoogle::start(config).await.unwrap();
    let mut google = GoogleConfig::new("client", SecretString::from("secret"));
    google.api_base = g.calendar_api().parse().unwrap();
    google.token_url = g.token_url().parse().unwrap();
    let p = GoogleProvider::new(google, Arc::new(WallClock)).unwrap();
    let mut cx = account_cx(AccountId::from_u128(1), Timestamp::now());
    cx.token = g.access_token().into();
    cx.clock = Arc::new(WallClock);
    let cal = CollectionRef(g.primary());
    Env { g, p, cx, cal }
}

fn seeded(events: u32) -> Config {
    Config {
        seed: Seed {
            seed: 7,
            calendars: 0,
            events,
        },
        ..Config::default()
    }
}

fn full() -> FetchHint {
    FetchHint {
        start: Start::Full,
        page_size: None,
    }
}

impl Env {
    async fn read(
        &self,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> Vec<Result<ChangePage, SyncError>> {
        self.p
            .changes(&self.cx, &self.cal, cursor, hint)
            .collect()
            .await
    }

    async fn read_ok(&self, cursor: Option<Cursor>, hint: FetchHint) -> Vec<ChangePage> {
        self.read(cursor, hint)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn put(&self, event: Value) -> String {
        self.g.put_event(&self.cal.0, event)
    }
}

fn timed(summary: &str, start: Timestamp) -> Value {
    let end = start + SignedDuration::from_hours(1);
    json!({
        "summary": summary,
        "start": { "dateTime": start.to_string(), "timeZone": "UTC" },
        "end": { "dateTime": end.to_string(), "timeZone": "UTC" },
    })
}

fn body(page_upsert: &musubi_model::RemoteObject) -> Value {
    serde_json::from_slice(&page_upsert.raw.body).unwrap()
}

/// Upserts and removals of a whole pass, in order, by key.
fn applied(pages: &[ChangePage]) -> (BTreeMap<String, Value>, Vec<String>) {
    let mut upserts = BTreeMap::new();
    let mut removals = Vec::new();
    for p in pages {
        for o in &p.upserts {
            upserts.insert(o.key.0.clone(), body(o));
        }
        removals.extend(p.removals.iter().map(|k| k.0.clone()));
    }
    (upserts, removals)
}

#[tokio::test]
async fn discover_lists_every_calendar() {
    let e = env(Config {
        seed: Seed {
            seed: 1,
            calendars: 2,
            events: 0,
        },
        ..Config::default()
    })
    .await;
    let hidden = "hidden@group.calendar.google.com";
    e.g.apply(&Change::AddCalendar {
        entry: json!({ "id": hidden, "summary": "Hidden", "hidden": true }),
    });
    let d = e.p.discover(&e.cx).await.unwrap();
    // The fake lists hidden calendars regardless; Google needs the parameter.
    assert!(
        e.g.requests()
            .iter()
            .all(|r| !r.contains("calendarList") || r.contains("showHidden=true"))
    );
    assert!(d.collections.iter().any(|c| c.remote.0 == hidden));
    let ids: Vec<_> = d.collections.iter().map(|c| c.remote.0.clone()).collect();
    assert_eq!(ids, e.g.calendars());
    let primary = &d.collections[0];
    assert!(primary.primary && primary.access == Access::Owner);
    assert_eq!(primary.time_zone.as_deref(), Some("Europe/Prague"));
    assert_eq!(d.collections[2].access, Access::Reader);
    assert_eq!(d.collections[1].name, "Calendar 1");
}

#[tokio::test]
async fn initial_window_reads_only_the_window() {
    let e = env(Config::default()).await;
    let epoch: Timestamp = "2026-11-11T00:00:00Z".parse().unwrap();
    let inside = e.put(timed("inside", epoch + SignedDuration::from_hours(5)));
    let outside = e.put(timed(
        "outside",
        epoch + SignedDuration::from_hours(24 * 60),
    ));
    let window = TimeWindow::new(
        epoch - SignedDuration::from_hours(24),
        epoch + SignedDuration::from_hours(24 * 30),
    )
    .unwrap();

    let pages = e
        .read_ok(
            None,
            FetchHint {
                start: Start::Window(window),
                page_size: None,
            },
        )
        .await;
    assert!(
        pages
            .iter()
            .all(|p| p.pass == Pass::Window && p.cursor.is_none())
    );
    assert!(pages.last().unwrap().last);
    let (upserts, removals) = applied(&pages);
    assert!(upserts.contains_key(&inside) && !upserts.contains_key(&outside));
    assert!(removals.is_empty());
    assert_eq!(upserts[&inside], e.g.snapshot(&e.cal.0)[&inside]);
}

#[tokio::test]
async fn full_read_pages_across_empty_pages_and_resumes_from_its_cursor() {
    let e = env(Config {
        max_page_size: 7,
        empty_first_page: true,
        empty_page_every: 2,
        ..seeded(40)
    })
    .await;
    let hint = FetchHint {
        start: Start::Full,
        page_size: NonZeroU32::new(5),
    };
    let pages = e.read_ok(None, hint).await;

    assert!(
        pages[0].upserts.is_empty() && !pages[0].last,
        "empty first page does not end the pass"
    );
    assert!(pages.len() > 2);
    let mid = &pages[1..pages.len() - 1];
    assert!(
        mid.iter().any(|p| p.upserts.is_empty()),
        "an empty page mid-read does not end the pass either"
    );
    assert!(
        pages
            .iter()
            .all(|p| p.pass == Pass::Full && p.cursor.is_some())
    );
    assert_eq!(pages.iter().filter(|p| p.last).count(), 1);
    assert!(pages.last().unwrap().last);
    assert!(e.g.requests().iter().any(|r| r.contains("maxResults=5")));

    // Every event as served (seed series carry cancelled instances), bytes intact.
    let (upserts, removals) = applied(&pages);
    assert_eq!(upserts, e.g.snapshot(&e.cal.0));
    assert!(removals.is_empty());

    // A crash after page 2: resuming from its cursor yields exactly the rest.
    let rest = e.read_ok(pages[1].cursor.clone(), hint).await;
    let keys = |ps: &[ChangePage]| -> Vec<String> {
        ps.iter()
            .flat_map(|p| p.upserts.iter().map(|o| o.key.0.clone()))
            .collect()
    };
    assert_eq!(keys(&rest), keys(&pages[2..]));
}

#[tokio::test]
async fn incremental_read_returns_only_what_changed() {
    let e = env(seeded(12)).await;
    let pages = e.read_ok(None, full()).await;
    let cursor = pages.last().unwrap().cursor.clone();
    let (before, _) = applied(&pages);
    let mut ids = before.keys().filter(|k| !k.contains('_'));
    let (gone, edited) = (ids.next().unwrap().clone(), ids.next().unwrap().clone());

    let epoch: Timestamp = "2026-11-11T00:00:00Z".parse().unwrap();
    let added = e.put(timed("added", epoch));
    e.g.delete_event(&e.cal.0, &gone);
    let mut changed = before[&edited].clone();
    changed["summary"] = json!("edited");
    e.put(changed);

    let delta = e.read_ok(cursor, full()).await;
    assert!(
        delta
            .iter()
            .all(|p| p.pass == Pass::Delta && p.cursor.is_some())
    );
    let (upserts, removals) = applied(&delta);
    assert_eq!(upserts.keys().collect::<Vec<_>>().len(), 2);
    assert!(upserts.contains_key(&added));
    assert_eq!(upserts[&edited]["summary"], "edited");
    assert_eq!(removals, [gone]);

    // Nothing new since: one empty, final page with a fresh cursor.
    let again = e
        .read_ok(delta.last().unwrap().cursor.clone(), full())
        .await;
    assert_eq!(again.len(), 1);
    assert!(again[0].last && again[0].upserts.is_empty() && again[0].removals.is_empty());
}

#[tokio::test]
async fn expired_sync_token_is_cursor_expired() {
    let e = env(seeded(3)).await;
    let cursor = e.read_ok(None, full()).await.pop().unwrap().cursor;
    e.g.apply(&Change::ExpireSyncTokens {
        calendar: e.cal.0.clone(),
    });
    let out = e.read(cursor, full()).await;
    assert_eq!(out.len(), 1, "the stream ends after the error");
    assert!(matches!(out[0], Err(SyncError::CursorExpired)));
}

#[tokio::test]
async fn rejected_page_token_of_a_stored_cursor_is_cursor_expired() {
    // A stored mid-read cursor whose page token Google no longer accepts (400).
    // K8: confirm the status Google answers for an expired page token.
    let e = env(seeded(3)).await;
    let stale = Cursor(br#"{"full":{"page":"no-longer-valid"}}"#.to_vec());
    let out = e.read(Some(stale), full()).await;
    assert_eq!(out.len(), 1);
    assert!(
        matches!(out[0], Err(SyncError::CursorExpired)),
        "{:?}",
        out[0]
    );
}

#[tokio::test]
async fn throttling_carries_retry_after_and_other_failures_map() {
    let e = env(seeded(3)).await;
    let first = |out: Vec<Result<ChangePage, SyncError>>| out.into_iter().next().unwrap();

    e.g.fail(Endpoint::EventsList, Fault::rate_limited(Some(7)), 1);
    let err = first(e.read(None, full()).await).unwrap_err();
    assert!(
        matches!(err, SyncError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(7)),
        "{err:?}"
    );

    e.g.fail(Endpoint::EventsList, Fault::forbidden_rate_limited(None), 1);
    let err = first(e.read(None, full()).await).unwrap_err();
    assert!(
        matches!(err, SyncError::RateLimited { retry_after: None }),
        "{err:?}"
    );

    e.g.fail(Endpoint::CalendarList, Fault::backend_error(), 1);
    assert!(matches!(
        e.p.discover(&e.cx).await,
        Err(SyncError::Transient(_))
    ));

    // The fault is spent; the next read succeeds.
    assert!(first(e.read(None, full()).await).is_ok());

    e.g.expire_access_tokens();
    assert!(matches!(
        e.p.discover(&e.cx).await,
        Err(SyncError::Unauthorized)
    ));
}

#[tokio::test]
async fn refresh_issues_tokens_and_revoked_grant_needs_reauth() {
    let e = env(Config {
        rotate_refresh_tokens: true,
        ..Config::default()
    })
    .await;
    e.g.expire_access_tokens();
    assert!(matches!(
        e.p.discover(&e.cx).await,
        Err(SyncError::Unauthorized)
    ));
    let old = e.g.refresh_token();
    let t = e.p.refresh(&SecretString::from(old.clone())).await.unwrap();
    assert_eq!(t.expires_in, Duration::from_secs(3599));
    let rotated = t
        .rotated_refresh
        .expect("rotation on")
        .expose_secret()
        .to_owned();
    assert_eq!(rotated, e.g.refresh_token());

    // The new access token works.
    let mut cx = account_cx(AccountId::from_u128(1), Timestamp::now());
    cx.token = t.access;
    assert!(e.p.discover(&cx).await.is_ok());

    // Rotation revoked the old refresh token; revocation kills the new one.
    let err = e.p.refresh(&SecretString::from(old)).await.unwrap_err();
    assert!(matches!(err, SyncError::ReauthRequired), "{err:?}");
    e.g.revoke_refresh_token(&rotated);
    let err = e.p.refresh(&SecretString::from(rotated)).await.unwrap_err();
    assert!(matches!(err, SyncError::ReauthRequired), "{err:?}");
}

#[tokio::test]
async fn fetch_returns_live_events_and_omits_the_rest() {
    let e = env(Config::default()).await;
    let epoch: Timestamp = "2026-11-11T00:00:00Z".parse().unwrap();
    let live = e.put(timed("live", epoch));
    let gone = e.put(timed("gone", epoch));
    e.g.delete_event(&e.cal.0, &gone);
    let r = |k: &str| RemoteRef {
        collection: e.cal.clone(),
        key: RemoteKey(k.into()),
    };
    let got =
        e.p.fetch(&e.cx, &[r(&live), r(&gone), r("unknown")])
            .await
            .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].key.0, live);
    assert_eq!(body(&got[0]), e.g.snapshot(&e.cal.0)[&live]);
}

/// Accepts webhook calls and hands over their headers.
async fn webhook() -> (Uri, mpsc::UnboundedReceiver<HeaderMap>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/hook", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let text = String::from_utf8_lossy(&buf);
            let headers: HeaderMap = text
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(':'))
                .filter_map(|(k, v)| {
                    Some((
                        HeaderName::try_from(k.trim()).ok()?,
                        HeaderValue::try_from(v.trim()).ok()?,
                    ))
                })
                .collect();
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
            let _ = tx.send(headers);
        }
    });
    (uri, rx)
}

async fn next(rx: &mut mpsc::UnboundedReceiver<HeaderMap>) -> HeaderMap {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("notification within 5 s")
        .unwrap()
}

#[tokio::test]
async fn watch_create_renew_cancel_and_verify() {
    let ttl = Duration::from_secs(3600);
    let e = env(Config {
        max_watch_ttl: ttl,
        ..Config::default()
    })
    .await;
    let (hook, mut rx) = webhook().await;

    // Create: the expiration is the one the server granted (1 h here), not a constant.
    let a: Subscription = e.p.subscribe(&e.cx, &e.cal, &hook).await.unwrap();
    let granted = a.expiration.duration_since(Timestamp::now());
    assert!(
        granted > SignedDuration::from_mins(59) && granted <= SignedDuration::from_mins(60),
        "{granted:?}"
    );
    assert_eq!(a.collection, e.cal);

    let sync = next(&mut rx).await;
    assert_eq!(e.p.channel_of(&sync), Some(a.channel.clone()));
    assert_eq!(e.p.verify(&sync, &a), Ok(PushHint::Handshake));
    e.put(timed("x", "2026-11-11T00:00:00Z".parse().unwrap()));
    let exists = next(&mut rx).await;
    assert_eq!(e.p.verify(&exists, &a), Ok(PushHint::Changed));

    // Renew before `a.expiration`: open a successor, then stop the old channel.
    let b = e.p.subscribe(&e.cx, &e.cal, &hook).await.unwrap();
    assert_ne!(a.channel, b.channel);
    assert_ne!(a.token.expose_secret(), b.token.expose_secret());
    let b_sync = next(&mut rx).await;
    assert_eq!(e.p.verify(&b_sync, &b), Ok(PushHint::Handshake));
    e.p.unsubscribe(&e.cx, &a).await.unwrap();
    assert_eq!(e.g.channels(), vec![b.channel.0.clone()]);
    // Stopping again (unknown channel) is fine.
    e.p.unsubscribe(&e.cx, &a).await.unwrap();

    // A's channel id carrying B's token is forged; B's notification is not A's.
    let mut forged = exists.clone();
    forged.insert(
        "x-goog-channel-token",
        b.token.expose_secret().parse().unwrap(),
    );
    assert_eq!(e.p.verify(&forged, &a), Err(PushRejected::BadToken));
    assert_eq!(e.p.verify(&b_sync, &a), Err(PushRejected::UnknownChannel));

    e.p.unsubscribe(&e.cx, &b).await.unwrap();
    assert!(e.g.channels().is_empty());
}
