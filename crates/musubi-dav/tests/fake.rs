//! `musubi-dav` against the fake DAV server (plan, checkpoint K4).

use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::StreamExt;
use jiff::Timestamp;
use musubi_dav::{ConnectError, DavConfig, DavProvider, DavSetup, Principals};
use musubi_model::capability::DeltaSupport;
use musubi_model::provider::{Access, Discovery, FetchHint, Pass, Start};
use musubi_model::remote::RemoteRef;
use musubi_model::{
    AccountCx, AccountId, ChangePage, CollectionRef, Credential, Cursor, EntityKind, Provider,
    ReauthReason, RemoteKey, SyncError, TimeWindow,
};
use musubi_testkit::dav::{Config, FakeDav, Fault, Host, Kind, Quirk, Req};
use secrecy::SecretString;

const PASSWORD: &str = "secret";

fn event(uid: &str, day: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20261101T000000Z\r\nDTSTART:{day}T100000Z\r\nDTEND:{day}T110000Z\r\n\
         SUMMARY:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn card(uid: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:{uid}\r\nEND:VCARD\r\n")
}

fn cx(setup: &DavSetup, password: &str) -> AccountCx {
    let mut cx = musubi_testkit::account_cx(AccountId::from_u128(1), Timestamp::now());
    cx.credential = Credential::Basic {
        username: "alice".into(),
        password: SecretString::from(password),
    };
    cx.endpoint = Some(setup.endpoint().unwrap());
    cx
}

fn setup(url: String) -> DavSetup {
    DavSetup {
        server_url: Some(url),
        ..DavSetup::default()
    }
}

fn rref(collection: CollectionRef, key: RemoteKey) -> RemoteRef {
    RemoteRef { collection, key }
}

fn provider() -> DavProvider {
    DavProvider::new(DavConfig {
        allow_private: true,
    })
    .unwrap()
}

/// Connects and stores the endpoint in the context, as the server does.
async fn connected(p: &DavProvider, setup: &DavSetup) -> (AccountCx, Discovery) {
    let mut cx = cx(setup, PASSWORD);
    let d = p.connect(&cx).await.unwrap();
    cx.endpoint = d.endpoint.clone();
    (cx, d)
}

async fn rediscover(p: &DavProvider, cx: &mut AccountCx) -> Discovery {
    let d = p.discover(cx).await.unwrap();
    cx.endpoint = d.endpoint.clone();
    d
}

fn col<'a>(d: &'a Discovery, key: &str) -> &'a musubi_model::provider::RemoteCollection {
    d.collections
        .iter()
        .find(|c| c.remote.0 == key)
        .unwrap_or_else(|| panic!("{key} not discovered"))
}

