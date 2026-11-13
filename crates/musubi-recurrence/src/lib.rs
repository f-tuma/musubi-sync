//! Recurrence expansion over `jiff`: [`expand`]`(series, window, limit)`.
//!
//! The RFC 5545 rule arithmetic is the vendored `rrule` 0.14 core (`src/vendor/rrule`),
//! which only ever sees civil time marked UTC. Everything that depends on a zone lives in
//! this edge (docs/research/phase0/recurrence.md, verdict and fixes 1-12):
//!
//! - **Keys.** Every occurrence is identified by a [`Timestamp`]. For a zoned series it is the
//!   instant; for a floating or all-day series it is the local wall time read as UTC, the
//!   convention of `musubi_model::OccurrenceRow`. [`recurrence_id`] computes the same key for
//!   a correlation id, so expansion and joins always agree.
//! - **Gap and fold** (RFC 5545 3.3.5): a local time in a gap is shifted forward by the gap
//!   length, a local time in a fold takes the first (earlier) instant.
//! - **Floating never meets the host zone**: no code path reads the system time zone.
//! - **DTSTART is always the first instance**; with `COUNT` it counts towards the total even
//!   when the rule itself would not produce it. A series with only RDATEs never calls the core.
//! - **UNTIL** is compared as an exact key, so an `UNTIL` inside a fold hour is honoured.
//!   A DATE `UNTIL` on a timed series means the end of that local day; a local `UNTIL` on a
//!   zoned series (invalid per RFC) is read in the series zone.
//! - **EXDATE with a DATE value** on a timed series removes every occurrence on that local day.
//! - **Bounded by type**: there is no unbounded iterator; a window is mandatory, more than
//!   `limit` occurrences in it is an error, and generation has a fixed budget.
//!
//! Time zones are IANA zones from jiff's bundled tzdb; [`resolve_tzid`] maps a TZID (IANA name,
//! alias or prefixed path) to one and fails loudly otherwise. Matching a custom `VTIMEZONE`
//! body or a Windows zone name is out of scope for phase 1 (fix 8 covers IANA zones only).
//!
//! **Expansion cost** (fix 12, skip to the window start, is deferred): the core walks from
//! DTSTART to the window. [`RecurrenceError::TooExpensive`] means the series cannot be expanded
//! in this window, not that it is empty; callers must surface it. Known cases: a `MINUTELY` or
//! `SECONDLY` rule from a DTSTART years before the window (more than [`MAX_GENERATED`]
//! instants), and a rule that matches nothing for 100 000 periods in a row (the core's
//! `MAX_ITER_LOOP`), e.g. `FREQ=DAILY;BYMONTH=2;BYMONTHDAY=30` with a window about 275 years
//! after DTSTART.
//!
//! **Overrides** are explicit instances: an override is emitted even when its RECURRENCE-ID is
//! excluded by EXDATE or matches no generated occurrence.
//!
//! **Gap merging**: occurrences of a sub-daily rule that fall into a spring-forward gap move
//! forward by the gap length (RFC 5545 3.3.5, the reading chosen in recurrence.md) and can land
//! on the next real occurrence. COUNT counts generated instances and the recurrence set holds
//! each instant once, so `FREQ=HOURLY;COUNT=4` across the gap gives three. This is intended.
//!
//! The vendored core is private; the only way in is [`expand`]:
//!
//! ```compile_fail,E0603
//! use musubi_recurrence::vendor;
//! ```

mod vendor;

use std::collections::BTreeMap;
use std::ops::Range;

use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, ToSpan};

use vendor::rrule::{RRule, RRuleIter};

/// Most civil instants the core may generate in one [`expand`] call (all rules together).
/// Guards against rules such as `FREQ=SECONDLY` from a DTSTART decades before the window.
pub const MAX_GENERATED: usize = 1_000_000;

