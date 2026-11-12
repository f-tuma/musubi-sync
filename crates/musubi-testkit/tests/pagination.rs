//! Type test of the paging contract (docs/plan/phase1.md, K0): the `Provider`
//! interface can be implemented without `unsafe` or `todo!()`, and a consumer
//! written against it handles empty mid-stream pages, cursor resume, deltas,
//! expired cursors and window reads.

use std::collections::BTreeSet;
use std::num::NonZeroU32;

use http::Uri;
use jiff::{SignedDuration, Timestamp};
use musubi_model::provider::{ChannelId, FetchHint, Pass, PushHint, Start};
use musubi_model::remote::{MediaType, Raw};
use musubi_model::{
    AccountId, ChangePage, CollectionRef, Cursor, Provider, PushRejected, RemoteKey, RemoteObject,
    SyncError, TimeWindow, Validator,
};
use musubi_testkit::{FakeObject, FakeProvider, account_cx, block_on, collect};

const NOW: Timestamp = Timestamp::constant(1_794_000_000, 0);

fn obj(key: &str, span: Option<(i64, i64)>) -> FakeObject {
    FakeObject {
        object: RemoteObject {
            key: RemoteKey(key.into()),
            validator: Validator(format!("\"{key}-v1\"")),
            raw: Raw {
                media_type: MediaType::Json,
                body: format!("{{\"id\":\"{key}\"}}").into_bytes(),
            },
            master: None,
        },
        span: span.map(|(a, b)| {
            (
                NOW + SignedDuration::from_hours(a),
                NOW + SignedDuration::from_hours(b),
            )
        }),
    }
}

fn hint(start: Start) -> FetchHint {
    FetchHint {
        start,
        page_size: None,
    }
}

/// Reads a stream the way the engine does: stop after `last` or the first error.
fn read(
    p: &dyn Provider,
    col: &CollectionRef,
    cursor: Option<Cursor>,
    start: Start,
) -> Result<Vec<ChangePage>, SyncError> {
    let cx = account_cx(AccountId::from_u128(1), NOW);
    let mut pages = Vec::new();
    for item in collect(p.changes(&cx, col, cursor, hint(start))) {
        let page = item?;
        let last = page.last;
        pages.push(page);
        if last {
            break;
        }
    }
    Ok(pages)
}

fn keys(pages: &[ChangePage]) -> Vec<String> {
    pages
        .iter()
        .flat_map(|p| &p.upserts)
        .map(|o| o.key.0.clone())
        .collect()
}

fn setup() -> (FakeProvider, CollectionRef) {
    let fake = FakeProvider::new()
        .with_page_size(NonZeroU32::new(2).unwrap())
        .with_empty_page_after_first();
    let col = CollectionRef("primary".into());
    for k in ["a", "b", "c", "d", "e"] {
        fake.put(&col, obj(k, None));
    }
    (fake, col)
}

#[test]
fn full_read_pages_with_empty_mid_stream_page() {
    let (fake, col) = setup();
    let pages = read(&fake, &col, None, Start::Full).unwrap();

    let sizes: Vec<_> = pages.iter().map(|p| (p.upserts.len(), p.last)).collect();
    assert_eq!(sizes, [(2, false), (0, false), (2, false), (1, true)]);
    assert!(
        pages
            .iter()
            .all(|p| p.pass == Pass::Full && p.cursor.is_some())
    );
    assert_eq!(keys(&pages), ["a", "b", "c", "d", "e"]);
}

#[test]
fn full_read_resumes_from_any_page_cursor() {
    let (fake, col) = setup();
    let all = read(&fake, &col, None, Start::Full).unwrap();
    for (i, page) in all.iter().enumerate().filter(|(_, p)| !p.last) {
        // Crash after writing page `i`: resume from its stored cursor.
        let rest = read(&fake, &col, page.cursor.clone(), Start::Full).unwrap();
        let mut seen = keys(&all[..=i]);
        seen.extend(keys(&rest));
        assert_eq!(seen, ["a", "b", "c", "d", "e"], "resume after page {i}");
        assert!(rest.last().is_some_and(|p| p.last));
    }
}

#[test]
fn full_read_misses_no_key_when_collection_changes_mid_read() {
    let (fake, col) = setup();
    let first = read(&fake, &col, None, Start::Full).unwrap();
    // Page 1 delivered a, b. Then both are deleted and aa inserted behind the
    // cursor; an offset into the live map would now skip c.
    fake.delete(&col, &RemoteKey("a".into()));
    fake.delete(&col, &RemoteKey("b".into()));
    fake.put(&col, obj("aa", None));
    let rest = read(&fake, &col, first[0].cursor.clone(), Start::Full).unwrap();
    assert_eq!(keys(&rest), ["c", "d", "e"]);

    // The delta from the end of the full read delivers both changes.
    let delta = read(
        &fake,
        &col,
        rest.last().and_then(|p| p.cursor.clone()),
        Start::Full,
    )
    .unwrap();
    assert_eq!(keys(&delta), ["aa"]);
    let removed: Vec<_> = delta.iter().flat_map(|p| &p.removals).collect();
    assert_eq!(removed, [&RemoteKey("a".into()), &RemoteKey("b".into())]);
}

