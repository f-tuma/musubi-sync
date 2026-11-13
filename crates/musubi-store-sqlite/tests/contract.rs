//! Contract tests of `TenantStore` (invariants 1–6 of `musubi_model::store`).
//! The bodies use only the trait; the SQLite store is the implementation under test.

mod common;

use common::*;
use jiff::tz::TimeZone;
use musubi_model::journal::{ChangeOp, ChangeTarget};
use musubi_model::store::{
    CollectionError, CollectionStatus, Coverage, DiscoveryWrite, Include, OccurrenceWrite,
    RangeQuery, SeriesExtension,
};
use musubi_model::store::{OccurrenceRow, Occurrences};
use musubi_model::{Cursor, ReadId, RemoteKey, Seq, Storage, StoreError, TenantStore, TimeWindow};

fn setup() -> (
    TempDir,
    musubi_store_sqlite::SqliteStorage,
    musubi_store_sqlite::SqliteTenant,
) {
    let dir = TempDir::new();
    let s = storage(dir.path());
    let t = s.open(TENANT).unwrap();
    (dir, s, t)
}

fn live_keys(t: &impl TenantStore, col: musubi_model::CollectionId, ks: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = t
        .items_by_remote(col, &keys(ks))
        .unwrap()
        .into_iter()
        .filter(|s| !s.tombstone)
        .map(|s| s.key.0)
        .collect();
    v.sort();
    v
}

fn window(a: i64, b: i64) -> RangeQuery {
    RangeQuery {
        window: TimeWindow::new(t(a), t(b)).unwrap(),
        collections: None,
        zone: TimeZone::UTC,
        after: None,
        limit: 1000,
        ids_only: false,
    }
}

/// Invariant 1: a failure in the middle of a page leaves nothing of it, cursor included.
#[test]
fn page_is_written_whole_or_not_at_all() {
    let (dir, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let mut p = page(col, vec![event("a", "1", 0)]);
    p.cursor = Some(Cursor(b"c1".to_vec()));
    t.apply_page(&p).unwrap();
    let head = t.head().unwrap();

    // Fault injection at the SQL boundary: the second item's occurrence insert aborts.
    let raw =
        rusqlite::Connection::open(dir.path().join(format!("tenants/{TENANT}/main.db"))).unwrap();
    raw.execute_batch(&format!(
        "CREATE TRIGGER boom BEFORE INSERT ON occurrences WHEN NEW.start = {}
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
        t_us(99)
    ))
    .unwrap();
    let mut p = page(col, vec![event("b", "1", 1), event("c", "1", 99)]);
    p.removals = keys(&["a"]);
    p.cursor = Some(Cursor(b"c2".to_vec()));
    assert!(t.apply_page(&p).is_err());

    assert_eq!(t.head().unwrap(), head);
    assert_eq!(live_keys(&t, col, &["a", "b", "c"]), ["a"]);
    let c = t.collections().unwrap().pop().unwrap();
    assert_eq!(c.cursor, Some(Cursor(b"c1".to_vec())));
}

fn t_us(h: i64) -> i64 {
    t(h).as_microsecond()
}

/// Invariant 2: a repeated page changes nothing and does not advance `Seq`;
/// with a read it only marks the items seen.
#[test]
fn repeated_page_is_a_no_op() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let p = page(col, vec![event("a", "1", 0), event("b", "1", 2)]);
    let head = t.apply_page(&p).unwrap();
    let a = t
        .items_by_remote(col, &keys(&["a"]))
        .unwrap()
        .pop()
        .unwrap();
    let before = t.entity(a.entity, Include::default()).unwrap().unwrap();

    assert_eq!(t.apply_page(&p).unwrap(), head);
    let mut full = p.clone();
    full.read = Some(ReadId(1));
    assert_eq!(t.apply_page(&full).unwrap(), head);

    let after = t.entity(a.entity, Include::default()).unwrap().unwrap();
    assert_eq!((after.rev, after.seq), (before.rev, before.seq));
    assert_eq!(t.changes(head, 10).unwrap().changes, []);
    assert_eq!(t.unseen_count(col, ReadId(1)).unwrap(), 0);
}

