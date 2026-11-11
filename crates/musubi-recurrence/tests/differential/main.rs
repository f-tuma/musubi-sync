//! Differential fixtures: the phase 0 corpus (`cases.json`) against its stored expectations
//! (`expected.json`), both copied from `spikes/recurrence/corpus/`.
//!
//! The expectations were produced by python-dateutil and ical.js (the oracles) plus hand
//! decisions recorded per case (`decided_by`, `why`). Nothing here runs an oracle: to change
//! the corpus, regenerate it in `spikes/recurrence` (`./run.sh`, then `compare.py --seed`) and
//! copy both files back. Scoring follows `spikes/recurrence/compare.py`: starts are compared as
//! strings (`...Z` for zoned, no suffix for floating, `YYYY-MM-DD` for all-day), clipped to the
//! case window `[lo, hi)` and to its `limit`.

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, ToSpan};
use musubi_recurrence::{Moment, Override, Series, expand};
use serde_json::Value;

/// Cases where a loud error is the phase 1 behaviour, while the oracle (with the VTIMEZONE
/// in hand) expands. Matching a custom VTIMEZONE body or a Windows zone name is out of scope
/// for phase 1 (recurrence.md, fix 8 covers IANA zones; `resolve_tzid`).
const LOUD_ERROR: &[&str] = &["vtimezone-windows-name", "vtimezone-custom-rules"];

