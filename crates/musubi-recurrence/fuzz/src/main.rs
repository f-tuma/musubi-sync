//! AFL harness: decodes the input into a [`Series`], a window and a cap, calls [`expand`] and
//! checks its invariants. A panic (in the core or here) is a crash.
//!
//! Adapted from rust-rrule's `rrule-afl-fuzz` (copyright 2021 Fredrik Meringdal, Ralph
//! Bisschops; MIT or Apache-2.0): the byte readers and the rule fields come from its
//! `take_data.rs` and `take_rrule.rs`. Upstream builds an `RRule` value; we build the RRULE
//! text instead, because the core is private and the text is what the edge receives.

use std::collections::BTreeSet;
use std::ops::Range;

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};
use musubi_recurrence::{Moment, Override, Series, expand};

fn main() {
    afl::fuzz!(|data: &[u8]| {
        let mut data = data;
        if let Some((series, window, limit)) = take_case(&mut data) {
            check(&series, window, limit);
        }
    });
}

fn check(series: &Series, window: Range<Timestamp>, limit: usize) {
    let Ok(out) = expand(series, window.clone(), limit) else {
        return; // every error is an acceptable answer; only a panic or a wrong result is not
    };
    assert!(out.len() <= limit, "{} > cap {limit}", out.len());
    assert!(
        out.windows(2).all(|p| p[0].start <= p[1].start),
        "not sorted"
    );
    let rids: BTreeSet<_> = out.iter().map(|i| i.recurrence_id).collect();
    assert_eq!(rids.len(), out.len(), "duplicate recurrence id");
    for i in &out {
        let overlaps = i.start < window.end
            && (i.end > window.start || (i.start == i.end && i.start >= window.start));
        assert!(overlaps, "{i:?} outside {window:?}");
    }
}

const ZONES: &[Option<&str>] = &[
    None,
    Some("UTC"),
    Some("Europe/Prague"),
    Some("America/New_York"),
    Some("Australia/Lord_Howe"),
    Some("Pacific/Apia"),
    Some("Asia/Kathmandu"),
    Some("America/St_Johns"),
];

fn take_case(input: &mut &[u8]) -> Option<(Series, Range<Timestamp>, usize)> {
    let zone =
        ZONES[usize::from(take_byte(input)) % ZONES.len()].map(|z| jiff::tz::db().get(z).unwrap());
    let start = take_datetime(input)?;
    let mut series = Series {
        start,
        zone,
        duration: Span::new()
            .try_minutes(i64::from(take_data_i16(input)))
            .ok()?,
        rules: vec![],
        exrules: vec![],
        rdates: (0..take_byte(input) % 4)
            .filter_map(|_| take_moment(input))
            .collect(),
        exdates: (0..take_byte(input) % 4)
            .filter_map(|_| take_moment(input))
            .collect(),
        overrides: vec![],
    };
    for _ in 0..take_byte(input) % 3 {
        series.overrides.push(Override {
            recurrence_id: take_moment(input)?,
            start: take_moment(input)?,
            duration: None,
            this_and_future: take_byte(input) % 2 == 0,
        });
    }
    // Window: around DTSTART read as UTC, up to ~7 years long, so it usually meets the series.
    let lo = start.to_zoned(TimeZone::UTC).ok()?.timestamp();
    let lo = lo
        .checked_add(Span::new().hours(i64::from(take_data_i16(input))))
        .ok()?;
    let hi = lo
        .checked_add(Span::new().hours(i64::from(take_data_u16(input))))
        .ok()?;
    let limit = usize::from(take_data_u16(input) % 2048);

    // The rest: either free text (one rule per line, `!` marks an EXRULE) or rules built
    // field by field, which reaches the core's arithmetic more often than random text.
    if take_byte(input) % 4 == 0 {
        for line in String::from_utf8_lossy(input).lines() {
            match line.strip_prefix('!') {
                Some(ex) => series.exrules.push(ex.to_owned()),
                None => series.rules.push(line.to_owned()),
            }
        }
    } else {
        for _ in 0..1 + take_byte(input) % 2 {
            series.rules.push(take_rule(input));
        }
        for _ in 0..take_byte(input) % 2 {
            series.exrules.push(take_rule(input));
        }
    }
    Some((series, lo..hi, limit))
}