async fn read(
    p: &DavProvider,
    cx: &AccountCx,
    key: &str,
    cursor: Option<Cursor>,
    start: Start,
    page: u32,
) -> Result<Vec<ChangePage>, SyncError> {
    let mut hint = FetchHint::new(start);
    hint.page_size = NonZeroU32::new(page);
    let c = CollectionRef(key.to_owned());
    p.changes(cx, &c, cursor, hint)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

async fn full(p: &DavProvider, cx: &AccountCx, key: &str) -> Vec<ChangePage> {
    read(p, cx, key, None, Start::Full, 500).await.unwrap()
}

async fn err(p: &DavProvider, cx: &AccountCx, key: &str, cursor: Option<Cursor>) -> SyncError {
    read(p, cx, key, cursor, Start::Full, 500)
        .await
        .unwrap_err()
}

fn keys(pages: &[ChangePage]) -> Vec<String> {
    let mut k: Vec<String> = pages
        .iter()
        .flat_map(|p| p.stubs.iter().map(|s| s.key.0.clone()))
        .collect();
    k.sort();
    k
}

fn last_cursor(pages: &[ChangePage]) -> Cursor {
    let l = pages.last().unwrap();
    assert!(l.last);
    l.cursor.clone().unwrap()
}

/// Every page but the last is not last; exactly one last page at the end.
fn well_formed(pages: &[ChangePage]) {
    let (l, rest) = pages.split_last().unwrap();
    assert!(l.last);
    assert!(rest.iter().all(|p| !p.last));
}

async fn fake() -> FakeDav {
    FakeDav::start(Config::default()).await.unwrap()
}

#[tokio::test]
async fn discovers_through_well_known_and_a_direct_url() {
    let dav = fake().await;
    let ev = dav.add_collection(Kind::Calendar(&["VEVENT"]), "work", "Work");
    let todo = dav.add_collection(Kind::Calendar(&["VTODO"]), "todo", "Todo");
    let all = dav.add_collection(Kind::Calendar(&[]), "all", "All");
    let journal = dav.add_collection(Kind::Calendar(&["VJOURNAL"]), "journal", "J");
    let ro = dav.add_collection(Kind::Calendar(&["VEVENT"]), "shared", "Shared");
    let ctag = dav.add_collection(Kind::Calendar(&["VEVENT"]), "ctag", "Ctag");
    let none = dav.add_collection(Kind::Calendar(&["VEVENT"]), "none", "None");
    let sub = dav.add_collection(Kind::Subscribed, "holidays", "Holidays");
    let book = dav.add_collection(Kind::AddressBook, "contacts", "Contacts");
    dav.quirk(&ro, Quirk::ReadOnly);
    dav.quirk(&ctag, Quirk::NoSync);
    dav.quirk(&none, Quirk::NoSync);
    dav.quirk(&none, Quirk::NoCtag);
    let p = provider();

    let (_, d) = connected(&p, &setup(dav.server_url())).await;
    assert_eq!(
        dav.count("PROPFIND"),
        dav.requests()
            .iter()
            .filter(|r| r.method == "PROPFIND")
            .count()
    );
    assert!(
        dav.requests()
            .iter()
            .any(|r| r.path == "/.well-known/caldav")
    );
    let mut found: Vec<&str> = d.collections.iter().map(|c| c.remote.0.as_str()).collect();
    found.sort();
    let mut want = vec![&ev, &todo, &all, &ro, &ctag, &none, &book];
    want.sort();
    assert_eq!(found, want);
    for skipped in [&journal, &sub] {
        assert!(!found.contains(&skipped.as_str()));
    }
    assert!(
        !found
            .iter()
            .any(|k| k.ends_with("/inbox/") || k.ends_with("/outbox/"))
    );
    let c = col(&d, &ev);
    assert_eq!(
        (c.kind, c.name.as_str(), c.access),
        (EntityKind::Event, "Work", Access::Writer)
    );
    assert_eq!(c.delta, Some(DeltaSupport::SyncToken));
    assert_eq!(col(&d, &todo).kind, EntityKind::Task);
    assert_eq!(col(&d, &all).kind, EntityKind::Event);
    assert_eq!(col(&d, &ro).access, Access::Reader);
    assert_eq!(col(&d, &ctag).delta, Some(DeltaSupport::CtagEtag));
    assert_eq!(col(&d, &none).delta, Some(DeltaSupport::None));
    assert_eq!(col(&d, &book).kind, EntityKind::Contact);
    let principals = Principals::of(d.endpoint.as_ref().unwrap());
    let principal = format!("{}{}", dav.url(Host::Caldav), dav.principal());
    assert_eq!(principals.subject(), Some(principal.as_str()));
    assert_eq!(principals.carddav.as_deref(), Some(principal.as_str()));

    // A direct URL (non-empty path) skips the well-known lookup.
    dav.clear_requests();
    let direct = format!("{}{}", dav.url(Host::Caldav), dav.calendar_home());
    let (_, d2) = connected(&p, &setup(direct)).await;
    assert!(
        !dav.requests()
            .iter()
            .any(|r| r.path.starts_with("/.well-known/"))
    );
    assert_eq!(d2.collections.len(), d.collections.len());
    assert_eq!(Principals::of(d2.endpoint.as_ref().unwrap()), principals);
}

#[tokio::test]
async fn discovers_split_caldav_and_carddav_hosts() {
    let dav = FakeDav::start(Config {
        split: true,
        ..Config::default()
    })
    .await
    .unwrap();
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    let book = dav.add_collection(Kind::AddressBook, "contacts", "Contacts");
    dav.put(&book, "c1.vcf", &card("c1"));
    let p = provider();
    let s = DavSetup {
        caldav_url: Some(dav.server_url()),
        carddav_url: Some(dav.carddav_url()),
        ..DavSetup::default()
    };
    let (cx, d) = connected(&p, &s).await;
    col(&d, &cal);
    assert_eq!(col(&d, &book).kind, EntityKind::Contact);
    let pr = Principals::of(d.endpoint.as_ref().unwrap());
    assert_eq!(
        pr.caldav,
        Some(format!("{}{}", dav.url(Host::Caldav), dav.principal()))
    );
    assert_eq!(
        pr.carddav,
        Some(format!("{}{}", dav.url(Host::Carddav), dav.principal()))
    );
    // The address book is read on its own host.
    dav.clear_requests();
    let pages = full(&p, &cx, &book).await;
    assert_eq!(keys(&pages), [format!("{book}c1.vcf")]);
    assert!(dav.requests().iter().all(|r| r.host == Host::Carddav));
    let refs = [rref(
        CollectionRef(book.clone()),
        RemoteKey(format!("{book}c1.vcf")),
    )];
    let objs = p.fetch(&cx, &refs).await.unwrap();
    assert_eq!(objs[0].kind, EntityKind::Contact);
    // A CardDAV-only account takes its subject from the CardDAV principal.
    let s = DavSetup {
        carddav_url: Some(dav.carddav_url()),
        ..DavSetup::default()
    };
    let (_, d) = connected(&p, &s).await;
    let pr = Principals::of(d.endpoint.as_ref().unwrap());
    assert_eq!(pr.caldav, None);
    assert_eq!(pr.subject(), pr.carddav.as_deref());
}

#[tokio::test]
async fn sync_token_level_window_full_and_delta() {
    let dav = fake().await;
    let c = dav.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    dav.put(&c, "in.ics", &event("in", "20261113"));
    dav.put(&c, "old.ics", &event("old", "20250101"));
    dav.put(&c, "x.ics", &event("x", "20261120"));
    let p = provider();
    let (cx, _) = connected(&p, &setup(dav.server_url())).await;

    let w = TimeWindow::new(
        "2026-11-01T00:00:00Z".parse().unwrap(),
        "2026-12-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    dav.clear_requests();
    let pages = read(&p, &cx, &c, None, Start::Window(w), 1).await.unwrap();
    well_formed(&pages);
    assert_eq!(dav.count("REPORT calendar-query"), 1);
    assert!(
        pages
            .iter()
            .all(|p| p.pass == Pass::Window && p.cursor.is_none() && p.stubs.is_empty())
    );
    let mut got: Vec<&str> = pages
        .iter()
        .flat_map(|p| &p.upserts)
        .map(|o| o.key.0.as_str())
        .collect();
    got.sort();
    assert_eq!(got, [format!("{c}in.ics"), format!("{c}x.ics")]);
    assert_eq!(pages.len(), 2);

    dav.clear_requests();
    let pages = read(&p, &cx, &c, None, Start::Full, 2).await.unwrap();
    assert_eq!(dav.count("REPORT sync-collection"), 1);
    well_formed(&pages);
    assert_eq!(pages.len(), 2);
    assert!(
        pages
            .iter()
            .all(|p| p.pass == Pass::Full && p.upserts.is_empty())
    );
    assert_eq!(keys(&pages).len(), 3);
    assert!(
        pages
            .iter()
            .flat_map(|p| &p.stubs)
            .all(|s| s.validator.is_some())
    );
    let cursor = last_cursor(&pages);

    dav.put(&c, "new.ics", &event("new", "20261114"));
    dav.delete(&format!("{c}old.ics"));
    let pages = read(&p, &cx, &c, Some(cursor.clone()), Start::Full, 500)
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].pass, Pass::Delta);
    assert_eq!(keys(&pages), [format!("{c}new.ics")]);
    assert_eq!(pages[0].removals, [RemoteKey(format!("{c}old.ics"))]);
    let next = last_cursor(&pages);
    assert_ne!(next, cursor);
    // Nothing changed since: an empty last delta page.
    let pages = read(&p, &cx, &c, Some(next), Start::Full, 500)
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert!(pages[0].stubs.is_empty() && pages[0].removals.is_empty() && pages[0].last);

    // Tasks and contacts never take the window: a full read.
    let t = dav.add_collection(Kind::Calendar(&["VTODO"]), "todo", "Todo");
    let mut cx = cx;
    rediscover(&p, &mut cx).await;
    let pages = read(&p, &cx, &t, None, Start::Window(w), 500)
        .await
        .unwrap();
    assert_eq!(pages[0].pass, Pass::Full);
}

