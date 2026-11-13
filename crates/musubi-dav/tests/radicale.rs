//! `musubi-dav` against the pinned Radicale (plan, checkpoint K4). Needs
//! `MUSUBI_RADICALE`; run by the CI job `conformance`.

use std::num::NonZeroU32;

use futures_util::StreamExt;
use jiff::Timestamp;
use musubi_dav::{DavConfig, DavProvider, DavSetup, Principals};
use musubi_model::capability::DeltaSupport;
use musubi_model::provider::{Access, FetchHint, Pass, Start};
use musubi_model::remote::RemoteRef;
use musubi_model::{
    AccountCx, AccountId, ChangePage, CollectionRef, Credential, Cursor, EntityKind, Provider,
    RemoteKey, SyncError, TimeWindow,
};
use musubi_testkit::radicale::{PASSWORD, Radicale, USER};
use secrecy::SecretString;

fn event(uid: &str, day: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20261101T000000Z\r\nDTSTART:{day}T100000Z\r\nDTEND:{day}T110000Z\r\n\
         SUMMARY:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

async fn read(
    p: &DavProvider,
    cx: &AccountCx,
    key: &str,
    cursor: Option<Cursor>,
    start: Start,
) -> Result<Vec<ChangePage>, SyncError> {
    let mut hint = FetchHint::new(start);
    hint.page_size = NonZeroU32::new(2);
    p.changes(cx, &CollectionRef(key.to_owned()), cursor, hint)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
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
    let (l, rest) = pages.split_last().unwrap();
    assert!(l.last && rest.iter().all(|p| !p.last));
    l.cursor.clone().unwrap()
}

#[tokio::test]
async fn generic_profile_against_radicale() {
    let r = Radicale::start().await;
    let cal = r.calendar();
    let book = r.addressbook();
    r.put(&cal, "in.ics", &event("in", "20261113")).await;
    r.put(&cal, "old.ics", &event("old", "20250101")).await;
    let odd = r.put(&cal, "a@b c.ics", &event("ab", "20261114")).await;
    r.put(
        &book,
        "c.vcf",
        "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:c\r\nFN:C\r\nEND:VCARD\r\n",
    )
    .await;

    let p = DavProvider::new(DavConfig {
        allow_private: true,
    })
    .unwrap();
    let setup = DavSetup {
        server_url: Some(r.url()),
        ..DavSetup::default()
    };
    let mut cx = musubi_testkit::account_cx(AccountId::from_u128(1), Timestamp::now());
    cx.credential = Credential::Basic {
        username: USER.into(),
        password: SecretString::from(PASSWORD),
    };
    cx.endpoint = Some(setup.endpoint().unwrap());

    // Discovery through the well-known URL.
    let d = p.connect(&cx).await.unwrap();
    cx.endpoint = d.endpoint.clone();
    let c = d
        .collections
        .iter()
        .find(|c| c.remote.0 == cal)
        .expect("calendar");
    assert_eq!((c.kind, c.access), (EntityKind::Event, Access::Writer));
    assert_eq!(c.delta, Some(DeltaSupport::SyncToken));
    let b = d
        .collections
        .iter()
        .find(|c| c.remote.0 == book)
        .expect("address book");
    assert_eq!(b.kind, EntityKind::Contact);
    let subject = Principals::of(d.endpoint.as_ref().unwrap())
        .subject()
        .map(str::to_owned);
    assert_eq!(subject, Some(format!("{}{USER}/", r.url())));

    // Window, full, delta.
    let w = TimeWindow::new(
        "2026-11-01T00:00:00Z".parse().unwrap(),
        "2026-12-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let pages = read(&p, &cx, &cal, None, Start::Window(w)).await.unwrap();
    assert!(pages.iter().all(|p| p.pass == Pass::Window));
    let mut got: Vec<String> = pages
        .iter()
        .flat_map(|p| &p.upserts)
        .map(|o| o.key.0.clone())
        .collect();
    got.sort();
    assert_eq!(got, [odd.clone(), format!("{cal}in.ics")]);

    let pages = read(&p, &cx, &cal, None, Start::Full).await.unwrap();
    assert_eq!(pages.len(), 2);
    assert!(pages.iter().all(|p| p.pass == Pass::Full));
    assert_eq!(
        keys(&pages),
        [odd.clone(), format!("{cal}in.ics"), format!("{cal}old.ics")]
    );
    let cursor = last_cursor(&pages);

    r.put(&cal, "new.ics", &event("new", "20261115")).await;
    r.delete(&odd).await;
    let pages = read(&p, &cx, &cal, Some(cursor.clone()), Start::Full)
        .await
        .unwrap();
    assert_eq!(pages[0].pass, Pass::Delta);
    assert_eq!(keys(&pages), [format!("{cal}new.ics")]);
    assert_eq!(pages.last().unwrap().removals, [RemoteKey(odd.clone())]);
    let next = last_cursor(&pages);

    // A token Radicale never issued: 403 with valid-sync-token.
    let mut bogus = serde_json::from_slice::<serde_json::Value>(&next.0).unwrap();
    bogus["sync_token"]["token"] = format!("http://radicale.org/ns/sync/{}", "0".repeat(64)).into();
    let e = read(
        &p,
        &cx,
        &cal,
        Some(Cursor(serde_json::to_vec(&bogus).unwrap())),
        Start::Full,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, SyncError::CursorExpired), "{e:?}");

    // Bodies through multiget, the percent-encoded name included.
    let odd2 = r.put(&cal, "a@b c.ics", &event("ab2", "20261116")).await;
    let refs: Vec<RemoteRef> = [odd2.clone(), format!("{cal}in.ics")]
        .into_iter()
        .map(|k| RemoteRef {
            collection: CollectionRef(cal.clone()),
            key: RemoteKey(k),
        })
        .chain([RemoteRef {
            collection: CollectionRef(book.clone()),
            key: RemoteKey(format!("{book}c.vcf")),
        }])
        .collect();
    let objs = p.fetch(&cx, &refs).await.unwrap();
    assert_eq!(objs.len(), 3);
    let o = objs.iter().find(|o| o.key.0 == odd2).expect("a@b c.ics");
    // Radicale re-serialises what it stores; the body is its, unchanged by us.
    assert!(String::from_utf8_lossy(&o.raw.body).contains("UID:ab2\r\n"));
    assert!(!o.validator.0.starts_with("musubi:"));
    assert!(objs.iter().any(|o| o.kind == EntityKind::Contact));
    let pages = read(&p, &cx, &cal, Some(next), Start::Full).await.unwrap();
    assert_eq!(keys(&pages), [odd2]);
}