#[test]
fn corpus_matches_expectations() {
    let cases = load("cases.json");
    let cases = cases.as_array().unwrap();
    let expected = load("expected.json");
    assert_eq!(
        cases.len(),
        89,
        "corpus size changed; update this count deliberately"
    );

    let mut diffs = Vec::new();
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let exp = &expected[id];
        let got = run_case(case);
        let pass = match &got {
            Ok(starts) => {
                !LOUD_ERROR.contains(&id)
                    && exp["accept"].as_array().unwrap().iter().any(|a| {
                        let a: Vec<&str> = a
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|s| s.as_str().unwrap())
                            .collect();
                        a == *starts
                    })
            }
            // `error_ok` cases are all decided by policy now (whole-day EXDATE, EXRULE, lenient
            // UNTIL, TZID suffix), so only the loud-error cases may fail.
            Err(_) => LOUD_ERROR.contains(&id),
        };
        if !pass {
            diffs.push(format!("{id}: got {got:?}, accepted {}", exp["accept"]));
        }
    }
    assert!(
        diffs.is_empty(),
        "{} diffs:\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

fn load(name: &str) -> Value {
    let path = format!("{}/tests/differential/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

// ---------- the case runner ----------

fn run_case(case: &Value) -> Result<Vec<String>, String> {
    let ics = case["ics"].as_str().unwrap();
    let (series, kind) = series_of(ics)?;
    let window = case["window"]
        .as_array()
        .map(|w| key(w[0].as_str().unwrap())..key(w[1].as_str().unwrap()));
    let limit = case["limit"].as_u64().map(|l| l as usize);
    let until = key(case["iter_until"].as_str().unwrap());
    let max = case["iter_max"].as_u64().unwrap() as usize;
    let cap = 200_000;

    let mut starts: Vec<Timestamp> = match window {
        Some(w) => run(&series, w, cap)?,
        None => {
            // No window: the oracles iterated from DTSTART to `iter_until`, at most `iter_max`
            // (or `limit`) items. Grow a window from DTSTART until that many are found.
            let want = limit.unwrap_or(max);
            let lo = key("18000101T000000Z");
            let mut out = Vec::new();
            for years in [1, 4, 16, 64, 256, 1024, 8000] {
                let hi = jiff_add(series.start, years.years())
                    .min(until.checked_add(1.second()).unwrap_or(Timestamp::MAX));
                out = run(&series, lo..hi, cap)?;
                if out.len() >= want || hi > until {
                    break;
                }
            }
            out.truncate(want);
            out
        }
    };
    if let Some(l) = limit {
        starts.truncate(l);
    }
    Ok(starts.into_iter().map(|t| fmt(kind, t)).collect())
}

fn run(
    series: &Series,
    window: std::ops::Range<Timestamp>,
    cap: usize,
) -> Result<Vec<Timestamp>, String> {
    let lo = window.start;
    let hi = window.end;
    let out = expand(series, window, cap).map_err(|e| e.to_string())?;
    // compare.py clips by start; with zero durations, overlap and start-in-window coincide.
    Ok(out
        .into_iter()
        .map(|i| i.start)
        .filter(|s| lo <= *s && *s < hi)
        .collect())
}

fn jiff_add(start: DateTime, span: Span) -> Timestamp {
    key_of(
        start
            .saturating_add(span)
            .min(jiff::civil::date(9999, 1, 1).at(0, 0, 0, 0)),
    )
}

/// `20260427T000000Z` (or without `Z`) as a key: wall time read as UTC.
fn key(s: &str) -> Timestamp {
    key_of(civil(s.trim_end_matches('Z')).unwrap())
}

fn key_of(dt: DateTime) -> Timestamp {
    // `iter_until` 9999-12-31 is past the last representable instant.
    dt.to_zoned(TimeZone::UTC)
        .map_or(Timestamp::MAX, |z| z.timestamp())
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Instant,
    Floating,
    Date,
}

fn fmt(kind: Kind, t: Timestamp) -> String {
    let dt = t.to_zoned(TimeZone::UTC).datetime();
    match kind {
        Kind::Instant => dt.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
        Kind::Floating => dt.strftime("%Y-%m-%dT%H:%M:%S").to_string(),
        Kind::Date => dt.strftime("%Y-%m-%d").to_string(),
    }
}

// ---------- minimal ICS reading (ported from spikes/recurrence/candidates) ----------

struct Prop {
    name: String,
    params: String,
    value: String,
}

impl Prop {
    fn param(&self, key: &str) -> Option<&str> {
        self.params.split(';').skip(1).find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            k.eq_ignore_ascii_case(key).then_some(v)
        })
    }

    fn is_date(&self) -> bool {
        self.param("VALUE")
            .is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
            || self.value.len() == 8
    }
}

fn parse_prop(l: &str) -> Prop {
    let (head, value) = l.split_once(':').unwrap_or((l, ""));
    let (name, params) = head.split_at(head.find(';').unwrap_or(head.len()));
    Prop {
        name: name.to_ascii_uppercase(),
        params: params.into(),
        value: value.into(),
    }
}

fn events(ics: &str) -> Vec<Vec<Prop>> {
    let unfolded = ics
        .replace("\r\n", "\n")
        .replace("\n ", "")
        .replace("\n\t", "");
    let (mut out, mut cur, mut in_event) = (vec![], vec![], false);
    for l in unfolded.lines() {
        match l {
            "BEGIN:VEVENT" => in_event = true,
            "END:VEVENT" => {
                in_event = false;
                out.push(std::mem::take(&mut cur));
            }
            _ if in_event => cur.push(parse_prop(l)),
            _ => {}
        }
    }
    out
}

fn civil(s: &str) -> Result<DateTime, String> {
    DateTime::strptime("%Y%m%dT%H%M%S", s).map_err(|e| format!("{s}: {e}"))
}

fn zone(id: &str) -> Result<TimeZone, String> {
    musubi_recurrence::resolve_tzid(id).map_err(|e| e.to_string())
}

/// The series of the master VEVENT plus its overrides, and how its starts are printed.
fn series_of(ics: &str) -> Result<(Series, Kind), String> {
    let evs = events(ics);
    let is_override = |e: &Vec<Prop>| e.iter().any(|p| p.name == "RECURRENCE-ID");
    let master = evs.iter().find(|e| !is_override(e)).ok_or("no master")?;
    let dtstart = master
        .iter()
        .find(|p| p.name == "DTSTART")
        .ok_or("no DTSTART")?;
    let tzid = dtstart.param("TZID");
    let (start, zone, kind) = if dtstart.is_date() {
        let d = Date::strptime("%Y%m%d", &dtstart.value).map_err(|e| e.to_string())?;
        (
            d.to_datetime(jiff::civil::Time::midnight()),
            None,
            Kind::Date,
        )
    } else if let Some(id) = tzid {
        (civil(&dtstart.value)?, Some(zone(id)?), Kind::Instant)
    } else if let Some(v) = dtstart.value.strip_suffix('Z') {
        (civil(v)?, Some(TimeZone::UTC), Kind::Instant)
    } else {
        (civil(&dtstart.value)?, None, Kind::Floating)
    };
    let mut series = Series {
        start,
        zone,
        duration: Span::new(),
        rules: vec![],
        exrules: vec![],
        rdates: vec![],
        exdates: vec![],
        overrides: vec![],
    };
    for p in master {
        match p.name.as_str() {
            "RRULE" => series.rules.push(p.value.clone()),
            "EXRULE" => series.exrules.push(p.value.clone()),
            "RDATE" => series.rdates.extend(moments(p, tzid)?),
            "EXDATE" => series.exdates.extend(moments(p, tzid)?),
            _ => {}
        }
    }
    for o in evs.iter().filter(|e| is_override(e)) {
        let rid = o.iter().find(|p| p.name == "RECURRENCE-ID").unwrap();
        let start = o
            .iter()
            .find(|p| p.name == "DTSTART")
            .ok_or("override without DTSTART")?;
        series.overrides.push(Override {
            recurrence_id: moments(rid, tzid)?[0],
            start: moments(start, tzid)?[0],
            duration: None,
            this_and_future: rid
                .param("RANGE")
                .is_some_and(|v| v.eq_ignore_ascii_case("THISANDFUTURE")),
        });
    }
    Ok((series, kind))
}

/// The values of a DATE / DATE-TIME / PERIOD property. A value in the series' own TZID stays
/// local, so it goes through exactly the gap/fold resolution of the occurrences.
fn moments(p: &Prop, series_tzid: Option<&str>) -> Result<Vec<Moment>, String> {
    let date = p.is_date();
    p.value
        .split(',')
        .map(|v| {
            let v = v.split('/').next().unwrap(); // PERIOD: its start
            if date {
                return Ok(Moment::Day(
                    Date::strptime("%Y%m%d", v).map_err(|e| e.to_string())?,
                ));
            }
            if let Some(utc) = v.strip_suffix('Z') {
                return Ok(Moment::Instant(key(utc)));
            }
            match p.param("TZID") {
                Some(id) if Some(id) != series_tzid => Ok(Moment::Instant(
                    civil(v)?
                        .to_zoned(zone(id)?)
                        .map_err(|e| e.to_string())?
                        .timestamp(),
                )),
                _ => Ok(Moment::Local(civil(v)?)),
            }
        })
        .collect()
}