#[tokio::test]
async fn ctag_level_cheap_check_is_one_request() {
    let dav = fake().await;
    let c = dav.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    dav.quirk(&c, Quirk::NoSync);
    dav.put(&c, "a.ics", &event("a", "20261113"));
    dav.put(&c, "b.ics", &event("b", "20261114"));
    let p = provider();
    let (cx, d) = connected(&p, &setup(dav.server_url())).await;
    assert_eq!(col(&d, &c).delta, Some(DeltaSupport::CtagEtag));

    dav.clear_requests();
    let pages = read(&p, &cx, &c, None, Start::Full, 1).await.unwrap();
    well_formed(&pages);
    assert_eq!(pages.len(), 2);
    assert_eq!(dav.count("PROPFIND"), 2);
    assert_eq!(dav.count("REPORT"), 0);
    assert!(pages.iter().all(|p| p.pass == Pass::Full));
    assert_eq!(keys(&pages), [format!("{c}a.ics"), format!("{c}b.ics")]);
    let cursor = last_cursor(&pages);

    dav.clear_requests();
    let pages = read(&p, &cx, &c, Some(cursor.clone()), Start::Full, 500)
        .await
        .unwrap();
    assert_eq!(dav.requests().len(), 1);
    assert_eq!(dav.requests()[0].depth.as_deref(), Some("0"));
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].pass, Pass::Delta);
    assert!(pages[0].stubs.is_empty() && pages[0].last);
    assert_eq!(pages[0].cursor.as_ref(), Some(&cursor));

    dav.put(&c, "c.ics", &event("c", "20261115"));
    dav.clear_requests();
    let pages = read(&p, &cx, &c, Some(cursor.clone()), Start::Full, 500)
        .await
        .unwrap();
    assert_eq!(dav.count("PROPFIND"), 2);
    assert_eq!(pages[0].pass, Pass::Full);
    assert_eq!(keys(&pages).len(), 3);
    assert_ne!(last_cursor(&pages), cursor);
}

