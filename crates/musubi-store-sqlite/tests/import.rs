//! 50k-event import with concurrent readers: the WAL stays bounded (plan K1).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use jiff::tz::TimeZone;
use musubi_model::remote::MediaType;
use musubi_model::store::RangeQuery;
use musubi_model::{Raw, Seq, Storage, TenantStore, TimeWindow};

const EVENTS: usize = 50_000;
const PAGE: usize = 500;
/// Body size of a typical event with a description and attendees.
const BODY: usize = 4 << 10;

#[test]
fn import_keeps_wal_bounded() {
    let d = TempDir::new();
    let s = storage(d.path());
    let mut tn = s.open(TENANT).unwrap();
    let col = collection(&mut tn, "a");
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4u64)
        .map(|r| {
            let tn = s.open(TENANT).unwrap();
            let stop = stop.clone();
            std::thread::spawn(move || {
                // API-shaped reads: a calendar view and an SSE consumer tailing the journal.
                // (Not `stats()`: its counts scan the tables, a monitoring call.)
                let (mut reads, mut since) = (0u64, Seq(0));
                while !stop.load(Ordering::Relaxed) {
                    let day = ((reads * 7 + r * 91) % 365) as i64;
                    tn.range(&RangeQuery {
                        window: TimeWindow::new(t(24 * day), t(24 * (day + 7))).unwrap(),
                        collections: Some(vec![col]),
                        zone: TimeZone::UTC,
                        after: None,
                        limit: 200,
                        ids_only: false,
                    })
                    .unwrap();
                    since = tn.changes(since, 1000).unwrap().next;
                    reads += 1;
                }
                reads
            })
        })
        .collect();

    let start = Instant::now();
    for p in 0..EVENTS / PAGE {
        let items = (0..PAGE)
            .map(|i| {
                let n = p * PAGE + i;
                let mut w = event(&format!("e{n}"), "1", (n % (24 * 365)) as i64);
                w.raw = Raw {
                    media_type: MediaType::Json,
                    body: format!(r#"{{"id":"e{n}","pad":"{}"}}"#, "x".repeat(BODY)).into_bytes(),
                };
                w.raw_hash = w.raw.hash();
                w
            })
            .collect();
        tn.apply_page(&page(col, items)).unwrap();
    }
    let took = start.elapsed();
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();

    let st = tn.stats().unwrap();
    let waited = tn.gate_wait_max();
    println!(
        "{EVENTS} events in {took:?}, {reads} concurrent reads, reads waited at most {waited:?}; {st:?}"
    );
    assert_eq!(st.items, EVENTS as u64);
    assert!(reads > 0);
    assert!(
        st.wal_max_bytes <= 256 << 20,
        "largest WAL {} bytes",
        st.wal_max_bytes
    );
    // Release (CI runs this test so too) outpaces the background PASSIVE and
    // resets the WAL; a debug build is too slow to get there.
    if cfg!(not(debug_assertions)) {
        assert!(st.truncates > 0, "{st:?}");
    }
    // A read waits for at most one reset: the 10 ms drain plus the TRUNCATE,
    // both inside `truncate_max` (~10 ms locally, ~0.5 s on a CI disk).
    assert!(
        waited <= st.truncate_max + Duration::from_millis(100) && waited < Duration::from_secs(5),
        "reads waited {waited:?}; {st:?}"
    );

    // A page is capped whatever the caller asks.
    let all = tn.changes(Seq(0), u32::MAX).unwrap();
    assert_eq!((all.changes.len(), all.more), (10_000, true));
}