/// An RRULE value; the fields follow upstream `take_rrule_from_data`.
fn take_rule(input: &mut &[u8]) -> String {
    const FREQ: [&str; 7] = [
        "YEARLY", "MONTHLY", "WEEKLY", "DAILY", "HOURLY", "MINUTELY", "SECONDLY",
    ];
    const DAY: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
    let mut r = format!("FREQ={}", FREQ[usize::from(take_byte(input)) % 7]);
    r += &format!(";INTERVAL={}", take_data_u16(input) % 400);
    if take_byte(input) % 2 == 0 {
        r += &format!(";COUNT={}", take_data_u16(input));
    }
    if take_byte(input) % 2 == 0
        && let Some(u) = take_datetime(input)
    {
        // All three UNTIL forms: UTC, local, DATE.
        r += &match take_byte(input) % 3 {
            0 => u.strftime(";UNTIL=%Y%m%dT%H%M%SZ").to_string(),
            1 => u.strftime(";UNTIL=%Y%m%dT%H%M%S").to_string(),
            _ => u.strftime(";UNTIL=%Y%m%d").to_string(),
        };
    }
    r += &format!(";WKST={}", DAY[usize::from(take_byte(input)) % 7]);
    let mut list = |name: &str, items: Vec<String>| {
        if !items.is_empty() {
            r += &format!(";{name}={}", items.join(","));
        }
    };
    list(
        "BYSETPOS",
        take_vec(input, |i| (take_data_i16(i) % 400).to_string()),
    );
    list(
        "BYMONTH",
        take_vec(input, |i| (take_byte(i) % 14).to_string()),
    );
    list(
        "BYMONTHDAY",
        take_vec(input, |i| (take_data_i8(i) % 33).to_string()),
    );
    list(
        "BYYEARDAY",
        take_vec(input, |i| (take_data_i16(i) % 370).to_string()),
    );
    list(
        "BYWEEKNO",
        take_vec(input, |i| (take_data_i8(i) % 55).to_string()),
    );
    list(
        "BYDAY",
        take_vec(input, |i| {
            let day = DAY[usize::from(take_byte(i)) % 7];
            match take_byte(i) % 2 {
                0 => day.to_owned(),
                _ => format!("{}{day}", take_data_i8(i) % 54),
            }
        }),
    );
    list(
        "BYHOUR",
        take_vec(input, |i| (take_byte(i) % 25).to_string()),
    );
    list(
        "BYMINUTE",
        take_vec(input, |i| (take_byte(i) % 61).to_string()),
    );
    list(
        "BYSECOND",
        take_vec(input, |i| (take_byte(i) % 62).to_string()),
    );
    r
}

fn take_moment(input: &mut &[u8]) -> Option<Moment> {
    Some(match take_byte(input) % 3 {
        0 => Moment::Local(take_datetime(input)?),
        1 => Moment::Instant(
            take_datetime(input)?
                .to_zoned(TimeZone::UTC)
                .ok()?
                .timestamp(),
        ),
        _ => Moment::Day(take_date(input)?),
    })
}

/// Years 1800..=2311, so most inputs land where real calendars are.
fn take_date(input: &mut &[u8]) -> Option<Date> {
    let year = 1800 + (take_data_u16(input) % 512) as i16;
    Date::new(
        year,
        (take_byte(input) % 13) as i8,
        (take_byte(input) % 32) as i8,
    )
    .ok()
}

/// Whole seconds, except that a set high bit of the second byte adds a fraction (an error path).
fn take_datetime(input: &mut &[u8]) -> Option<DateTime> {
    let date = take_date(input)?;
    let (h, m, s) = (
        take_byte(input) % 24,
        take_byte(input),
        take_byte(input) % 60,
    );
    let nanos = if m & 0x80 != 0 { 500_000_000 } else { 0 };
    date.at(h as i8, (m % 60) as i8, s as i8, nanos).into()
}

// ---------- byte readers (upstream take_data.rs): missing input reads as zero ----------

/// Up to 5 items, as upstream `take_vec_*`.
fn take_vec(input: &mut &[u8], mut item: impl FnMut(&mut &[u8]) -> String) -> Vec<String> {
    (0..take_byte(input) % 6).map(|_| item(input)).collect()
}

fn take<const N: usize>(input: &mut &[u8]) -> [u8; N] {
    let mut out = [0; N];
    if let Some((head, rest)) = input.split_first_chunk::<N>() {
        out = *head;
        *input = rest;
    }
    out
}

fn take_byte(input: &mut &[u8]) -> u8 {
    take::<1>(input)[0]
}

fn take_data_i8(input: &mut &[u8]) -> i8 {
    i8::from_be_bytes(take(input))
}

fn take_data_i16(input: &mut &[u8]) -> i16 {
    i16::from_be_bytes(take(input))
}

fn take_data_u16(input: &mut &[u8]) -> u16 {
    u16::from_be_bytes(take(input))
}