#[tokio::test]
async fn no_delta_level_lists_etags_every_time() {
    let dav = fake().await;
    let c = dav.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    dav.quirk(&c, Quirk::NoSync);
    dav.quirk(&c, Quirk::NoCtag);
    dav.put(&c, "a.ics", &event("a", "20261113"));
    let p = provider();
    let (cx, d) = connected(&p, &setup(dav.server_url())).await;
    assert_eq!(col(&d, &c).delta, Some(DeltaSupport::None));
    let pages = full(&p, &cx, &c).await;
    assert_eq!(pages[0].pass, Pass::Full);
    let cursor = last_cursor(&pages);
    dav.clear_requests();
    let pages = read(&p, &cx, &c, Some(cursor), Start::Full, 500)
        .await
        .unwrap();
    assert_eq!(dav.requests().len(), 1);
    assert_eq!(dav.requests()[0].depth.as_deref(), Some("1"));
    assert_eq!(pages[0].pass, Pass::Full);
    assert_eq!(keys(&pages), [format!("{c}a.ics")]);
}

/// A connected account with one synced calendar and its delta cursor.
async fn synced(dav: &FakeDav) -> (DavProvider, AccountCx, String, Cursor) {
    let c = dav.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    for i in 0..5 {
        dav.put(
            &c,
            &format!("{i}.ics"),
            &event(&format!("e{i}"), "20261113"),
        );
    }
    let p = provider();
    let (cx, _) = connected(&p, &setup(dav.server_url())).await;
    let cursor = last_cursor(&full(&p, &cx, &c).await);
    (p, cx, c, cursor)
}