/// A recurring event, as far as expansion needs it.
#[derive(Debug, Clone)]
pub struct Series {
    /// DTSTART as local wall time in `zone` (whole seconds).
    pub start: DateTime,
    /// Zone of `start`; `None` for floating and all-day series.
    pub zone: Option<TimeZone>,
    /// Nominal length of each occurrence (`P1D` is one calendar day, even across DST).
    pub duration: Span,
    /// RRULE values, e.g. `FREQ=WEEKLY;BYDAY=MO;UNTIL=20270101T000000Z`.
    pub rules: Vec<String>,
    /// EXRULE values (deprecated in RFC 5545, still sent by some providers).
    pub exrules: Vec<String>,
    /// RDATEs. A PERIOD value is passed as its start.
    pub rdates: Vec<Moment>,
    /// EXDATEs.
    pub exdates: Vec<Moment>,
    /// Modified occurrences (RECURRENCE-ID overrides).
    pub overrides: Vec<Override>,
}

/// One DATE or DATE-TIME value of a series property (RDATE, EXDATE, RECURRENCE-ID, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// Local wall time in the series zone (floating for a floating series).
    Local(DateTime),
    /// An exact instant (`Z` or another zone). Only valid for a zoned series.
    Instant(Timestamp),
    /// A DATE value: local midnight; as an EXDATE it removes every occurrence on that day.
    Day(Date),
}

/// A modified occurrence (an override instance with RECURRENCE-ID).
#[derive(Debug, Clone)]
pub struct Override {
    /// The original start of the occurrence it replaces.
    pub recurrence_id: Moment,
    /// Its new start.
    pub start: Moment,
    /// Its new length; `None` keeps the series duration.
    pub duration: Option<Span>,
    /// `RANGE=THISANDFUTURE`: the change of local start time and the duration apply to this
    /// and every later occurrence.
    pub this_and_future: bool,
}

/// One occurrence of a series. Keys follow the convention described in the crate docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instance {
    /// Start.
    pub start: Timestamp,
    /// End (`>= start` for a non-negative duration).
    pub end: Timestamp,
    /// Original start, the correlation key with override items.
    pub recurrence_id: Timestamp,
}

/// Why a series could not be expanded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecurrenceError {
    /// An RRULE or EXRULE value that the core rejects.
    #[error("invalid rule `{rule}`: {reason}")]
    InvalidRule {
        /// The rule value as given.
        rule: String,
        /// The parser's or validator's message.
        reason: String,
    },
    /// An `UNTIL` that is not `YYYYMMDD`, `YYYYMMDDTHHMMSS` or `YYYYMMDDTHHMMSSZ`.
    #[error("invalid UNTIL `{0}`")]
    InvalidUntil(String),
    /// DTSTART with a fractional second; RFC 5545 times are whole seconds.
    #[error("DTSTART {0} has a fractional second")]
    FractionalStart(DateTime),
    /// An exact instant was given for a floating series, where it has no local meaning.
    #[error("instant {0} given for a floating series")]
    InstantInFloatingSeries(Timestamp),
    /// More occurrences overlap the window than the caller's limit.
    #[error("more than {limit} occurrences in the window")]
    TooManyOccurrences {
        /// The limit that was exceeded.
        limit: usize,
    },
    /// Expansion would generate more than [`MAX_GENERATED`] instants, or the rule produced
    /// nothing for so long that the core gave up before reaching the window.
    #[error("expansion is too expensive for this window")]
    TooExpensive,
    /// A date or time outside the supported range.
    #[error("time out of range: {0}")]
    OutOfRange(String),
    /// A TZID that names no IANA zone (see [`resolve_tzid`]).
    #[error("unknown time zone `{0}`")]
    UnknownTimeZone(String),
}

/// The IANA zone of a TZID, from jiff's bundled tzdb.
///
/// Accepts an IANA name or one of its tzdb aliases (`US/Eastern`, `Europe/Kiev`), then the
/// longest `Area/Location` suffix after a path prefix, as Mozilla and other clients write
/// (`/mozilla.org/20050126_1/Europe/Prague`). Anything else is an error, never UTC or floating.
/// Matching a custom `VTIMEZONE` body to a zone is not done here (it belongs with the iCalendar
/// parser); a TZID such as a Windows name (`Central Europe Standard Time`) is an error.
pub fn resolve_tzid(tzid: &str) -> Result<TimeZone, RecurrenceError> {
    let db = jiff::tz::db();
    if let Ok(tz) = db.get(tzid) {
        return Ok(tz);
    }
    // Suffixes keep at least one `/`, so a prefix never resolves to a bare `UTC` or `EST`.
    let mut rest = tzid;
    while let Some((_, tail)) = rest.split_once('/') {
        if !tail.contains('/') {
            break;
        }
        if let Ok(tz) = db.get(tail) {
            return Ok(tz);
        }
        rest = tail;
    }
    Err(RecurrenceError::UnknownTimeZone(tzid.to_owned()))
}

