//! Named regressions from docs/research/phase0/recurrence.md (verdict, fixes 1-12) and the
//! edge's contract: window, cap, overlap, keys.

use jiff::civil::{DateTime, date};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, ToSpan};
use musubi_recurrence::{
    Instance, Moment, Override, RecurrenceError, Series, expand, recurrence_id, resolve_tzid,
};

fn prague() -> Option<TimeZone> {
    Some(jiff::tz::db().get("Europe/Prague").unwrap())
}

fn series(start: DateTime, zone: Option<TimeZone>, rules: &[&str]) -> Series {
    Series {
        start,
        zone,
        duration: Span::new(),
        rules: rules.iter().map(|r| r.to_string()).collect(),
        exrules: vec![],
        rdates: vec![],
        exdates: vec![],
        overrides: vec![],
    }
}

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn wide() -> std::ops::Range<Timestamp> {
    ts("2000-01-01T00:00:00Z")..ts("2100-01-01T00:00:00Z")
}

fn starts(s: &Series) -> Vec<String> {
    expand(s, wide(), 1000)
        .unwrap()
        .iter()
        .map(|i| i.start.to_string())
        .collect()
}

/// Fix 1: DTSTART inside the spring-forward gap goes to the core as civil time and is
/// resolved by the edge (offset before the gap: 02:30 CET = 01:30Z); the next day is normal.
#[test]
fn dst_prague_explicit_gap_dtstart() {
    let s = series(
        date(2026, 3, 29).at(2, 30, 0, 0),
        prague(),
        &["FREQ=DAILY;COUNT=2"],
    );
    assert_eq!(starts(&s), ["2026-03-29T01:30:00Z", "2026-03-30T00:30:00Z"]);
}

/// Fix 2: UNTIL before DTSTART is not an error; the series is DTSTART alone.
#[test]
fn until_before_dtstart() {
    let s = series(
        date(2026, 3, 2).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;UNTIL=20260301T000000Z"],
    );
    assert_eq!(starts(&s), ["2026-03-02T08:00:00Z"]);
}

/// Fix 3: no RRULE at all; DTSTART plus RDATEs.
#[test]
fn rdate_only() {
    let mut s = series(date(2026, 4, 1).at(9, 0, 0, 0), prague(), &[]);
    s.rdates = vec![
        Moment::Local(date(2026, 4, 5).at(9, 0, 0, 0)),
        Moment::Local(date(2026, 4, 7).at(9, 0, 0, 0)),
    ];
    assert_eq!(
        starts(&s),
        [
            "2026-04-01T07:00:00Z",
            "2026-04-05T07:00:00Z",
            "2026-04-07T07:00:00Z"
        ]
    );
}

/// Fix 4: DTSTART (a Tuesday) is not produced by `BYDAY=MO`, yet it is the first of COUNT=3.
/// A synchronised DTSTART must not lose the last occurrence.
#[test]
fn dtstart_not_in_rule() {
    let s = series(
        date(2026, 3, 10).at(9, 0, 0, 0),
        prague(),
        &["FREQ=WEEKLY;BYDAY=MO;COUNT=3"],
    );
    assert_eq!(
        starts(&s),
        [
            "2026-03-10T08:00:00Z",
            "2026-03-16T08:00:00Z",
            "2026-03-23T08:00:00Z"
        ]
    );
    let s = series(
        date(2026, 3, 9).at(9, 0, 0, 0),
        prague(),
        &["FREQ=WEEKLY;BYDAY=MO;COUNT=3"],
    );
    assert_eq!(
        starts(&s),
        [
            "2026-03-09T08:00:00Z",
            "2026-03-16T08:00:00Z",
            "2026-03-23T08:00:00Z"
        ]
    );
}