/// Invariant 3: `Seq` strictly increases and is not reused after a prune.
#[test]
fn seq_increases_and_survives_prune() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let h1 = t.apply_page(&page(col, vec![event("a", "1", 0)])).unwrap();
    let h2 = t.apply_page(&page(col, vec![event("a", "2", 0)])).unwrap();
    assert!(h2 > h1);
    let all = t.changes(Seq(0), 100).unwrap();
    assert!(all.changes.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(all.next, h2);

    let info = t
        .prune(jiff::Timestamp::now() + jiff::SignedDuration::from_secs(1))
        .unwrap();
    assert_eq!(info.oldest, h2);
    assert_eq!(t.head().unwrap(), h2);
    assert!(matches!(
        t.changes(Seq(0), 10),
        Err(StoreError::CursorTooOld { oldest }) if oldest == h2
    ));
    let h3 = t.apply_page(&page(col, vec![event("a", "3", 0)])).unwrap();
    assert!(h3 > h2);
    let after = t.changes(h2, 10).unwrap();
    assert!(after.changes.iter().all(|c| c.seq > h2));
}

/// Invariant 4: full-read bookkeeping.
#[test]
fn full_read_marks_and_finishes() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(
        col,
        ["a", "b", "c", "d"].map(|k| event(k, "1", 0)).to_vec(),
    ))
    .unwrap();

    // Read 1, first page sees a.
    let mut p = page(col, vec![]);
    p.read = Some(ReadId(1));
    p.unchanged = keys(&["a"]);
    t.apply_page(&p).unwrap();
    assert_eq!(t.unseen_count(col, ReadId(1)).unwrap(), 3);
    // Not finished before the last page.
    assert!(matches!(
        t.finish_full_read(col, ReadId(1)),
        Err(StoreError::StaleRead)
    ));

    // A delta page while the read is active counts as seen (b changed, c removed).
    let mut delta = page(col, vec![event("b", "2", 0)]);
    delta.removals = keys(&["c"]);
    t.apply_page(&delta).unwrap();
    assert_eq!(t.unseen(col, ReadId(1), None, 10).unwrap(), keys(&["d"]));

    // Last page of read 1.
    let mut last = page(col, vec![]);
    last.read = Some(ReadId(1));
    last.last = true;
    t.apply_page(&last).unwrap();
    let state = t.collections().unwrap().pop().unwrap();
    assert_eq!(
        (state.active_read, state.read_complete),
        (Some(ReadId(1)), true)
    );
    // A later page without `last` keeps it complete.
    t.apply_page(&page(col, vec![])).unwrap();
    assert!(t.collections().unwrap().pop().unwrap().read_complete);

    let before = t.head().unwrap();
    let head = t.finish_full_read(col, ReadId(1)).unwrap();
    assert!(head > before);
    assert_eq!(live_keys(&t, col, &["a", "b", "c", "d"]), ["a", "b"]);
    let state = t.collections().unwrap().pop().unwrap();
    assert_eq!(
        (state.active_read, state.read_complete, state.last_read),
        (None, false, Some(ReadId(1)))
    );

    // A read not above the last one is stale, and nothing is written.
    let mut stale = page(col, vec![event("e", "1", 0)]);
    stale.read = Some(ReadId(1));
    assert!(matches!(t.apply_page(&stale), Err(StoreError::StaleRead)));
    assert_eq!(t.head().unwrap(), head);
}

/// Invariant 4: a newer read replaces the active one and drops its marks;
/// an older one is stale.
#[test]
fn newer_read_replaces_active_read() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(col, vec![event("a", "1", 0), event("b", "1", 0)]))
        .unwrap();
    let mut p = page(col, vec![]);
    p.read = Some(ReadId(5));
    p.unchanged = keys(&["a", "b"]);
    t.apply_page(&p).unwrap();

    let mut p = page(col, vec![]);
    p.read = Some(ReadId(6));
    p.unchanged = keys(&["a"]);
    p.last = true;
    t.apply_page(&p).unwrap();
    assert_eq!(t.unseen(col, ReadId(6), None, 10).unwrap(), keys(&["b"]));

    let mut old = page(col, vec![]);
    old.read = Some(ReadId(5));
    assert!(matches!(t.apply_page(&old), Err(StoreError::StaleRead)));
    assert!(matches!(
        t.finish_full_read(col, ReadId(5)),
        Err(StoreError::StaleRead)
    ));
    t.finish_full_read(col, ReadId(6)).unwrap();
    assert_eq!(live_keys(&t, col, &["a", "b"]), ["a"]);
}

