//! B9 (plan 6.5): 10 000 series expanded over 2026–2031, a regression guard
//! (phase 0: 2.2–3.1 s on its own corpus; this set is synthetic: daily,
//! weekly, monthly and yearly rules in three zones and floating, some with
//! COUNT and EXDATE). `MUSUBI_BENCH_SMOKE=1` expands 1 000.
//!
//! Run: `cargo bench -p musubi-recurrence --bench expand --locked`.

use std::time::Instant;

use jiff::ToSpan;
use jiff::civil::date;
use jiff::tz::TimeZone;
use musubi_recurrence::{Moment, Series, expand};

const RULES: [&str; 8] = [
    "FREQ=DAILY",
    "FREQ=WEEKLY;BYDAY=MO,WE,FR",
    "FREQ=WEEKLY;INTERVAL=2",
    "FREQ=MONTHLY;BYDAY=2TU",
    "FREQ=MONTHLY;BYMONTHDAY=15",
    "FREQ=YEARLY",
    "FREQ=WEEKLY;COUNT=20",
    "FREQ=DAILY;COUNT=400",
];

fn main() {
    let smoke = std::env::var_os("MUSUBI_BENCH_SMOKE").is_some();
    let n = if smoke { 1_000 } else { 10_000 };
    let zones = [
        Some("Europe/Prague"),
        Some("America/New_York"),
        Some("UTC"),
        None,
    ];
    let series: Vec<Series> = (0..n)
        .map(|i| {
            let start = date(2024, 1, 1).at(9, 0, 0, 0) + (i as i64 * 7).hours();
            Series {
                start,
                zone: zones[i % 4].map(|z| TimeZone::get(z).expect("tzdb")),
                duration: 1.hour(),
                rules: vec![RULES[i % RULES.len()].to_owned()],
                exrules: Vec::new(),
                rdates: Vec::new(),
                exdates: if i % 5 == 0 {
                    vec![Moment::Local(start + 7.days())]
                } else {
                    Vec::new()
                },
                overrides: Vec::new(),
            }
        })
        .collect();
    let window =
        "2026-01-01T00:00:00Z".parse().expect("ts").."2031-01-01T00:00:00Z".parse().expect("ts");
    let t0 = Instant::now();
    let mut occurrences = 0;
    for s in &series {
        occurrences += expand(s, window.clone(), 1_000_000).expect("expand").len();
    }
    let secs = t0.elapsed().as_secs_f64();
    println!("B9 recurrence: {n} series over 2026–2031, {occurrences} occurrences, {secs:.2} s");
    // Phase 0's upper bound, scaled to the series count.
    let budget = 3.1 * n as f64 / 10_000.0;
    assert!(
        secs < budget,
        "B9 regression: {secs:.2} s, budget {budget:.2} s"
    );
}
