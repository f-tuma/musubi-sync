//! Runs one Rust recurrence candidate over the corpus (or a synthetic benchmark).
//!
//!   rrule-candidates run <candidate> ../corpus/cases.json > ../results/<candidate>.json
//!   rrule-candidates bench <candidate> <series> <years>
//!
//! Candidates: rrule, rrule2, rrule-jiff, calcard, calcard-next, parse-only (bench baseline).
//! Output strings match the oracles: zoned/UTC -> YYYY-MM-DDTHH:MM:SSZ,
//! floating -> YYYY-MM-DDTHH:MM:SS, DATE -> YYYY-MM-DD.
use serde_json::{json, Map, Value};
use std::{sync::mpsc, thread, time::Duration};

// ---------- minimal ICS reading (only what the rrule-family adapters need) ----------

struct Prop {
    name: String,
    params: String, // ";A=B;C=D" exactly as written
    value: String,
}

impl Prop {
    fn param(&self, key: &str) -> Option<&str> {
        self.params.split(';').skip(1).find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            k.eq_ignore_ascii_case(key).then_some(v)
        })
    }
    fn line(&self, name: &str) -> String {
        format!("{name}{}:{}", self.params, self.value)
    }
}

fn events(ics: &str) -> Vec<Vec<Prop>> {
    let unfolded = ics.replace("\r\n", "\n").replace("\n ", "").replace("\n\t", "");
    let (mut out, mut cur, mut in_event) = (vec![], vec![], false);
    for l in unfolded.lines() {
        match l {
            "BEGIN:VEVENT" => in_event = true,
            "END:VEVENT" => {
                in_event = false;
                out.push(std::mem::take(&mut cur));
            }
            _ if in_event => {
                let (head, value) = l.split_once(':').unwrap_or((l, ""));
                let (name, params) = head.split_at(head.find(';').unwrap_or(head.len()));
                cur.push(Prop { name: name.to_ascii_uppercase(), params: params.into(), value: value.into() });
            }
            _ => {}
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Instant, // UTC or TZID
    Floating,
    Date,
}

fn kind_of(p: &Prop) -> Kind {
    if p.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || p.value.len() == 8 {
        Kind::Date
    } else if p.value.ends_with('Z') || p.param("TZID").is_some() {
        Kind::Instant
    } else {
        Kind::Floating
    }
}

/// Seconds: UTC epoch for instants, "naive epoch" (wall time read as UTC) otherwise.
fn fmt(kind: Kind, secs: i64) -> String {
    let dt = chrono::DateTime::from_timestamp(secs, 0).expect("in range").naive_utc();
    match kind {
        Kind::Instant => dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        Kind::Floating => dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
        Kind::Date => dt.format("%Y-%m-%d").to_string(),
    }
}

fn utc_secs(s: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ").expect("iter_until").and_utc().timestamp()
}

struct Bounds {
    from: i64, // only used by calcard-next, which takes a window natively
    until: i64,
    max: usize,
}

type Out = Result<Vec<String>, String>;

// ---------- rrule-family: the crate expands DTSTART/RRULE/RDATE/EXDATE/EXRULE; overrides are shared ----------

/// `expand(set_text)` -> seconds; `parse_dt(dtstart_line)` -> seconds (same convention as `fmt`).
fn rrule_family(
    ics: &str,
    b: &Bounds,
    expand: impl Fn(&str, Kind, &Bounds) -> Result<Vec<i64>, String>,
    parse_dt: impl Fn(&str, Kind) -> Result<i64, String>,
) -> Out {
    let evs = events(ics);
    let is_override = |e: &Vec<Prop>| e.iter().any(|p| p.name == "RECURRENCE-ID");
    let master = evs.iter().find(|e| !is_override(e)).ok_or("no master")?;
    let dtstart = master.iter().find(|p| p.name == "DTSTART").ok_or("no DTSTART")?;
    let kind = kind_of(dtstart);
    let set: Vec<String> = master
        .iter()
        .filter(|p| matches!(p.name.as_str(), "DTSTART" | "RRULE" | "RDATE" | "EXDATE" | "EXRULE"))
        .map(|p| p.line(&p.name))
        .collect();
    let mut occ = expand(&set.join("\n"), kind, b)?;
    for o in evs.iter().filter(|e| is_override(e)) {
        let rid = o.iter().find(|p| p.name == "RECURRENCE-ID").unwrap();
        let start = o.iter().find(|p| p.name == "DTSTART").ok_or("override without DTSTART")?;
        let rid_line = format!(
            "DTSTART{}:{}",
            rid.params.split(';').filter(|kv| !kv.to_ascii_uppercase().starts_with("RANGE=")).collect::<Vec<_>>().join(";"),
            rid.value
        );
        let (r, n) = (parse_dt(&rid_line, kind)?, parse_dt(&start.line("DTSTART"), kind)?);
        if rid.param("RANGE").is_some_and(|v| v.eq_ignore_ascii_case("THISANDFUTURE")) {
            occ.iter_mut().filter(|d| **d >= r).for_each(|d| *d += n - r);
        } else {
            occ.retain(|d| *d != r);
            occ.push(n);
        }
    }
    occ.sort();
    occ.dedup();
    Ok(occ.into_iter().map(|s| fmt(kind, s)).collect())
}

macro_rules! chrono_rrule {
    ($name:ident, $krate:ident) => {
        fn $name(ics: &str, b: &Bounds) -> Out {
            use chrono::DateTime;
            let secs = |d: &DateTime<$krate::Tz>, k: Kind| match k {
                Kind::Instant => d.timestamp(),
                _ => d.naive_local().and_utc().timestamp(),
            };
            let expand = |text: &str, k: Kind, b: &Bounds| {
                let set: $krate::RRuleSet = text.parse().map_err(|e| format!("{e}"))?;
                Ok((&set)
                    .into_iter()
                    .map(|d| secs(&d, k))
                    .take_while(|s| *s <= b.until)
                    .take(b.max)
                    .collect())
            };
            let parse_dt = |line: &str, k: Kind| {
                let set: $krate::RRuleSet = format!("{line}\nRRULE:FREQ=DAILY;COUNT=1").parse().map_err(|e| format!("{e}"))?;
                Ok(secs(set.get_dt_start(), k))
            };
            rrule_family(ics, b, expand, parse_dt)
        }
    };
}
chrono_rrule!(run_rrule, rrule);
chrono_rrule!(run_rrule2, rrule2);

fn run_rrule_jiff(ics: &str, b: &Bounds) -> Out {
    let secs = |z: &jiff::Zoned, k: Kind| match k {
        Kind::Instant => z.timestamp().as_second(),
        _ => z.datetime().to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp().as_second(),
    };
    let expand = |text: &str, k: Kind, b: &Bounds| {
        let set: rrule_jiff::RRuleSet = text.parse().map_err(|e| format!("{e}"))?;
        Ok((&set).into_iter().map(|d| secs(&d, k)).take_while(|s| *s <= b.until).take(b.max).collect())
    };
    let parse_dt = |line: &str, k: Kind| {
        let set: rrule_jiff::RRuleSet = format!("{line}\nRRULE:FREQ=DAILY;COUNT=1").parse().map_err(|e| format!("{e}"))?;
        Ok(secs(set.get_dt_start(), k))
    };
    rrule_family(ics, b, expand, parse_dt)
}

// ---------- calcard family: whole-calendar expansion incl. VTIMEZONE and overrides ----------

/// Formats one instance from its own zone: a floating result for a TZID input shows up
/// as a mismatch (silent zone substitution), which is what we want to detect.
fn fmt_result(master_kind: Kind, floating: bool, naive: i64, instant: i64) -> String {
    match (master_kind, floating) {
        (Kind::Date, _) => fmt(Kind::Date, naive),
        (_, true) => fmt(Kind::Floating, naive),
        (_, false) => fmt(Kind::Instant, instant),
    }
}

fn master_kind(ics: &str) -> Kind {
    events(ics)
        .iter()
        .find(|e| !e.iter().any(|p| p.name == "RECURRENCE-ID"))
        .and_then(|e| e.iter().find(|p| p.name == "DTSTART").map(kind_of))
        .unwrap_or(Kind::Instant)
}

fn run_calcard(ics: &str, b: &Bounds) -> Out {
    use calcard::common::timezone::Tz;
    let cal = calcard::icalendar::ICalendar::parse(ics).map_err(|e| format!("parse: {e:?}"))?;
    let exp = cal.expand_dates(Tz::Floating, b.max);
    if !exp.errors.is_empty() {
        return Err(format!("{:?}", exp.errors));
    }
    let k = master_kind(ics);
    let mut v: Vec<String> = exp
        .events
        .iter()
        .filter(|e| e.start.timestamp() <= b.until + 86_400)
        .map(|e| {
            let floating = matches!(e.start.timezone(), Tz::Floating);
            fmt_result(k, floating, e.start.naive_local().and_utc().timestamp(), e.start.timestamp())
        })
        .collect();
    v.sort();
    v.dedup();
    Ok(v)
}

fn run_calcard_next(ics: &str, b: &Bounds) -> Out {
    use calcard_next::common::timezone::Tz;
    let cal = calcard_next::icalendar::ICalendar::parse(ics).map_err(|e| format!("parse: {e:?}"))?;
    let ts = |s: i64| jiff::Timestamp::from_second(s.clamp(jiff::Timestamp::MIN.as_second(), jiff::Timestamp::MAX.as_second())).unwrap();
    let exp = cal.expand_dates_between(Tz::Floating, ts(b.from), ts(b.until), b.max);
    if !exp.errors.is_empty() {
        return Err(format!("{:?}", exp.errors));
    }
    let k = master_kind(ics);
    let mut v: Vec<String> = exp
        .events
        .iter()
        .map(|e| {
            let floating = e.start.time_zone().is_unknown();
            let naive = e.start.datetime().to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp().as_second();
            fmt_result(k, floating, naive, e.start.timestamp().as_second())
        })
        .collect();
    v.sort();
    v.dedup();
    Ok(v)
}

fn candidate(name: &str) -> fn(&str, &Bounds) -> Out {
    match name {
        "rrule" => run_rrule,
        "rrule2" => run_rrule2,
        "rrule-jiff" => run_rrule_jiff,
        "calcard" => run_calcard,
        "calcard-next" => run_calcard_next,
        "parse-only" => |ics, _| Ok(vec![format!("{}", events(ics).len())]),
        other => panic!("unknown candidate {other}"),
    }
}

/// Runs `f` on a thread with a deadline; panics and hangs become errors.
fn guarded(f: fn(&str, &Bounds) -> Out, ics: String, b: Bounds) -> Out {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let r = std::panic::catch_unwind(|| f(&ics, &b))
            .unwrap_or_else(|p| Err(format!("panic: {}", p.downcast_ref::<String>().cloned().or(p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())));
        let _ = tx.send(r);
    });
    // shortcut: a timed-out thread keeps spinning until the process exits; fine for a CLI run.
    rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| Err("timeout after 10 s".into()))
}