/// Invariant 4: a newer read replacing an already complete one starts incomplete,
/// so the older read's completion cannot finish the newer one early.
#[test]
fn newer_read_replaces_complete_read() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(col, vec![event("a", "1", 0), event("b", "1", 0)]))
        .unwrap();
    let mut p = page(col, vec![]);
    p.read = Some(ReadId(5));
    p.unchanged = keys(&["a", "b"]);
    p.last = true;
    t.apply_page(&p).unwrap();

    let mut p = page(col, vec![]);
    p.read = Some(ReadId(6));
    p.unchanged = keys(&["a"]);
    t.apply_page(&p).unwrap();
    let state = t.collections().unwrap().pop().unwrap();
    assert_eq!(
        (state.active_read, state.read_complete),
        (Some(ReadId(6)), false)
    );
    assert!(matches!(
        t.finish_full_read(col, ReadId(6)),
        Err(StoreError::StaleRead)
    ));
    assert_eq!(live_keys(&t, col, &["a", "b"]), ["a", "b"]);
}

/// Invariant 5: one item per key, one entity per item, one placement per entity;
/// a removed key that comes back revives the same entity.
#[test]
fn one_item_entity_and_placement_per_key() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(col, vec![event("a", "1", 0)])).unwrap();
    let first = t.items_by_remote(col, &keys(&["a"])).unwrap();
    t.apply_page(&page(col, vec![event("a", "2", 0)])).unwrap();
    let second = t.items_by_remote(col, &keys(&["a"])).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(
        (first[0].item, first[0].entity),
        (second[0].item, second[0].entity)
    );

    let e = t
        .entity(
            second[0].entity,
            Include {
                placements: true,
                raw: false,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(e.rev, 2);
    assert_eq!(e.placements.len(), 1);
    assert_eq!(e.placements[0].item, second[0].item);
    assert!(e.canonical.unwrap().get().contains(r#""v":"2""#));
    let item = t
        .item(
            second[0].item,
            Include {
                placements: false,
                raw: true,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(item.raw.unwrap().body, br#"{"id":"a","v":"2"}"#);
    assert_eq!(item.validator.0, "a@2");

    let mut rm = page(col, vec![]);
    rm.removals = keys(&["a", "unknown"]);
    t.apply_page(&rm).unwrap();
    let e = t
        .entity(
            second[0].entity,
            Include {
                placements: true,
                raw: false,
            },
        )
        .unwrap()
        .unwrap();
    assert!(e.tombstone && e.placements.is_empty());
    assert!(t.range(&window(-1, 2)).unwrap().occurrences.is_empty());

    let head = t.apply_page(&page(col, vec![event("a", "3", 0)])).unwrap();
    let back = t.items_by_remote(col, &keys(&["a"])).unwrap();
    assert_eq!(back[0].entity, second[0].entity);
    assert!(!back[0].tombstone);
    let created = t.changes(Seq(head.0 - 2), 10).unwrap().changes;
    assert!(
        created.iter().all(|c| c.op == ChangeOp::Created),
        "{created:?}"
    );
    assert_eq!(t.stats().unwrap().items, 1);
}

/// Invariant 6: sorted by (start, entity, rid), paged without duplicates or gaps.
#[test]
fn range_is_sorted_and_paged() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let mut items: Vec<_> = (0..20)
        .map(|i| event(&format!("e{i}"), "1", i % 4))
        .collect();
    items.push(series("s", 1, 5, None));
    t.apply_page(&page(col, items)).unwrap();

    let all = t.range(&window(0, 200)).unwrap();
    assert!(all.next.is_none());
    let key = |o: &musubi_model::store::OccurrenceView| (o.start, o.entity, o.recurrence_id);
    assert_eq!(all.occurrences.len(), 25);
    assert!(all.occurrences.windows(2).all(|w| key(&w[0]) < key(&w[1])));

    let mut q = window(0, 200);
    q.limit = 3;
    q.ids_only = true;
    let mut paged = Vec::new();
    loop {
        let p = t.range(&q).unwrap();
        assert!(p.occurrences.iter().all(|o| o.canonical.is_none()));
        paged.extend(p.occurrences.iter().map(key));
        match p.next {
            Some(n) => q.after = Some(n),
            None => break,
        }
    }
    assert_eq!(paged, all.occurrences.iter().map(key).collect::<Vec<_>>());
}

/// Overlap semantics, long events found from before the window, collection filter.
#[test]
fn range_overlap_and_filter() {
    let (_d, _s, mut t) = setup();
    let a = collection(&mut t, "a");
    let b = collection(&mut t, "b");
    let long = item(
        "long",
        "1",
        Occurrences {
            rows: vec![OccurrenceRow {
                start: common::t(0),
                end: common::t(100),
                recurrence_id: None,
                floating: false,
            }],
            series: None,
        },
    );
    t.apply_page(&page(a, vec![long, event("x", "1", 10)]))
        .unwrap();
    t.apply_page(&page(b, vec![event("y", "1", 10)])).unwrap();

    assert_eq!(t.range(&window(50, 60)).unwrap().occurrences.len(), 1);
    assert_eq!(t.range(&window(11, 12)).unwrap().occurrences.len(), 1); // end is exclusive
    assert_eq!(t.range(&window(10, 11)).unwrap().occurrences.len(), 3);
    let mut q = window(10, 11);
    q.collections = Some(vec![b]);
    assert_eq!(t.range(&q).unwrap().occurrences.len(), 1);
    q.collections = Some(vec![]);
    assert!(t.range(&q).unwrap().occurrences.is_empty());
}

/// Floating occurrences are placed in the query zone before the exact filter.
#[test]
fn floating_occurrences_follow_the_zone() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let mut w = event("f", "1", 9); // 09:00–10:00 wall time
    w.occurrences.rows[0].floating = true;
    t.apply_page(&page(col, vec![w])).unwrap();

    let mut q = window(9, 10); // 09:00–10:00 UTC
    assert_eq!(t.range(&q).unwrap().occurrences.len(), 1);
    q.zone = TimeZone::fixed(jiff::tz::offset(-5)); // 09:00 wall = 14:00 UTC
    assert!(t.range(&q).unwrap().occurrences.is_empty());
    q.window = TimeWindow::new(common::t(14), common::t(15)).unwrap();
    let o = t.range(&q).unwrap().occurrences;
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].start, common::t(9), "index convention is returned");
}

/// A separate exception item of a series suppresses the master's occurrence it replaces.
#[test]
fn exception_item_suppresses_master_occurrence() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let mut ex = event("s_ex", "1", 24 + 1 + 3); // day 2 moved by three hours
    ex.correlation = Some(musubi_model::store::Correlation {
        uid: "s".into(),
        recurrence_id: Some(common::t(25)),
    });
    t.apply_page(&page(col, vec![series("s", 1, 3, Some(3)), ex]))
        .unwrap();
    let occ = t.range(&window(0, 100)).unwrap().occurrences;
    let starts: Vec<_> = occ.iter().map(|o| o.start).collect();
    assert_eq!(starts, [common::t(1), common::t(28), common::t(49)]);
}

/// Series horizon: `unindexed`, `series_to_extend` and `extend_occurrences`.
#[test]
fn series_index_extension() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(
        col,
        vec![series("s", 0, 2, None), series("done", 0, 2, Some(2))],
    ))
    .unwrap();
    let s = t.items_by_remote(col, &keys(&["s"])).unwrap()[0].entity;

    let page1 = t.range(&window(0, 24 * 10)).unwrap();
    assert_eq!(page1.unindexed.len(), 1);
    assert_eq!(page1.unindexed[0].entity, s);
    let due = t.series_to_extend(common::t(24 * 10), 10).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].indexed_until, common::t(48));

    let ext = |from: i64| SeriesExtension {
        entity: s,
        from: common::t(from),
        rows: vec![row(48, Some(48)), row(72, Some(72))],
        indexed_until: common::t(96),
    };
    // A stale horizon is skipped.
    t.extend_occurrences(&OccurrenceWrite {
        extensions: vec![ext(24)],
    })
    .unwrap();
    assert_eq!(t.range(&window(0, 240)).unwrap().occurrences.len(), 4);
    t.extend_occurrences(&OccurrenceWrite {
        extensions: vec![ext(48)],
    })
    .unwrap();
    assert_eq!(t.range(&window(0, 240)).unwrap().occurrences.len(), 6);
    assert_eq!(
        t.series_to_extend(common::t(240), 10).unwrap()[0].indexed_until,
        common::t(96)
    );
    assert!(t.series_to_extend(common::t(96), 10).unwrap().is_empty());
}