/// Floating time never meets the host zone: 02:30 on 2026-03-29 exists in floating time even
/// where the host's zone has a gap. Checked in-process and again in a child process with
/// `TZ=Europe/Prague` (where a leak of the host zone would shift or drop that instance).
#[test]
fn floating_daily_host_gap() {
    floating_daily_host_gap_in_this_process();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "floating_daily_host_gap_in_this_process",
            "--nocapture",
        ])
        .env("TZ", "Europe/Prague")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{stdout}"
    );
}

#[test]
fn floating_daily_host_gap_in_this_process() {
    let s = series(
        date(2026, 3, 28).at(2, 30, 0, 0),
        None,
        &["FREQ=DAILY;COUNT=3"],
    );
    // Floating keys are the wall time read as UTC.
    assert_eq!(
        starts(&s),
        [
            "2026-03-28T02:30:00Z",
            "2026-03-29T02:30:00Z",
            "2026-03-30T02:30:00Z"
        ]
    );
}

/// Fix 5: a DATE EXDATE on a timed series removes that whole local day.
#[test]
fn exdate_date_on_timed() {
    let mut s = series(
        date(2026, 4, 1).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;COUNT=3"],
    );
    s.exdates = vec![Moment::Day(date(2026, 4, 2))];
    assert_eq!(starts(&s), ["2026-04-01T07:00:00Z", "2026-04-03T07:00:00Z"]);
}

/// Fix 6: EXRULE removes what it generates.
#[test]
fn exrule_excludes() {
    let mut s = series(
        date(2026, 4, 1).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;COUNT=5"],
    );
    s.exrules = vec!["FREQ=DAILY;INTERVAL=2".into()];
    assert_eq!(starts(&s), ["2026-04-02T07:00:00Z", "2026-04-04T07:00:00Z"]);
}

/// Fix 7: UNTIL is compared as an exact instant, also inside the fold hour. On 2026-10-25
/// Prague repeats 02:00-03:00; 02:30 is its first pass (CEST, 00:30Z).
#[test]
fn until_in_fold_hour() {
    let start = date(2026, 10, 24).at(2, 30, 0, 0);
    let s = series(start, prague(), &["FREQ=DAILY;UNTIL=20261025T003000Z"]);
    assert_eq!(starts(&s), ["2026-10-24T00:30:00Z", "2026-10-25T00:30:00Z"]);
    let s = series(start, prague(), &["FREQ=DAILY;UNTIL=20261025T002959Z"]);
    assert_eq!(starts(&s), ["2026-10-24T00:30:00Z"]);
    // A local UNTIL (invalid per RFC, still sent) is read in the series zone: 02:30 local is
    // the same first pass, so the occurrence is included.
    let s = series(start, prague(), &["FREQ=DAILY;UNTIL=20261025T023000"]);
    assert_eq!(starts(&s), ["2026-10-24T00:30:00Z", "2026-10-25T00:30:00Z"]);
    // A DATE UNTIL on a timed series means through the end of that local day.
    let s = series(start, prague(), &["FREQ=DAILY;UNTIL=20261025"]);
    assert_eq!(starts(&s), ["2026-10-24T00:30:00Z", "2026-10-25T00:30:00Z"]);
}

/// Fix 9: a THISANDFUTURE override shifts the local time of this and later occurrences,
/// keeping their recurrence ids; a plain override moves one occurrence.
#[test]
fn overrides() {
    let mut s = series(
        date(2026, 4, 6).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;COUNT=4"],
    );
    s.overrides = vec![
        Override {
            recurrence_id: Moment::Local(date(2026, 4, 7).at(9, 0, 0, 0)),
            start: Moment::Local(date(2026, 4, 7).at(10, 0, 0, 0)),
            duration: Some(30.minutes()),
            this_and_future: true,
        },
        Override {
            recurrence_id: Moment::Local(date(2026, 4, 9).at(9, 0, 0, 0)),
            start: Moment::Instant(ts("2026-04-20T12:00:00Z")),
            duration: None,
            this_and_future: false,
        },
    ];
    let got = expand(&s, wide(), 10).unwrap();
    let row = |rid: &str, start: &str, end: &str| Instance {
        recurrence_id: ts(rid),
        start: ts(start),
        end: ts(end),
    };
    assert_eq!(
        got,
        [
            row(
                "2026-04-06T07:00:00Z",
                "2026-04-06T07:00:00Z",
                "2026-04-06T07:00:00Z"
            ),
            row(
                "2026-04-07T07:00:00Z",
                "2026-04-07T08:00:00Z",
                "2026-04-07T08:30:00Z"
            ),
            row(
                "2026-04-08T07:00:00Z",
                "2026-04-08T08:00:00Z",
                "2026-04-08T08:30:00Z"
            ),
            row(
                "2026-04-09T07:00:00Z",
                "2026-04-20T12:00:00Z",
                "2026-04-20T12:00:00Z"
            ),
        ]
    );
}