#[tokio::test]
async fn invalid_sync_token_is_cursor_expired() {
    let dav = fake().await;
    let (p, cx, c, cursor) = synced(&dav).await;
    for status in [403, 409] {
        dav.fail(
            Req::Report("sync-collection"),
            Fault::InvalidSyncToken { status },
            1,
        );
        let e = err(&p, &cx, &c, Some(cursor.clone())).await;
        assert!(matches!(e, SyncError::CursorExpired), "{status}: {e:?}");
    }
    dav.expire_sync_tokens(&c);
    assert!(matches!(
        err(&p, &cx, &c, Some(cursor)).await,
        SyncError::CursorExpired
    ));
}

#[tokio::test]
async fn truncated_multistatus_is_a_page_that_is_not_last() {
    let dav = fake().await;
    let (p, cx, c, cursor) = synced(&dav).await;
    dav.fail(Req::Report("sync-collection"), Fault::Truncate(2), 2);
    dav.clear_requests();
    let pages = full(&p, &cx, &c).await;
    assert_eq!(dav.count("REPORT sync-collection"), 3);
    well_formed(&pages);
    assert_eq!(pages.len(), 3);
    assert_eq!(keys(&pages).len(), 5);

    for i in 5..8 {
        dav.put(
            &c,
            &format!("{i}.ics"),
            &event(&format!("e{i}"), "20261113"),
        );
    }
    dav.fail(Req::Report("sync-collection"), Fault::Truncate(2), 1);
    let pages = read(&p, &cx, &c, Some(cursor), Start::Full, 500)
        .await
        .unwrap();
    well_formed(&pages);
    assert_eq!(pages.len(), 2);
    assert!(pages.iter().all(|p| p.pass == Pass::Delta));
    assert_eq!(keys(&pages).len(), 3);
}

#[tokio::test]
async fn statuses_map_to_sync_errors() {
    let dav = fake().await;
    let (p, cx, c, cursor) = synced(&dav).await;
    let status = |s: u16, r: Option<u64>| Fault::Status {
        status: s,
        retry_after: r,
    };
    let sync = Req::Report("sync-collection");

    dav.fail(sync.clone(), status(401, None), 1);
    let e = err(&p, &cx, &c, Some(cursor.clone())).await;
    assert!(
        matches!(
            e,
            SyncError::ReauthRequired {
                reason: ReauthReason::Rejected
            }
        ),
        "{e:?}"
    );

    // A bare 403 is a permission problem: no descent.
    dav.fail(sync.clone(), status(403, None), 1);
    assert!(matches!(
        err(&p, &cx, &c, Some(cursor.clone())).await,
        SyncError::Permanent(_)
    ));
    assert!(
        p.discover(&cx)
            .await
            .unwrap()
            .collections
            .iter()
            .all(|c| c.delta == Some(DeltaSupport::SyncToken))
    );

    dav.fail(sync.clone(), status(404, None), 1);
    assert!(matches!(
        err(&p, &cx, &c, Some(cursor.clone())).await,
        SyncError::Rediscover
    ));
    assert!(matches!(
        err(&p, &cx, "/calendars/alice/gone/", None).await,
        SyncError::Rediscover
    ));

    dav.fail(sync.clone(), status(429, Some(7)), 1);
    let e = err(&p, &cx, &c, Some(cursor.clone())).await;
    assert!(
        matches!(e, SyncError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(7)),
        "{e:?}"
    );
    dav.fail(sync.clone(), status(503, Some(3)), 1);
    let e = err(&p, &cx, &c, Some(cursor.clone())).await;
    assert!(
        matches!(e, SyncError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(3)),
        "{e:?}"
    );

    for s in [500, 503] {
        dav.fail(sync.clone(), status(s, None), 1);
        let e = err(&p, &cx, &c, Some(cursor.clone())).await;
        assert!(matches!(e, SyncError::Transient(_)), "{s}: {e:?}");
    }

    dav.fail(sync.clone(), Fault::Garbage, 1);
    assert!(matches!(
        err(&p, &cx, &c, Some(cursor.clone())).await,
        SyncError::Permanent(_)
    ));
    dav.fail(Req::Report("calendar-multiget"), Fault::Garbage, 1);
    let refs = [rref(
        CollectionRef(c.clone()),
        RemoteKey(format!("{c}0.ics")),
    )];
    assert!(matches!(
        p.fetch(&cx, &refs).await,
        Err(SyncError::Permanent(_))
    ));

    // A network error (nothing listens any more) is transient.
    drop(dav);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let e = err(&p, &cx, &c, Some(cursor)).await;
    assert!(matches!(e, SyncError::Transient(_)), "{e:?}");
}