fn version(name: &str) -> &'static str {
    match name {
        "rrule" => "rrule 0.14.0 (crates.io, chrono-tz)",
        "rrule2" => "rrule2 0.15.0 (crates.io, chrono-tz)",
        "rrule-jiff" => "rrule jiff port, ccope/rust-rrule@50db3e2 (closed PR #151)",
        "calcard" => "calcard 0.3.14 (crates.io, chrono-tz), ICalendar::expand_dates",
        "calcard-next" => "calcard-next 3.0.1 (crates.io, jiff), ICalendar::expand_dates_between",
        _ => "",
    }
}

fn run(name: &str, cases_path: &str) {
    let f = candidate(name);
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(cases_path).unwrap()).unwrap();
    let mut results = Map::new();
    for c in &cases {
        let window_from = c["window"].as_array().map(|w| utc_secs(w[0].as_str().unwrap()) - 2 * 86_400);
        let b = Bounds {
            from: window_from.unwrap_or(jiff::Timestamp::MIN.as_second()),
            until: utc_secs(c["iter_until"].as_str().unwrap()),
            max: c["iter_max"].as_u64().unwrap() as usize,
        };
        let ics = format!("BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:x\n{}END:VCALENDAR\n", c["ics"].as_str().unwrap());
        let r = match guarded(f, ics, b) {
            Ok(v) => json!({ "ok": v }),
            Err(e) => json!({ "error": e }),
        };
        results.insert(c["id"].as_str().unwrap().to_string(), r);
    }
    println!("{}", json!({ "engine": version(name), "results": results }));
}