/// The cap is an error, never a truncated result.
#[test]
fn cap_is_an_error() {
    let s = series(date(2026, 1, 1).at(9, 0, 0, 0), prague(), &["FREQ=DAILY"]);
    let window = ts("2026-01-01T00:00:00Z")..ts("2026-01-11T00:00:00Z");
    assert_eq!(expand(&s, window.clone(), 10).unwrap().len(), 10);
    assert_eq!(
        expand(&s, window, 9),
        Err(RecurrenceError::TooManyOccurrences { limit: 9 })
    );
}

/// Generation has a budget, independent of the caller's cap.
#[test]
fn generation_budget_is_an_error() {
    let s = series(date(1990, 1, 1).at(0, 0, 0, 0), None, &["FREQ=SECONDLY"]);
    let window = ts("2026-01-01T00:00:00Z")..ts("2026-01-01T00:00:01Z");
    assert_eq!(expand(&s, window, 10), Err(RecurrenceError::TooExpensive));
}

/// A rule that never matches inside the window ends instead of searching forever.
#[test]
fn impossible_rule_ends() {
    let s = series(
        date(2026, 1, 1).at(9, 0, 0, 0),
        None,
        &["FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30"],
    );
    let got = expand(
        &s,
        ts("2027-01-01T00:00:00Z")..ts("2030-01-01T00:00:00Z"),
        10,
    )
    .unwrap();
    assert!(got.is_empty());
}