#[test]
fn discovery_tracks_collections() {
    let (_d, _s, mut t) = setup();
    let d = |names: &[&str], complete| DiscoveryWrite {
        account: ACCOUNT,
        collections: names.iter().map(|n| remote(n)).collect(),
        complete,
    };
    t.apply_discovery(&d(&["a", "b"], true)).unwrap();
    let h = t.head().unwrap();
    assert_eq!(t.changes(Seq(0), 10).unwrap().changes.len(), 2);
    t.apply_discovery(&d(&["a", "b"], true)).unwrap();
    assert_eq!(t.head().unwrap(), h, "unchanged list writes nothing");

    t.apply_discovery(&d(&["a"], false)).unwrap();
    assert!(
        t.collections().unwrap().iter().all(|c| !c.gone),
        "incomplete list marks nothing gone"
    );
    t.apply_discovery(&d(&["a"], true)).unwrap();
    let gone: Vec<_> = t
        .collections()
        .unwrap()
        .into_iter()
        .filter(|c| c.gone)
        .map(|c| c.remote.name)
        .collect();
    assert_eq!(gone, ["b"]);

    let mut renamed = remote("b");
    renamed.name = "B".into();
    t.apply_discovery(&DiscoveryWrite {
        account: ACCOUNT,
        collections: vec![renamed],
        complete: false,
    })
    .unwrap();
    let b = t
        .collections()
        .unwrap()
        .into_iter()
        .find(|c| c.remote.remote.0 == "b")
        .unwrap();
    assert!(!b.gone);
    assert_eq!(b.remote.name, "B");
    let last = t.changes(Seq(0), 10).unwrap().changes.pop().unwrap();
    assert_eq!(
        (last.target, last.op),
        (ChangeTarget::Collection(b.id), ChangeOp::Updated)
    );
}