// ---------- benchmark: N mixed series, expand a window of `years` from 2026-01-01 ----------

fn synthetic(i: usize) -> String {
    const RULES: [&str; 8] = [
        "FREQ=DAILY",
        "FREQ=WEEKLY;BYDAY=MO,WE,FR",
        "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU",
        "FREQ=MONTHLY;BYMONTHDAY=15",
        "FREQ=MONTHLY;BYDAY=-1FR",
        "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1",
        "FREQ=YEARLY;BYMONTH=3;BYDAY=2SU",
        "FREQ=DAILY;COUNT=400",
    ];
    const TZ: [&str; 4] = ["Europe/Prague", "America/New_York", "Australia/Lord_Howe", "Asia/Tokyo"];
    let (y, m, d, h) = (2020 + i % 7, 1 + i % 12, 1 + i % 28, 6 + i % 12);
    let exdate = if i % 5 == 0 { format!("EXDATE;TZID={}:{y}{m:02}{:02}T{h:02}0000\n", TZ[i % 4], d) } else { String::new() };
    format!(
        "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:bench\nBEGIN:VEVENT\nUID:b{i}\nDTSTAMP:20260101T000000Z\nDTSTART;TZID={}:{y}{m:02}{d:02}T{h:02}0000\nRRULE:{}\n{exdate}END:VEVENT\nEND:VCALENDAR\n",
        TZ[i % 4],
        RULES[i % RULES.len()]
    )
}