/// The key of an occurrence that starts at `local` in `zone` (`None`: floating or all-day).
///
/// Same convention as [`Instance::recurrence_id`]: the instant for a zoned time (gap and fold
/// resolved per RFC 5545 3.3.5), the wall time read as UTC for a floating one.
pub fn recurrence_id(
    local: DateTime,
    zone: Option<&TimeZone>,
) -> Result<Timestamp, RecurrenceError> {
    local
        .to_zoned(zone.cloned().unwrap_or(TimeZone::UTC))
        .map(|z| z.timestamp())
        .map_err(out_of_range)
}

/// Expands `series` to the occurrences that overlap `window` (same overlap rule as
/// `musubi_model::TimeWindow`: `start < window.end` and `end > window.start`, a zero-length
/// occurrence counts when it starts inside the window). The result is sorted by start.
/// The window is half-open, so an empty or reversed one contains no instant and gives an
/// empty result, even when an occurrence spans it.
///
/// For a floating or all-day series the window is read in the same convention as the keys
/// (wall time as UTC). More than `limit` occurrences is an error, never a truncated result.
///
/// There is no unbounded form; leaving out the window does not compile:
///
/// ```compile_fail,E0061
/// fn all(s: &musubi_recurrence::Series) {
///     let _ = musubi_recurrence::expand(s, 1000);
/// }
/// ```
pub fn expand(
    series: &Series,
    window: Range<Timestamp>,
    limit: usize,
) -> Result<Vec<Instance>, RecurrenceError> {
    if window.start >= window.end {
        return Ok(Vec::new());
    }
    if series.start.subsec_nanosecond() != 0 {
        return Err(RecurrenceError::FractionalStart(series.start));
    }
    let edge = Edge { series };
    let overrides = edge.overrides()?;

    // Civil bound of generation: an occurrence whose key is before `window.end` starts no
    // later than the window end's local time plus the largest zone transition (< 2 days).
    // An override may move an occurrence from later into the window.
    let mut horizon = edge.civil(window.end).saturating_add(2.days());
    for o in &overrides {
        horizon = horizon.max(o.rid_civil.saturating_add(1.day()));
        if o.this_and_future {
            horizon = horizon.max(horizon.saturating_sub(o.shift));
        }
    }

    // Base occurrences: rid key -> local start. DTSTART is always the first instance.
    let mut budget = MAX_GENERATED;
    let mut occ: BTreeMap<Timestamp, DateTime> = BTreeMap::new();
    occ.insert(edge.key(series.start)?, series.start);
    for rule in &series.rules {
        for local in edge.rule(rule, horizon, true, &mut budget)? {
            occ.insert(edge.key(local)?, local);
        }
    }
    for rdate in &series.rdates {
        let (key, local) = edge.moment(*rdate)?;
        occ.insert(key, local);
    }

    // Exclusions.
    for rule in &series.exrules {
        for local in edge.rule(rule, horizon, false, &mut budget)? {
            occ.remove(&edge.key(local)?);
        }
    }
    for exdate in &series.exdates {
        match *exdate {
            Moment::Day(day) => occ.retain(|_, local| local.date() != day),
            other => {
                occ.remove(&edge.moment(other)?.0);
            }
        }
    }

    // rid -> (start, duration).
    let mut instances: BTreeMap<Timestamp, (Timestamp, Span)> = BTreeMap::new();
    for &rid in occ.keys() {
        instances.insert(rid, (rid, series.duration));
    }
    // THISANDFUTURE in rid order, so a later split overrides an earlier one for the
    // occurrences after it whatever order the overrides came in.
    let mut this_and_future: Vec<&Resolved> =
        overrides.iter().filter(|o| o.this_and_future).collect();
    this_and_future.sort_by_key(|o| o.rid);
    for o in this_and_future {
        for (&rid, &local) in occ.range(o.rid..) {
            let start = edge.key(local.checked_add(o.shift).map_err(out_of_range)?)?;
            instances.insert(rid, (start, o.duration.unwrap_or(series.duration)));
        }
        instances.insert(o.rid, (o.start, o.duration.unwrap_or(series.duration)));
    }
    // An override is an explicit instance (RFC 5545 3.8.4.4): it is emitted even when its
    // RECURRENCE-ID is excluded by EXDATE/EXRULE or matches no generated occurrence, so data
    // the server sent is never hidden.
    for o in overrides.iter().filter(|o| !o.this_and_future) {
        instances.insert(o.rid, (o.start, o.duration.unwrap_or(series.duration)));
    }

    let mut out = Vec::new();
    for (rid, (start, duration)) in instances {
        if start >= window.end {
            continue;
        }
        let end = edge.end(start, duration)?;
        let overlaps = end > window.start || (start == end && start >= window.start);
        if overlaps {
            if out.len() == limit {
                return Err(RecurrenceError::TooManyOccurrences { limit });
            }
            out.push(Instance {
                start,
                end,
                recurrence_id: rid,
            });
        }
    }
    out.sort();
    Ok(out)
}