#[test]
fn purge_collection_tombstones_everything() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(col, vec![event("a", "1", 0), event("b", "1", 1)]))
        .unwrap();
    let before = t.head().unwrap();
    t.purge_collection(col).unwrap();
    assert!(t.collections().unwrap().is_empty());
    assert!(t.range(&window(0, 10)).unwrap().occurrences.is_empty());
    let ch = t.changes(before, 10).unwrap().changes;
    assert_eq!(ch.len(), 5); // 2 entities, 2 placements, 1 collection
    assert!(ch.iter().all(|c| c.op == ChangeOp::Deleted));
    assert!(matches!(
        t.apply_page(&page(col, vec![])),
        Err(StoreError::NotFound)
    ));
    assert!(matches!(t.purge_collection(col), Err(StoreError::NotFound)));
}

#[test]
fn collection_status_round_trips() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let s = CollectionStatus {
        coverage: Coverage::Window(TimeWindow::new(common::t(0), common::t(10)).unwrap()),
        error: Some(CollectionError {
            message: "rate limited".into(),
            at: common::t(1),
            permanent: false,
        }),
        synced_at: Some(common::t(2)),
        full_read_at: None,
    };
    let head = t.head().unwrap();
    t.set_collection_state(col, &s).unwrap();
    assert_eq!(t.collections().unwrap()[0].status, s);
    assert_eq!(t.head().unwrap(), head);
    let full = CollectionStatus {
        coverage: Coverage::Full,
        ..Default::default()
    };
    t.set_collection_state(col, &full).unwrap();
    assert_eq!(t.collections().unwrap()[0].status, full);
    let other = musubi_model::CollectionId::from_u128(1);
    assert!(matches!(
        t.set_collection_state(other, &full),
        Err(StoreError::NotFound)
    ));
}

#[test]
fn changes_are_paged() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(
        col,
        (0..5).map(|i| event(&format!("k{i}"), "1", 0)).collect(),
    ))
    .unwrap();
    let mut since = Seq(0);
    let mut n = 0;
    loop {
        let p = t.changes(since, 4).unwrap();
        n += p.changes.len();
        since = p.next;
        if !p.more {
            break;
        }
    }
    assert_eq!((n, since), (11, t.head().unwrap())); // 1 collection + 5 entities + 5 placements
    let empty = t.changes(since, 4).unwrap();
    assert_eq!(
        (empty.next, empty.more, empty.changes.len()),
        (since, false, 0)
    );
}