#[test]
fn delta_pages_skip_no_key_when_a_delivered_key_changes_again() {
    let (fake, col) = setup();
    let full = read(&fake, &col, None, Start::Full).unwrap();
    let start = full.last().and_then(|p| p.cursor.clone());
    for k in ["a", "b", "c"] {
        fake.put(&col, obj(k, None));
    }
    let first = read(&fake, &col, start, Start::Full).unwrap();
    assert_eq!(keys(&first[..1]), ["a", "b"]);
    // a changes again after page 1: c must still come on page 2.
    fake.put(&col, obj("a", None));
    let rest = read(&fake, &col, first[0].cursor.clone(), Start::Full).unwrap();
    assert_eq!(keys(&rest), ["c"]);
    let next = read(
        &fake,
        &col,
        rest.last().and_then(|p| p.cursor.clone()),
        Start::Full,
    )
    .unwrap();
    assert_eq!(keys(&next), ["a"]);
}

#[test]
fn delta_after_full_read_reports_upserts_and_removals() {
    let (fake, col) = setup();
    let full = read(&fake, &col, None, Start::Full).unwrap();
    let delta_cursor = full.last().and_then(|p| p.cursor.clone());

    // Nothing changed: one empty, last page with a cursor.
    let none = read(&fake, &col, delta_cursor.clone(), Start::Full).unwrap();
    assert_eq!(none.len(), 1);
    assert!(none[0].last && none[0].upserts.is_empty() && none[0].cursor.is_some());

    fake.put(&col, obj("f", None));
    fake.delete(&col, &RemoteKey("b".into()));
    fake.put(&col, obj("c", None));
    let delta = read(&fake, &col, delta_cursor, Start::Full).unwrap();
    assert!(
        delta
            .iter()
            .all(|p| p.pass == Pass::Delta && p.cursor.is_some())
    );
    assert_eq!(
        keys(&delta).into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from(["c".into(), "f".into()])
    );
    let removed: Vec<_> = delta.iter().flat_map(|p| &p.removals).collect();
    assert_eq!(removed, [&RemoteKey("b".into())]);

    // The new delta cursor sees nothing more.
    let next = read(
        &fake,
        &col,
        delta.last().and_then(|p| p.cursor.clone()),
        Start::Full,
    )
    .unwrap();
    assert!(
        next.iter()
            .all(|p| p.upserts.is_empty() && p.removals.is_empty())
    );
}

#[test]
fn expired_cursor_is_reported_as_cursor_expired() {
    let (fake, col) = setup();
    let full = read(&fake, &col, None, Start::Full).unwrap();
    fake.put(&col, obj("f", None));
    fake.expire_cursors();
    let err = read(
        &fake,
        &col,
        full.last().and_then(|p| p.cursor.clone()),
        Start::Full,
    )
    .unwrap_err();
    assert!(matches!(err, SyncError::CursorExpired));
}

#[test]
fn window_read_has_no_cursor() {
    let fake = FakeProvider::new().with_page_size(NonZeroU32::new(1).unwrap());
    let col = CollectionRef("primary".into());
    fake.put(&col, obj("in", Some((1, 2))));
    fake.put(&col, obj("edge", Some((-1, 0))));
    fake.put(&col, obj("out", Some((30, 31))));
    fake.put(&col, obj("untimed", None));
    let window = TimeWindow::new(NOW, NOW + SignedDuration::from_hours(24)).unwrap();

    let pages = read(&fake, &col, None, Start::Window(window)).unwrap();
    assert_eq!(keys(&pages), ["in"]);
    assert!(
        pages
            .iter()
            .all(|p| p.pass == Pass::Window && p.cursor.is_none())
    );
    assert!(pages.last().is_some_and(|p| p.last));
}

#[test]
fn push_notifications_are_verified_against_the_subscription() {
    let (fake, col) = setup();
    let cx = account_cx(AccountId::from_u128(1), NOW);
    let push = fake.push().unwrap();
    let hook = Uri::from_static("https://example.invalid/hook");
    let sub = block_on(push.subscribe(&cx, &col, &hook, &ChannelId("ch".into()))).unwrap();
    assert!(sub.expiration > NOW);

    let hello = FakeProvider::notification(&sub, true);
    assert_eq!(push.channel_of(&hello), Some(sub.channel.clone()));
    assert_eq!(push.verify(&hello, &sub), Ok(PushHint::Handshake));
    assert_eq!(
        push.verify(&FakeProvider::notification(&sub, false), &sub),
        Ok(PushHint::Changed)
    );

    let mut forged = sub.clone();
    forged.token = "wrong".into();
    assert_eq!(
        push.verify(&FakeProvider::notification(&forged, false), &sub),
        Err(PushRejected::BadToken)
    );
    assert!(matches!(
        push.verify(&http::HeaderMap::new(), &sub),
        Err(PushRejected::Malformed(_))
    ));
}