#[tokio::test]
async fn redirects_inside_and_outside_the_allowed_origins() {
    let dav = fake().await;
    let (p, cx, c, cursor) = synced(&dav).await;
    let sync = Req::Report("sync-collection");
    for status in [301, 308] {
        dav.fail(
            sync.clone(),
            Fault::Redirect {
                status,
                to: Host::Caldav,
            },
            1,
        );
        let e = err(&p, &cx, &c, Some(cursor.clone())).await;
        assert!(matches!(e, SyncError::Rediscover), "{status}: {e:?}");
    }
    for status in [301, 302, 307, 308] {
        dav.fail(
            sync.clone(),
            Fault::Redirect {
                status,
                to: Host::Outside,
            },
            1,
        );
        let e = err(&p, &cx, &c, Some(cursor.clone())).await;
        assert!(
            matches!(
                e,
                SyncError::ReauthRequired {
                    reason: ReauthReason::EndpointMoved
                }
            ),
            "{status}: {e:?}"
        );
    }
    // The target got no request at all, so no Authorization either.
    assert!(dav.requests().iter().all(|r| r.host != Host::Outside));

    // 302/307 inside the origins are followed with the password.
    dav.fail(
        Req::Report("calendar-multiget"),
        Fault::Redirect {
            status: 307,
            to: Host::Caldav,
        },
        1,
    );
    let refs = [rref(
        CollectionRef(c.clone()),
        RemoteKey(format!("{c}0.ics")),
    )];
    assert_eq!(p.fetch(&cx, &refs).await.unwrap().len(), 1);
}

#[tokio::test]
async fn delta_descent_is_stored_and_survives_rediscovery() {
    for status in [405, 501] {
        let dav = fake().await;
        let (p, mut cx, c, cursor) = synced(&dav).await;
        dav.fail(
            Req::Report("sync-collection"),
            Fault::Status {
                status,
                retry_after: None,
            },
            1,
        );
        let e = err(&p, &cx, &c, Some(cursor.clone())).await;
        assert!(matches!(e, SyncError::Rediscover), "{status}: {e:?}");
        let d = rediscover(&p, &mut cx).await;
        assert_eq!(col(&d, &c).delta, Some(DeltaSupport::CtagEtag));
        let state = &cx.endpoint.as_ref().unwrap().state;
        assert_eq!(state["delta_override"][&c], "CtagEtag", "{state}");
    }
    let dav = fake().await;
    let (p, mut cx, c, cursor) = synced(&dav).await;
    dav.fail(
        Req::Report("sync-collection"),
        Fault::Status {
            status: 405,
            retry_after: None,
        },
        1,
    );
    err(&p, &cx, &c, Some(cursor.clone())).await;
    rediscover(&p, &mut cx).await;
    // The server still advertises sync-collection; the descent stays.
    let d = rediscover(&p, &mut cx).await;
    assert_eq!(col(&d, &c).delta, Some(DeltaSupport::CtagEtag));
    // Reads now go by ctag, and an old sync token starts a listing.
    dav.clear_requests();
    let pages = read(&p, &cx, &c, Some(cursor), Start::Full, 500)
        .await
        .unwrap();
    assert_eq!(dav.count("REPORT"), 0);
    assert_eq!(pages[0].pass, Pass::Full);
    assert_eq!(keys(&pages).len(), 5);
    // A new connect drops it.
    let (_, d) = connected(&p, &setup(dav.server_url())).await;
    assert_eq!(col(&d, &c).delta, Some(DeltaSupport::SyncToken));
}

