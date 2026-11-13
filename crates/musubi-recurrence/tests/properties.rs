//! Invariants of `expand` over generated series: sorted, inside the window, no duplicate
//! recurrence ids, at most the cap, and a cap overflow is an error rather than a cut.

use std::collections::BTreeSet;

use jiff::civil::{DateTime, date};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, ToSpan};
use musubi_recurrence::{Instance, Moment, Override, RecurrenceError, Series, expand};
use proptest::prelude::*;

const ZONES: &[Option<&str>] = &[
    None,
    Some("UTC"),
    Some("Europe/Prague"),
    Some("America/New_York"),
    Some("Australia/Lord_Howe"),
    Some("Pacific/Apia"),
];

fn local() -> impl Strategy<Value = DateTime> {
    (
        2020i16..2030,
        1i8..=12,
        1i8..=28,
        0i8..24,
        prop::sample::select(vec![0i8, 15, 30, 59]),
    )
        .prop_map(|(y, m, d, h, min)| date(y, m, d).at(h, min, 0, 0))
}

fn rule() -> impl Strategy<Value = String> {
    let freq = prop::sample::select(vec!["YEARLY", "MONTHLY", "WEEKLY", "DAILY", "HOURLY"]);
    let byday = prop::sample::subsequence(
        vec!["MO", "TU", "WE", "TH", "FR", "SA", "SU", "1MO", "-1FR"],
        0..3,
    );
    (
        freq,
        1u8..4,
        prop::option::of(1u16..40),
        prop::option::of(local()),
        byday,
        prop::option::of(prop::sample::subsequence(vec![1, 13, 29, 30, 31, -1], 1..3)),
        prop::option::of(prop::sample::subsequence(vec![0, 2, 3, 9, 23], 1..3)),
    )
        .prop_map(
            |(freq, interval, count, until, byday, bymonthday, byhour)| {
                let mut r = format!("FREQ={freq};INTERVAL={interval}");
                if let Some(c) = count {
                    r += &format!(";COUNT={c}");
                } else if let Some(u) = until {
                    r += &u.strftime(";UNTIL=%Y%m%dT%H%M%SZ").to_string();
                }
                if !byday.is_empty() {
                    r += &format!(";BYDAY={}", byday.join(","));
                }
                if let Some(d) = bymonthday {
                    r += &format!(
                        ";BYMONTHDAY={}",
                        d.iter().map(i32::to_string).collect::<Vec<_>>().join(",")
                    );
                }
                if let Some(h) = byhour {
                    r += &format!(
                        ";BYHOUR={}",
                        h.iter().map(i32::to_string).collect::<Vec<_>>().join(",")
                    );
                }
                r
            },
        )
}

fn moment() -> impl Strategy<Value = Moment> {
    prop_oneof![
        local().prop_map(Moment::Local),
        local().prop_map(|l| Moment::Day(l.date())),
    ]
}

prop_compose! {
    fn series()(
        start in local(),
        zone in prop::sample::select(ZONES),
        hours in 0i64..50,
        rules in prop::collection::vec(rule(), 0..3),
        exrules in prop::collection::vec(rule(), 0..2),
        rdates in prop::collection::vec(moment(), 0..4),
        exdates in prop::collection::vec(moment(), 0..4),
        overrides in prop::collection::vec((moment(), moment(), any::<bool>()), 0..3),
    ) -> Series {
        Series {
            start,
            zone: zone.map(|z| jiff::tz::db().get(z).unwrap()),
            duration: hours.hours(),
            rules,
            exrules,
            rdates,
            exdates,
            overrides: overrides
                .into_iter()
                .map(|(recurrence_id, start, this_and_future)| Override {
                    recurrence_id,
                    start,
                    duration: None,
                    this_and_future,
                })
                .collect(),
        }
    }
}

/// A window near the series start (where the occurrences are): `offset` hours after
/// DTSTART read as UTC, `len` hours long.
fn window(s: &Series, offset: i64, len: i64) -> std::ops::Range<Timestamp> {
    let lo = s.start.to_zoned(TimeZone::UTC).unwrap().timestamp() + offset.hours();
    lo..lo + len.hours()
}