/// An override with its moments resolved to keys and local times.
struct Resolved {
    rid: Timestamp,
    rid_civil: DateTime,
    start: Timestamp,
    /// Local start change (new - original), applied to later occurrences for THISANDFUTURE.
    shift: Span,
    duration: Option<Span>,
    this_and_future: bool,
}

/// Zone-dependent conversions for one series.
struct Edge<'a> {
    series: &'a Series,
}

impl Edge<'_> {
    fn zone(&self) -> TimeZone {
        self.series.zone.clone().unwrap_or(TimeZone::UTC)
    }

    /// Key of a local time (see [`recurrence_id`]).
    fn key(&self, local: DateTime) -> Result<Timestamp, RecurrenceError> {
        recurrence_id(local, self.series.zone.as_ref())
    }

    /// Local time of a key (inverse of `key` outside gaps and folds).
    fn civil(&self, key: Timestamp) -> DateTime {
        key.to_zoned(self.zone()).datetime()
    }

    /// Key and local time of a moment.
    fn moment(&self, moment: Moment) -> Result<(Timestamp, DateTime), RecurrenceError> {
        match moment {
            Moment::Local(local) => Ok((self.key(local)?, local)),
            Moment::Day(day) => {
                let local = day.to_datetime(Time::midnight());
                Ok((self.key(local)?, local))
            }
            Moment::Instant(t) if self.series.zone.is_some() => Ok((t, self.civil(t))),
            Moment::Instant(t) => Err(RecurrenceError::InstantInFloatingSeries(t)),
        }
    }

    /// End of an occurrence starting at key `start`, with nominal `duration` in the series zone.
    fn end(&self, start: Timestamp, duration: Span) -> Result<Timestamp, RecurrenceError> {
        start
            .to_zoned(self.zone())
            .checked_add(duration)
            .map(|z| z.timestamp())
            .map_err(out_of_range)
    }

    fn overrides(&self) -> Result<Vec<Resolved>, RecurrenceError> {
        let mut out = Vec::new();
        for o in &self.series.overrides {
            let (rid, rid_civil) = self.moment(o.recurrence_id)?;
            let (start, start_civil) = self.moment(o.start)?;
            let shift = rid_civil.until(start_civil).map_err(out_of_range)?;
            out.push(Resolved {
                rid,
                rid_civil,
                start,
                shift,
                duration: o.duration,
                this_and_future: o.this_and_future,
            });
        }
        Ok(out)
    }

    /// Local times generated by one RRULE/EXRULE up to `horizon`, already limited by its
    /// `UNTIL`. With `count_dtstart`, DTSTART counts towards `COUNT` even if the rule does not
    /// produce it (fix 4); the caller adds DTSTART itself.
    fn rule(
        &self,
        rule: &str,
        horizon: DateTime,
        count_dtstart: bool,
        budget: &mut usize,
    ) -> Result<Vec<DateTime>, RecurrenceError> {
        let invalid = |reason: String| RecurrenceError::InvalidRule {
            rule: rule.to_owned(),
            reason,
        };
        let (parsed, until) =
            vendor::rrule::parse_rule(rule).map_err(|e| invalid(e.to_string()))?;
        let until = until.map(|u| self.until(&u)).transpose()?;
        let start = to_chrono(self.series.start)?;
        let parsed: RRule = parsed.validate(start).map_err(|e| invalid(e.to_string()))?;
        let count = parsed.get_count();

        // The core is cut off at the horizon and, for UNTIL, one day past its local time
        // (gap/fold slack); the exact cut is by key below.
        let mut stop = horizon;
        if let Some(until) = until {
            stop = stop.min(self.civil(until).saturating_add(1.day()));
        }
        let mut iter: RRuleIter = parsed.iter_with_ctx(start, true);
        iter.stop_after = Some(to_chrono(stop)?.date_naive());

        let mut out = Vec::new();
        let mut take = count.map_or(usize::MAX, |c| c as usize);
        let mut first = true;
        while take > 0 {
            let Some(next) = iter.next() else { break };
            let local = from_chrono(next)?;
            if first && count_dtstart && local != self.series.start {
                take -= 1; // DTSTART is the first of COUNT
                if take == 0 {
                    break;
                }
            }
            first = false;
            if local > stop {
                break;
            }
            *budget = budget.checked_sub(1).ok_or(RecurrenceError::TooExpensive)?;
            if until.is_none_or(|u| self.key(local).is_ok_and(|k| k <= u)) {
                out.push(local);
            }
            take -= 1;
        }
        if iter.was_limited {
            return Err(RecurrenceError::TooExpensive);
        }
        Ok(out)
    }

    /// The last allowed key for an `UNTIL` value (inclusive).
    fn until(&self, value: &str) -> Result<Timestamp, RecurrenceError> {
        let bad = || RecurrenceError::InvalidUntil(value.to_owned());
        if value.len() == 8 {
            // DATE: through the end of that local day.
            let day = Date::strptime("%Y%m%d", value).map_err(|_| bad())?;
            let next = day
                .tomorrow()
                .map_err(|_| bad())?
                .to_datetime(Time::midnight());
            return Ok(self.key(next)? - 1.nanosecond());
        }
        let (text, utc) = match value.strip_suffix('Z') {
            Some(text) => (text, true),
            None => (value, false),
        };
        let local = DateTime::strptime("%Y%m%dT%H%M%S", text).map_err(|_| bad())?;
        match (utc, &self.series.zone) {
            (true, Some(_)) => local
                .to_zoned(TimeZone::UTC)
                .map(|z| z.timestamp())
                .map_err(out_of_range),
            // A local UNTIL, or any UNTIL of a floating series: wall time in the series zone.
            _ => self.key(local),
        }
    }
}

fn out_of_range(e: jiff::Error) -> RecurrenceError {
    RecurrenceError::OutOfRange(e.to_string())
}

fn to_chrono(dt: DateTime) -> Result<chrono::DateTime<chrono::Utc>, RecurrenceError> {
    chrono::NaiveDate::from_ymd_opt(dt.year().into(), dt.month() as u32, dt.day() as u32)
        .and_then(|d| d.and_hms_opt(dt.hour() as u32, dt.minute() as u32, dt.second() as u32))
        .map(|n| n.and_utc())
        .ok_or_else(|| RecurrenceError::OutOfRange(dt.to_string()))
}

fn from_chrono(dt: chrono::DateTime<chrono::Utc>) -> Result<DateTime, RecurrenceError> {
    use chrono::{Datelike, Timelike};
    let year = i16::try_from(dt.year()).map_err(|_| RecurrenceError::OutOfRange(dt.to_string()))?;
    // Values come from chrono, so month/day/hour/minute/second are in range.
    DateTime::new(
        year,
        dt.month() as i8,
        dt.day() as i8,
        dt.hour() as i8,
        dt.minute() as i8,
        dt.second() as i8,
        0,
    )
    .map_err(out_of_range)
}