#[tokio::test]
async fn multiget_goes_in_batches_of_max_batch() {
    let dav = fake().await;
    let c = dav.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    let book = dav.add_collection(Kind::AddressBook, "contacts", "Contacts");
    for i in 0..120 {
        dav.put(
            &c,
            &format!("{i}.ics"),
            &event(&format!("e{i}"), "20261113"),
        );
    }
    dav.put(&book, "c.vcf", &card("c"));
    let p = provider();
    let (cx, _) = connected(&p, &setup(dav.server_url())).await;
    assert_eq!(p.descriptor().limits.max_batch, Some(50));
    let mut refs: Vec<RemoteRef> = (0..120)
        .map(|i| rref(CollectionRef(c.clone()), RemoteKey(format!("{c}{i}.ics"))))
        .collect();
    refs.push(rref(
        CollectionRef(book.clone()),
        RemoteKey(format!("{book}c.vcf")),
    ));
    dav.clear_requests();
    let objs = p.fetch(&cx, &refs).await.unwrap();
    assert_eq!(objs.len(), 121);
    assert_eq!(dav.count("REPORT calendar-multiget"), 3);
    assert_eq!(dav.count("REPORT addressbook-multiget"), 1);
    let o = objs
        .iter()
        .find(|o| o.key.0 == format!("{c}7.ics"))
        .unwrap();
    assert_eq!(o.kind, EntityKind::Event);
    assert_eq!(Some(o.validator.0.clone()), dav.etag(&o.key.0));
    assert_eq!(o.raw.body, event("e7", "20261113").as_bytes());
    assert_eq!(
        objs.iter()
            .find(|o| o.key.0.ends_with("c.vcf"))
            .unwrap()
            .kind,
        EntityKind::Contact
    );

    // Deleted between listing and fetch: left out. No ETag: a body hash.
    dav.fail(
        Req::Report("calendar-multiget"),
        Fault::DeleteBeforeMultiget,
        1,
    );
    assert!(p.fetch(&cx, &refs[..2]).await.unwrap().is_empty());
    dav.quirk(&c, Quirk::NoEtag);
    let objs = p.fetch(&cx, &refs[2..3]).await.unwrap();
    assert!(
        objs[0].validator.0.starts_with("musubi:sha256:"),
        "{:?}",
        objs[0].validator
    );
}

#[tokio::test]
async fn hrefs_are_percent_decoded_keys() {
    let dav = fake().await;
    let c = dav.add_collection(Kind::Calendar(&["VEVENT"]), "my cal", "Cal");
    let p = provider();
    let (cx, d) = connected(&p, &setup(dav.server_url())).await;
    assert_eq!(c, "/calendars/alice/my cal/");
    col(&d, &c);
    let cursor = last_cursor(&full(&p, &cx, &c).await);
    let obj = dav.put(&c, "a@b c.ics", &event("ab", "20261113"));
    assert_eq!(obj, "/calendars/alice/my cal/a@b c.ics");
    let pages = read(&p, &cx, &c, Some(cursor.clone()), Start::Full, 500)
        .await
        .unwrap();
    assert_eq!(keys(&pages), std::slice::from_ref(&obj));
    assert_eq!(keys(&full(&p, &cx, &c).await), std::slice::from_ref(&obj));
    let refs = [rref(CollectionRef(c.clone()), RemoteKey(obj.clone()))];
    let objs = p.fetch(&cx, &refs).await.unwrap();
    assert_eq!(objs[0].key.0, obj);
    let next = last_cursor(&pages);
    dav.delete(&obj);
    let pages = read(&p, &cx, &c, Some(next), Start::Full, 500)
        .await
        .unwrap();
    assert_eq!(pages[0].removals, [RemoteKey(obj)]);
    assert!(dav.requests().iter().all(|r| !r.path.contains(' ')));
}