/// `musubi_model::TimeWindow::overlaps` for a window that may be empty: the window is
/// half-open, so `[t, t)` contains no instant and nothing overlaps it, not even an occurrence
/// spanning `t` (`TimeWindow::new` rejects it; `expand` returns nothing for it).
fn overlaps(i: &Instance, w: &std::ops::Range<Timestamp>) -> bool {
    w.start < w.end
        && i.start < w.end
        && (i.end > w.start || (i.start == i.end && i.start >= w.start))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn expand_invariants(s in series(), offset in -720i64..24 * 300, len in 0i64..24 * 200, limit in 0usize..300) {
        let w = window(&s, offset, len);
        match expand(&s, w.clone(), limit) {
            Ok(out) => {
                prop_assert!(out.len() <= limit);
                prop_assert!(out.windows(2).all(|p| p[0].start <= p[1].start), "not sorted");
                let rids: BTreeSet<_> = out.iter().map(|i| i.recurrence_id).collect();
                prop_assert_eq!(rids.len(), out.len(), "duplicate recurrence id");
                for i in &out {
                    prop_assert!(overlaps(i, &w), "{:?} outside {:?}", i, w);
                }
            }
            Err(RecurrenceError::TooManyOccurrences { limit: l }) => {
                prop_assert_eq!(l, limit);
                let all = expand(&s, w, usize::MAX).unwrap();
                prop_assert!(all.len() > limit, "cap error with only {} occurrences", all.len());
            }
            // Generated rules may be invalid (e.g. BYHOUR with HOURLY is fine, BYDAY=1MO with
            // WEEKLY is not); those must be reported, not expanded. TooExpensive is a failure
            // here: windows stay within a year of DTSTART and no generated rule is impossible,
            // so the budget is never the honest answer (0 in 23 000 local cases).
            Err(RecurrenceError::InvalidRule { .. }) => {}
            Err(e) => prop_assert!(false, "unexpected error {e}"),
        }
    }

    /// A window is a filter, not a different expansion: an occurrence found in a wide window
    /// is found, identically, by any window it overlaps.
    #[test]
    fn narrower_window_is_a_subset(s in series(), offset in -720i64..24 * 300, len in 0i64..24 * 200) {
        narrower_is_subset(&s, offset, len)?;
    }
}

fn narrower_is_subset(s: &Series, offset: i64, len: i64) -> Result<(), TestCaseError> {
    let w = window(s, offset, len);
    let wide = date(2000, 1, 1)
        .at(0, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .unwrap()
        .timestamp()
        ..date(2050, 1, 1)
            .at(0, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp();
    let (all, part) = (expand(s, wide, 100_000), expand(s, w.clone(), 100_000));
    // As above, these windows never need the budget; only an invalid rule may stop here.
    for r in [&all, &part] {
        prop_assert!(
            !matches!(r, Err(RecurrenceError::TooExpensive)),
            "TooExpensive for a generated series"
        );
    }
    let (Ok(all), Ok(part)) = (all, part) else {
        return Ok(());
    };
    let expected: Vec<_> = all.into_iter().filter(|i| overlaps(i, &w)).collect();
    prop_assert_eq!(part, expected);
    Ok(())
}

/// Reported seed: 32 h occurrences overlap each other, and a zero-length window one hour
/// after DTSTART lies inside the first one.
#[test]
fn zero_length_window_inside_an_occurrence() {
    let s = Series {
        start: date(2020, 1, 1).at(0, 0, 0, 0),
        zone: None,
        duration: 32.hours(),
        rules: vec!["FREQ=DAILY;INTERVAL=1".into()],
        exrules: vec![],
        rdates: vec![],
        exdates: vec![],
        overrides: vec![],
    };
    narrower_is_subset(&s, 1, 0).unwrap();
}

#[test]
fn zero_duration_span_is_allowed() {
    let s = Series {
        start: date(2026, 1, 1).at(0, 0, 0, 0),
        zone: None,
        duration: Span::new(),
        rules: vec![],
        exrules: vec![],
        rdates: vec![],
        exdates: vec![],
        overrides: vec![],
    };
    let t = date(2026, 1, 1)
        .at(0, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .unwrap()
        .timestamp();
    assert_eq!(expand(&s, t..t + 1.second(), 1).unwrap().len(), 1);
}