/// Overlap: same rule as `musubi_model::TimeWindow::overlaps`.
#[test]
fn window_overlap() {
    let mut s = series(
        date(2026, 5, 1).at(0, 0, 0, 0),
        None,
        &["FREQ=DAILY;COUNT=3"],
    );
    s.duration = 1.day();
    let day = |d: i8| {
        date(2026, 5, d)
            .at(0, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    };
    // All-day 05-01 ends at 05-02 00:00, so it does not overlap a window from 05-02.
    let got = expand(&s, day(2)..day(3), 10).unwrap();
    assert_eq!(got.iter().map(|i| i.start).collect::<Vec<_>>(), [day(2)]);
    // A zero-length occurrence counts at the window start, not at its end.
    s.duration = Span::new();
    assert_eq!(expand(&s, day(2)..day(3), 10).unwrap().len(), 1);
    // Empty and reversed windows are empty.
    assert!(expand(&s, day(2)..day(2), 10).unwrap().is_empty());
    assert!(expand(&s, day(3)..day(2), 10).unwrap().is_empty());
}

/// Durations are nominal in the series zone: one day across spring-forward is 23 hours.
#[test]
fn nominal_duration_across_dst() {
    let mut s = series(
        date(2026, 3, 28).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;COUNT=1"],
    );
    s.duration = 1.day();
    let got = expand(&s, wide(), 10).unwrap();
    assert_eq!(got[0].end.duration_since(got[0].start).as_hours(), 23);
}

/// `recurrence_id` gives the same key as expansion, also for a local time in a gap.
#[test]
fn recurrence_id_matches_expansion() {
    let start = date(2026, 3, 29).at(2, 30, 0, 0);
    let s = series(start, prague(), &["FREQ=DAILY;COUNT=1"]);
    let rid = expand(&s, wide(), 10).unwrap()[0].recurrence_id;
    assert_eq!(recurrence_id(start, prague().as_ref()), Ok(rid));
    assert_eq!(recurrence_id(start, None), Ok(ts("2026-03-29T02:30:00Z")));
}

/// An instant has no meaning in a floating series.
#[test]
fn instant_in_floating_series_is_an_error() {
    let mut s = series(
        date(2026, 4, 1).at(9, 0, 0, 0),
        None,
        &["FREQ=DAILY;COUNT=3"],
    );
    s.exdates = vec![Moment::Instant(ts("2026-04-02T09:00:00Z"))];
    assert!(matches!(
        expand(&s, wide(), 10),
        Err(RecurrenceError::InstantInFloatingSeries(_))
    ));
}

#[test]
fn invalid_rules_are_errors() {
    for rule in [
        "COUNT=3",
        "FREQ=DAILY;FOO=1",
        "FREQ=DAILY;BYHOUR=24",
        "FREQ=DAILY;UNTIL=2026",
    ] {
        let s = series(date(2026, 4, 1).at(9, 0, 0, 0), prague(), &[rule]);
        assert!(expand(&s, wide(), 10).is_err(), "{rule}");
    }
}

fn this_and_future(rid: DateTime, start: DateTime) -> Override {
    Override {
        recurrence_id: Moment::Local(rid),
        start: Moment::Local(start),
        duration: None,
        this_and_future: true,
    }
}

/// Two THISANDFUTURE splits apply in RECURRENCE-ID order, whatever order they are given in.
#[test]
fn this_and_future_order_independent() {
    let mut s = series(
        date(2026, 4, 6).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;COUNT=5"],
    );
    let a = this_and_future(
        date(2026, 4, 7).at(9, 0, 0, 0),
        date(2026, 4, 7).at(10, 0, 0, 0),
    );
    let b = this_and_future(
        date(2026, 4, 9).at(9, 0, 0, 0),
        date(2026, 4, 9).at(12, 0, 0, 0),
    );
    let expected = [
        "2026-04-06T07:00:00Z",
        "2026-04-07T08:00:00Z",
        "2026-04-08T08:00:00Z",
        "2026-04-09T10:00:00Z",
        "2026-04-10T10:00:00Z",
    ];
    s.overrides = vec![a.clone(), b.clone()];
    assert_eq!(starts(&s), expected);
    s.overrides = vec![b, a];
    assert_eq!(starts(&s), expected);
}

/// An override is an explicit instance: an EXDATE on its RECURRENCE-ID does not hide it.
#[test]
fn override_of_exdated_occurrence_is_emitted() {
    let mut s = series(
        date(2026, 4, 6).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;COUNT=3"],
    );
    s.exdates = vec![Moment::Local(date(2026, 4, 7).at(9, 0, 0, 0))];
    s.overrides = vec![Override {
        recurrence_id: Moment::Local(date(2026, 4, 7).at(9, 0, 0, 0)),
        start: Moment::Local(date(2026, 4, 7).at(11, 0, 0, 0)),
        duration: None,
        this_and_future: false,
    }];
    let got = expand(&s, wide(), 10).unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[1].recurrence_id, ts("2026-04-07T07:00:00Z"));
    assert_eq!(got[1].start, ts("2026-04-07T09:00:00Z"));
}

/// An override whose RECURRENCE-ID matches no generated occurrence is still emitted.
#[test]
fn override_without_occurrence_is_added() {
    let mut s = series(
        date(2026, 4, 6).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;COUNT=2"],
    );
    s.overrides = vec![Override {
        recurrence_id: Moment::Local(date(2026, 4, 20).at(9, 0, 0, 0)),
        start: Moment::Local(date(2026, 4, 21).at(9, 0, 0, 0)),
        duration: None,
        this_and_future: false,
    }];
    let got = expand(&s, wide(), 10).unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[2].recurrence_id, ts("2026-04-20T07:00:00Z"));
    assert_eq!(got[2].start, ts("2026-04-21T07:00:00Z"));
}