fn bench(name: &str, n: usize, years: i64) {
    let f = candidate(name);
    let from = utc_secs("20260101T000000Z");
    // max caps instances counted from DTSTART (calcard 0.3.14 has no window, so it needs this cap)
    let b = Bounds { from, until: from + years * 365 * 86_400, max: 5_000 };
    let (lo, hi) = (fmt(Kind::Instant, from), fmt(Kind::Instant, b.until));
    let inputs: Vec<String> = (0..n).map(synthetic).collect();
    let t = std::time::Instant::now();
    let (mut total, mut errors) = (0usize, 0usize);
    for ics in &inputs {
        match f(ics, &b) {
            Ok(v) => total += v.iter().filter(|s| **s >= lo && **s < hi).count(),
            Err(_) => errors += 1,
        }
    }
    let ms = t.elapsed().as_millis();
    println!("{}", json!({ "candidate": name, "series": n, "years": years, "instances_in_window": total, "errors": errors, "ms": ms }));
}

// ---------- timezones: chrono-tz (compiled-in tzdb) vs jiff (system tzdb), plus a custom VTIMEZONE via POSIX TZ ----------

fn tzcheck() {
    use chrono::{Offset, TimeZone};
    let jdb = jiff::tz::db();
    let (mut zones, mut missing, mut differing) = (0, vec![], vec![]);
    for tz in chrono_tz::TZ_VARIANTS {
        zones += 1;
        let Ok(jz) = jdb.get(tz.name()) else { missing.push(tz.name()); continue };
        // every 12 h from 1970 to 2038: catches any rule or history difference that lasts >= 12 h
        let first = (0..(68 * 365 * 2)).map(|i| i as i64 * 43_200).find(|&t| {
            let c = tz.offset_from_utc_datetime(&chrono::DateTime::from_timestamp(t, 0).unwrap().naive_utc()).fix().local_minus_utc();
            c != jz.to_offset(jiff::Timestamp::from_second(t).unwrap()).seconds()
        });
        if let Some(t) = first {
            differing.push(format!("{} (first at {})", tz.name(), fmt(Kind::Instant, t)));
        }
    }
    // custom VTIMEZONE from the corpus (STANDARD 1SU Nov 03:00 +0230->+0130, DAYLIGHT 2SU Mar 02:00 +0130->+0230)
    let custom = jiff::tz::TimeZone::posix("<+0130>-1:30<+0230>,M3.2.0/2,M11.1.0/3").unwrap();
    let dt = |s: &str| s.parse::<jiff::civil::DateTime>().unwrap().to_zoned(custom.clone()).unwrap().timestamp().to_string();
    println!("{}", json!({
        "chrono_tz_iana_version": chrono_tz::IANA_TZDB_VERSION,
        "jiff_db": format!("{jdb:?}").chars().take(120).collect::<String>(),
        "zones_compared": zones, "missing_in_jiff": missing, "differing": differing,
        "custom_vtimezone_posix": [dt("2026-03-06T09:00"), dt("2026-03-13T09:00"), dt("2026-03-20T09:00")],
    }));
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("run") => run(&a[2], &a[3]),
        Some("tzcheck") => tzcheck(),
        Some("bench") => bench(&a[2], a[3].parse().unwrap(), a[4].parse().unwrap()),
        _ => eprintln!("usage: run <candidate> <cases.json> | bench <candidate> <series> <years>"),
    }
}