#[test]
fn prune_removes_old_tombstones_and_orphaned_bodies() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(col, vec![event("a", "1", 0), event("b", "1", 0)]))
        .unwrap();
    t.apply_page(&page(col, vec![event("a", "2", 0)])).unwrap(); // body a@1 orphaned
    let mut rm = page(col, vec![]);
    rm.removals = keys(&["b"]);
    t.apply_page(&rm).unwrap();
    let b = t.items_by_remote(col, &keys(&["b"])).unwrap()[0].clone();

    let past = t.prune(common::t(-1000)).unwrap();
    assert_eq!((past.changes, past.tombstones), (0, 0));
    assert_eq!(past.raw, 1);
    let info = t
        .prune(jiff::Timestamp::now() + jiff::SignedDuration::from_secs(1))
        .unwrap();
    assert_eq!(info.tombstones, 3); // b's placement, item and entity
    assert_eq!(info.raw, 1); // b's body
    assert!(t.items_by_remote(col, &keys(&["b"])).unwrap().is_empty());
    assert!(t.entity(b.entity, Include::default()).unwrap().is_none());
    assert_eq!(live_keys(&t, col, &["a"]), ["a"]);
    let a = t.items_by_remote(col, &keys(&["a"])).unwrap()[0].item;
    assert!(
        t.item(
            a,
            Include {
                placements: false,
                raw: true
            }
        )
        .unwrap()
        .unwrap()
        .raw
        .is_some()
    );
}

#[test]
fn unseen_pages_by_key() {
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    t.apply_page(&page(
        col,
        (0..7).map(|i| event(&format!("k{i}"), "1", 0)).collect(),
    ))
    .unwrap();
    let mut p = page(col, vec![]);
    p.read = Some(ReadId(1));
    p.unchanged = keys(&["k3"]);
    t.apply_page(&p).unwrap();
    let mut after: Option<RemoteKey> = None;
    let mut all = Vec::new();
    loop {
        let ks = t.unseen(col, ReadId(1), after.as_ref(), 2).unwrap();
        if ks.is_empty() {
            break;
        }
        after = ks.last().cloned();
        all.extend(ks);
    }
    assert_eq!(all, keys(&["k0", "k1", "k2", "k4", "k5", "k6"]));
}

/// An item of `kind` without occurrences (tasks, contacts).
fn undated(key: &str, kind: musubi_model::EntityKind) -> musubi_model::store::ItemWrite {
    let mut w = item(key, "1", Occurrences::default());
    w.kind = kind;
    w
}