#[tokio::test]
async fn connect_errors() {
    let dav = fake().await;
    dav.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    let p = provider();
    let s = setup(dav.server_url());

    let e = p.connect(&cx(&s, "wrong")).await.unwrap_err();
    assert!(matches!(e, ConnectError::Unauthorized), "{e:?}");
    assert!(!format!("{e} {e:?}").contains("wrong"));
    let c = cx(&s, "wrong");
    assert!(!format!("{:?} {:?}", c.credential, c).contains("wrong"));
    let e = p.discover(&c).await.unwrap_err();
    assert!(matches!(
        e,
        SyncError::ReauthRequired {
            reason: ReauthReason::Rejected
        }
    ));

    let e = p
        .connect(&cx(
            &setup(format!("{}/", dav.url(Host::Outside))),
            PASSWORD,
        ))
        .await
        .unwrap_err();
    assert!(matches!(e, ConnectError::NotDav(_)), "{e:?}");

    dav.clear_requests();
    dav.fail(
        Req::Method("PROPFIND"),
        Fault::Redirect {
            status: 302,
            to: Host::Outside,
        },
        1,
    );
    let e = p.connect(&cx(&s, PASSWORD)).await.unwrap_err();
    let ConnectError::CrossOriginRedirect(target) = e else {
        panic!("{e:?}")
    };
    assert!(target.starts_with(&dav.url(Host::Outside)), "{target}");
    assert!(dav.requests().iter().all(|r| r.host != Host::Outside));

    // The user confirms the target: the password may follow it now.
    let confirmed = DavSetup {
        allow_origins: vec![dav.url(Host::Outside)],
        ..s.clone()
    };
    dav.fail(
        Req::Method("PROPFIND"),
        Fault::Redirect {
            status: 307,
            to: Host::Outside,
        },
        1,
    );
    // The well-known lookup lands on Outside (404) and falls back to the entry.
    p.connect(&cx(&confirmed, PASSWORD)).await.unwrap();
    let outside: Vec<_> = dav
        .requests()
        .into_iter()
        .filter(|r| r.host == Host::Outside)
        .collect();
    assert_eq!(outside.len(), 1);
    assert!(outside[0].auth);

    // A reconnect must find the same principals.
    let (cx1, _) = connected(&p, &s).await;
    let other = fake().await;
    other.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    let (cx2, _) = connected(&p, &setup(other.server_url())).await;
    let (e1, e2) = (
        cx1.endpoint.as_ref().unwrap(),
        cx2.endpoint.as_ref().unwrap(),
    );
    assert!(Principals::check_reconnect(e1, e1).is_ok());
    assert!(matches!(
        Principals::check_reconnect(e1, e2),
        Err(ConnectError::SubjectMismatch)
    ));
}

/// A server that answers every connection with `307` to `location`.
async fn redirector(location: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let r = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = s.write_all(r.as_bytes()).await;
        }
    });
    url
}

#[tokio::test]
async fn address_guard() {
    let dav = fake().await;
    dav.add_collection(Kind::Calendar(&["VEVENT"]), "cal", "Cal");
    let port = dav.url(Host::Caldav).rsplit(':').next().unwrap().to_owned();
    let strict = DavProvider::new(DavConfig::default()).unwrap();
    let lax = provider();
    let blocked =
        |r: Result<Discovery, ConnectError>| matches!(r, Err(ConnectError::BlockedAddress(_)));

    // A DNS name that resolves to 127.0.0.1: refused unless allow_private.
    let https_name = setup(format!("https://localhost:{port}/"));
    assert!(blocked(strict.connect(&cx(&https_name, PASSWORD)).await));
    let http_name = setup(format!("http://localhost:{port}/"));
    assert!(blocked(strict.connect(&cx(&http_name, PASSWORD)).await));
    // Loopback and private literals likewise; nothing reached the server.
    for u in [
        format!("https://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/"),
        "https://10.1.2.3/".into(),
        "https://[::1]/".into(),
    ] {
        assert!(
            blocked(strict.connect(&cx(&setup(u.clone()), PASSWORD)).await),
            "{u}"
        );
    }
    assert!(dav.requests().is_empty());
    // allow_private: loopback over http works, through the name as well.
    let (_, d) = connected(&lax, &http_name).await;
    assert_eq!(d.collections.len(), 1);
    // http elsewhere never, not even with allow_private.
    assert!(blocked(
        lax.connect(&cx(&setup("http://192.0.2.1/".into()), PASSWORD))
            .await
    ));
    // Nor after a redirect, even to a confirmed origin.
    let via = redirector("http://192.0.2.1/dav/").await;
    let s = DavSetup {
        allow_origins: vec!["http://192.0.2.1".into()],
        ..setup(via)
    };
    assert!(blocked(lax.connect(&cx(&s, PASSWORD)).await));
}