/// Intended: 02:30 in the spring-forward gap moves to 03:30 CEST (RFC 5545 3.3.5), the same
/// instant as the next real occurrence, so COUNT=4 yields three instances.
#[test]
fn hourly_gap_merges() {
    let s = series(
        date(2026, 3, 29).at(0, 30, 0, 0),
        prague(),
        &["FREQ=HOURLY;COUNT=4"],
    );
    assert_eq!(
        starts(&s),
        [
            "2026-03-28T23:30:00Z",
            "2026-03-29T00:30:00Z",
            "2026-03-29T01:30:00Z"
        ]
    );
}

/// Known `TooExpensive` cases (fix 12, skip-ahead, is deferred): the series cannot be expanded
/// in this window, which is not the same as an empty series.
#[test]
fn too_expensive_known_cases() {
    // More than MAX_GENERATED minutes between DTSTART and the window.
    let s = series(
        date(2020, 1, 1).at(0, 0, 0, 0),
        prague(),
        &["FREQ=MINUTELY"],
    );
    let window = ts("2026-01-01T00:00:00Z")..ts("2026-01-01T01:00:00Z");
    assert_eq!(expand(&s, window, 100), Err(RecurrenceError::TooExpensive));
    // An impossible rule with the window more than 100 000 empty periods out (the core's
    // MAX_ITER_LOOP; 100 000 days is about 274 years).
    let s = series(
        date(2026, 1, 1).at(9, 0, 0, 0),
        prague(),
        &["FREQ=DAILY;BYMONTH=2;BYMONTHDAY=30"],
    );
    let window = ts("2400-01-01T00:00:00Z")..ts("2401-01-01T00:00:00Z");
    assert_eq!(expand(&s, window, 100), Err(RecurrenceError::TooExpensive));
}

#[test]
fn tzid_resolution() {
    let name = |id: &str| resolve_tzid(id).map(|tz| tz.iana_name().unwrap().to_owned());
    assert_eq!(name("Europe/Prague").unwrap(), "Europe/Prague");
    // tzdb aliases (backward links) keep the name they were looked up by.
    assert!(resolve_tzid("US/Eastern").is_ok());
    assert!(resolve_tzid("Europe/Kiev").is_ok());
    assert_eq!(
        name("/mozilla.org/20050126_1/Europe/Prague").unwrap(),
        "Europe/Prague"
    );
    assert_eq!(name("/Europe/Prague").unwrap(), "Europe/Prague");
    assert_eq!(
        name("/citadel.org/20190914_1/America/Argentina/Buenos_Aires").unwrap(),
        "America/Argentina/Buenos_Aires"
    );
    for bad in [
        "Central Europe Standard Time",
        "/custom/UTC",
        "Europe/Atlantis",
        "",
    ] {
        assert_eq!(
            resolve_tzid(bad).unwrap_err(),
            RecurrenceError::UnknownTimeZone(bad.to_owned()),
            "{bad}"
        );
    }
}

/// Fuzzer find (CI fuzz run 43083630956): a non-ASCII BYDAY value panicked inside the core's
/// weekday parser; it is an invalid rule.
#[test]
fn non_ascii_byday_is_invalid() {
    for rule in [
        "FREQ=WEEKLY;BYDAY=M\u{fffd}",
        "FREQ=WEEKLY;BYDAY=MO,\u{e9}WE",
    ] {
        let s = series(date(2026, 1, 1).at(9, 0, 0, 0), prague(), &[rule]);
        let window = ts("2026-01-01T00:00:00Z")..ts("2026-02-01T00:00:00Z");
        assert!(
            matches!(
                expand(&s, window, 100),
                Err(RecurrenceError::InvalidRule { .. })
            ),
            "{rule}"
        );
    }
}