#[test]
fn list_entities_pages_one_kind_without_tombstones() {
    use musubi_model::EntityKind::{Contact, Event, Task};
    let (_d, _s, mut t) = setup();
    let (a, b, other) = (
        collection(&mut t, "a"),
        collection(&mut t, "b"),
        collection(&mut t, "other"),
    );
    let tasks = ["t1", "t2", "t3", "t4", "t5"];
    t.apply_page(&page(
        a,
        vec![
            undated("t1", Task),
            undated("t2", Task),
            undated("t3", Task),
            undated("c1", Contact),
            event("e1", "1", 0),
        ],
    ))
    .unwrap();
    t.apply_page(&page(b, vec![undated("t4", Task), undated("t5", Task)]))
        .unwrap();
    t.apply_page(&page(other, vec![undated("t9", Task)]))
        .unwrap();
    let mut gone = page(b, vec![]);
    gone.removals = keys(&["t5"]);
    t.apply_page(&gone).unwrap();

    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let p = t
            .list_entities(Task, &[a, b], after, 2, Include::default())
            .unwrap();
        assert!(p.len() <= 2);
        assert!(p.iter().all(|e| e.kind == Task && !e.tombstone));
        assert!(p.iter().all(|e| e.placements.is_empty()));
        let Some(last) = p.last() else { break };
        after = Some(last.id);
        seen.extend(p);
    }
    let ids: Vec<_> = seen.iter().map(|e| e.id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "in EntityId order");
    let mut docs: Vec<String> = seen
        .iter()
        .map(|e| e.canonical.as_ref().unwrap().get().to_owned())
        .collect();
    docs.sort();
    let want: Vec<String> = tasks[..4]
        .iter()
        .map(|k| format!(r#"{{"id":"{k}","v":"1"}}"#))
        .collect();
    assert_eq!(docs, want, "t5 tombstoned, t9 in another collection");

    let inc = Include {
        placements: true,
        raw: true,
    };
    let c = t.list_entities(Contact, &[a, b], None, 10, inc).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].placements.len(), 1);
    assert_eq!(c[0].placements[0].collection, a);
    assert_eq!(
        t.list_entities(Event, &[b], None, 10, inc).unwrap().len(),
        0
    );
    assert!(
        t.list_entities(Task, &[], None, 10, inc)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn stale_items_and_redecode() {
    use musubi_model::EntityKind::{Event, Task};
    let (_d, _s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let mut current = event("cur", "1", 3);
    current.mapping_version = 2;
    t.apply_page(&page(
        col,
        vec![
            event("a", "1", 0),
            event("b", "1", 1),
            event("dead", "1", 2),
            current,
            undated("task", Task),
        ],
    ))
    .unwrap();
    let mut gone = page(col, vec![]);
    gone.removals = keys(&["dead"]);
    t.apply_page(&gone).unwrap();

    // Stale under mapping 2: `a` and `b`, not the tombstone, the current one or the task.
    let p1 = t.stale_items(Event, &[col], 1, 2, None, 1).unwrap();
    let p2 = t
        .stale_items(Event, &[col], 1, 2, Some(p1[0].id), 1)
        .unwrap();
    assert!(p1[0].id < p2[0].id, "in ItemId order");
    assert!(
        t.stale_items(Event, &[col], 1, 2, Some(p2[0].id), 1)
            .unwrap()
            .is_empty()
    );
    let mut stale: Vec<_> = p1.into_iter().chain(p2).collect();
    stale.sort_by(|x, y| x.key.0.cmp(&y.key.0));
    assert_eq!(stale[0].key.0, "a");
    assert_eq!(stale[1].key.0, "b");
    let raw = stale[0].raw.as_ref().expect("with raw content");
    assert_eq!(raw.body, br#"{"id":"a","v":"1"}"#);
    assert_eq!(
        t.stale_items(Event, &[col], 2, 2, None, 10).unwrap().len(),
        3
    );
    assert!(
        t.stale_items(Event, &[], 2, 2, None, 10)
            .unwrap()
            .is_empty()
    );

    let entity_of = |t: &musubi_store_sqlite::SqliteTenant, id| {
        t.entity(
            t.item(id, Include::default()).unwrap().unwrap().entity,
            Include::default(),
        )
        .unwrap()
        .unwrap()
    };
    let rev_a = entity_of(&t, stale[0].id).rev;
    let rev_b = entity_of(&t, stale[1].id).rev;
    let head = t.head().unwrap();

    // `a` decodes to the same canonical form under mapping 2; `b` to a new one
    // that moves its occurrence. A changed body and an unknown key are skipped.
    let mut same = event("a", "1", 0);
    same.mapping_version = 2;
    same.validator = musubi_model::Validator("ignored".into());
    let mut changed = event("b", "1", 1);
    changed.mapping_version = 2;
    changed.canonical =
        Some(serde_json::value::RawValue::from_string(r#"{"id":"b","v":"new"}"#.into()).unwrap());
    changed.occurrences.rows = vec![row(7, None)];
    let mut moved = event("cur", "2", 3);
    moved.mapping_version = 3;
    let mut unknown = event("nope", "1", 0);
    unknown.mapping_version = 2;
    let after = t.redecode(col, &[same, changed, moved, unknown]).unwrap();

    let ch = journal(&t, head);
    assert_eq!(ch.len(), 1, "only the changed canonical form is journaled");
    let b_entity = entity_of(&t, stale[1].id);
    assert_eq!(
        (ch[0].target, ch[0].op),
        (ChangeTarget::Entity(b_entity.id), ChangeOp::Updated)
    );
    assert_eq!(after, ch[0].seq);
    assert_eq!(b_entity.rev, rev_b + 1);
    assert_eq!(b_entity.canonical.unwrap().get(), r#"{"id":"b","v":"new"}"#);
    assert_eq!(entity_of(&t, stale[0].id).rev, rev_a);

    let a = t.item(stale[0].id, Include::default()).unwrap().unwrap();
    assert_eq!(a.mapping_version, 2);
    assert_eq!(a.validator.0, "a@1", "no new validator");
    let cur = &t.items_by_remote(col, &keys(&["cur"])).unwrap()[0];
    assert_eq!(cur.mapping_version, 2, "changed raw body is skipped");
    assert!(t.items_by_remote(col, &keys(&["nope"])).unwrap().is_empty());
    let starts: Vec<_> = t
        .range(&window(0, 10))
        .unwrap()
        .occurrences
        .iter()
        .map(|o| o.start)
        .collect();
    assert_eq!(starts, vec![common::t(0), common::t(3), common::t(7)]);
    assert!(
        t.stale_items(Event, &[col], 1, 2, None, 10)
            .unwrap()
            .is_empty()
    );

    // Nothing changes: the head stays.
    let mut again = event("a", "1", 0);
    again.mapping_version = 2;
    assert_eq!(t.redecode(col, &[again]).unwrap(), after);
    assert_eq!(t.head().unwrap(), after);
    t.purge_collection(col).unwrap();
    assert!(matches!(t.redecode(col, &[]), Err(StoreError::NotFound)));
}

#[test]
fn redecoded_versions_round_trip() {
    use musubi_model::EntityKind::{Event, Task};
    use musubi_model::capability::ProviderId;
    let (_d, s, mut t) = setup();
    let google = ProviderId("google".into());
    let dav = ProviderId("dav".into());
    assert_eq!(t.redecoded(&google, Event).unwrap(), None, "absent");
    t.set_redecoded(&google, Event, 2, 3).unwrap();
    assert_eq!(t.redecoded(&google, Event).unwrap(), Some((2, 3)));
    assert_eq!(t.redecoded(&google, Task).unwrap(), None, "per kind");
    assert_eq!(t.redecoded(&dav, Event).unwrap(), None, "per provider");
    t.set_redecoded(&google, Event, 2, 4).unwrap();
    assert_eq!(t.redecoded(&google, Event).unwrap(), Some((2, 4)));
    drop(t);
    let t = s.open(TENANT).unwrap();
    assert_eq!(
        t.redecoded(&google, Event).unwrap(),
        Some((2, 4)),
        "durable"
    );
}

#[test]
fn push_resource_is_kept_until_purge() {
    let (d, s, mut t) = setup();
    let col = collection(&mut t, "cal");
    let push =
        |t: &musubi_store_sqlite::SqliteTenant| t.collections().unwrap()[0].push_resource.clone();
    assert_eq!(push(&t), None);
    t.set_push_resource(col, "res-1").unwrap();
    // Rediscovery, pages and status writes keep it.
    let mut renamed = remote("cal");
    renamed.name = "Calendar".into();
    t.apply_discovery(&DiscoveryWrite {
        account: ACCOUNT,
        collections: vec![renamed],
        complete: true,
    })
    .unwrap();
    t.apply_page(&page(col, vec![event("a", "1", 0)])).unwrap();
    t.set_collection_state(col, &CollectionStatus::default())
        .unwrap();
    // A channel swap (docs/plan/phase2.md 4.1) touches only `control.db`.
    channel_swap(&d, col);
    drop(t);
    let mut t = s.open(TENANT).unwrap();
    assert_eq!(push(&t).as_deref(), Some("res-1"));
    t.set_push_resource(col, "res-2").unwrap();
    assert_eq!(push(&t).as_deref(), Some("res-2"));
    t.purge_collection(col).unwrap();
    assert!(matches!(
        t.set_push_resource(col, "res-3"),
        Err(StoreError::NotFound)
    ));
}

/// Renews a channel of `col` the way the engine does: the new one pending,
/// then open, then the old one deleted.
fn channel_swap(d: &TempDir, col: musubi_model::CollectionId) {
    use musubi_model::control::{ChannelRecord, ChannelState};
    use musubi_model::provider::{ChannelId, Subscription};
    use musubi_model::{CollectionRef, ControlStore};
    let c = musubi_store_sqlite::SqliteControl::open(&d.path().join("control.db")).unwrap();
    let open = |id: &str| ChannelRecord {
        tenant: TENANT,
        account: ACCOUNT,
        collection: col,
        state: ChannelState::Open(Subscription::new(
            ChannelId(id.into()),
            CollectionRef("cal".into()),
            "res-1",
            secrecy::SecretString::from("tok"),
            common::t(100),
        )),
    };
    c.put_channel(&open("old")).unwrap();
    c.put_channel(&ChannelRecord {
        state: ChannelState::Pending(ChannelId("new".into())),
        ..open("new")
    })
    .unwrap();
    c.put_channel(&open("new")).unwrap();
    c.delete_channel(&ChannelId("old".into())).unwrap();
}
