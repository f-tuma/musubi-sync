//! Occurrences of a task in the index (docs/plan/phase2.md 12.2, 12.3): its
//! extent from `start`, `due` and `estimated_duration`, its done state, and
//! the series of a recurring task. Expansion is the events' ([`ingest`]).

use std::collections::HashMap;

use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, ToSpan, Unit};
use musubi_model::RemoteKey;
use musubi_model::error::BoxError;
use musubi_model::event::PatchObject;
use musubi_model::store::{OccurrenceRow, Occurrences};
use musubi_model::task::{CalendarTask, TaskProgress};
use musubi_recurrence::{RecurrenceError, recurrence_id};
use serde_json::Value;

use crate::ingest::{
    self, Index, expand_rows, index_series, rule_values, series, single, text, unresolved,
};

/// Index rows of one task. A cancelled task and a task without any date have
/// none; a task in a zone that cannot be resolved is reported as unindexed
/// (never floating), a series is indexed like an event's.
pub(crate) fn task_occurrences(
    key: &RemoteKey,
    t: &CalendarTask,
    index: Index,
    previous: Option<Timestamp>,
) -> Occurrences {
    if t.progress == TaskProgress::Cancelled {
        return Occurrences::default();
    }
    let zone = task_zone(t);
    let known = zone.as_ref().ok().and_then(Option::as_ref);
    let (anchor, span) = match master_extent(t, known) {
        Ok(Some(e)) => e,
        Ok(None) => return Occurrences::default(),
        Err(e) => {
            tracing::warn!(key = %key.0, error = %e, "task time out of range; not indexed");
            return Occurrences::default();
        }
    };
    let recurring = !(t.recurrence_rules.is_empty() && t.recurrence_overrides.is_empty());
    let zone = match zone {
        Ok(z) => z,
        Err(e) => {
            tracing::warn!(key = %key.0, error = %e, "zone not resolvable; reported as unindexed");
            return unresolved(anchor, span, recurring);
        }
    };
    if !recurring {
        let mut o = single(key, anchor, span, zone.as_ref(), None);
        o.rows.iter_mut().for_each(|r| r.done = done(t.progress));
        return o;
    }
    index_series(key, anchor, zone.as_ref(), index, previous, |until| {
        expand_task(
            t,
            anchor,
            span,
            zone.clone(),
            Timestamp::MIN,
            until,
            index.limit,
        )
    })
}

/// Rows extending a stored task master's index from `from` to `until`.
pub(crate) fn extension(
    canonical: &str,
    from: Timestamp,
    until: Timestamp,
    limit: usize,
) -> Result<Vec<OccurrenceRow>, BoxError> {
    let t: CalendarTask = serde_json::from_str(canonical)?;
    // An unresolved zone keeps the item in `unindexed`, never floating rows.
    let zone = task_zone(&t)?;
    let (anchor, span) =
        master_extent(&t, zone.as_ref())?.ok_or("task without a date has no series")?;
    Ok(expand_task(&t, anchor, span, zone, from, until, limit)?)
}

/// The task's zone, resolved as an event's ([`ingest::zone`]).
fn task_zone(t: &CalendarTask) -> Result<Option<TimeZone>, BoxError> {
    ingest::zone(t.time_zone.as_deref(), &t.time_zones)
}

/// Completed and failed occurrences are done (plan 12.3).
fn done(p: TaskProgress) -> bool {
    matches!(p, TaskProgress::Completed | TaskProgress::Failed)
}

/// The master's anchor and span, see [`extent`].
fn master_extent(
    t: &CalendarTask,
    zone: Option<&TimeZone>,
) -> Result<Option<(DateTime, Span)>, jiff::Error> {
    extent(
        &t.uid,
        zone,
        t.start,
        t.due,
        t.estimated_duration.map(|d| d.0),
        t.show_without_time,
    )
}

/// Where one occurrence starts (the anchor, `start ?? due`) and how long it
/// lasts (plan 12.2); `None` without any date. A date-only due covers its
/// whole day (nominal days); a timed end in a zone is exact hours, as for
/// events, so a DST change in between does not move it. An end before the
/// start (due before start, or a negative estimate) is clamped to the start
/// with a warning.
fn extent(
    uid: &str,
    zone: Option<&TimeZone>,
    start: Option<DateTime>,
    due: Option<DateTime>,
    estimated: Option<Span>,
    whole_day: bool,
) -> Result<Option<(DateTime, Span)>, jiff::Error> {
    let day = |t: DateTime| {
        if whole_day {
            t.checked_add(1.day())
        } else {
            Ok(t)
        }
    };
    let (anchor, end) = match (start, due, estimated) {
        (None, None, _) => return Ok(None),
        (None, Some(due), _) => (due, day(due)?),
        (Some(start), Some(due), _) => (start, day(due)?),
        (Some(start), None, Some(d)) => (start, start.checked_add(d)?),
        (Some(start), None, None) => (start, day(start)?),
    };
    let end = if end < anchor {
        tracing::warn!(%uid, %anchor, %end, "task ends before it starts; ends at its start");
        anchor
    } else {
        end
    };
    let span = match zone {
        Some(z) if !whole_day => anchor
            .to_zoned(z.clone())?
            .until((Unit::Hour, &end.to_zoned(z.clone())?))?,
        _ => anchor.until(end)?,
    };
    Ok(Some((anchor, span)))
}

/// Rows of a task series starting in `[from, until)`. An override moves
/// through the anchor member (`start`, else `due`) and changes its length
/// through the other members; its `progress` decides its done state, other
/// occurrences take the master's.
fn expand_task(
    t: &CalendarTask,
    anchor: DateTime,
    span: Span,
    zone: Option<TimeZone>,
    from: Timestamp,
    until: Timestamp,
    limit: usize,
) -> Result<Vec<OccurrenceRow>, RecurrenceError> {
    let mut done_at = HashMap::new();
    for (k, p) in &t.recurrence_overrides {
        let Some(v) = p.get("progress") else { continue };
        match progress(v) {
            Some(progress) => {
                done_at.insert(recurrence_id(*k, zone.as_ref())?, done(progress));
            }
            None => {
                tracing::warn!(uid = %t.uid, %k, %v, "unknown override progress; takes the master's");
            }
        }
    }
    let s = series(
        anchor,
        zone.clone(),
        span,
        rule_values(&t.recurrence_rules),
        Vec::new(),
        &t.recurrence_overrides,
        |rid, p| override_extent(t, zone.as_ref(), rid, p),
    );
    let master = done(t.progress);
    let mut rows = expand_rows(&s, from, until, limit)?;
    for r in &mut rows {
        let own = r.recurrence_id.and_then(|k| done_at.get(&k));
        r.done = own.copied().unwrap_or(master);
    }
    Ok(rows)
}

/// A patched `progress`; `null` removes it, leaving the default `needs-action`.
fn progress(v: &Value) -> Option<TaskProgress> {
    match v {
        Value::Null => Some(TaskProgress::default()),
        v => serde_json::from_value(v.clone()).ok(),
    }
}

/// Start and length of the override at `rid` with patch `p`. The length is
/// the series' unless the patch sets `due`, `estimatedDuration` or
/// `showWithoutTime`; members it leaves out are the master's, its due moved
/// with the start. A due-anchored series ignores a `start` the patch adds:
/// the occurrence stays keyed and anchored at its due.
fn override_extent(
    t: &CalendarTask,
    zone: Option<&TimeZone>,
    rid: DateTime,
    p: &PatchObject,
) -> (DateTime, Option<Span>) {
    let at = |name| text(p, name).and_then(|v| v.parse::<DateTime>().ok());
    let anchored_at_due = t.start.is_none();
    let start = at(if anchored_at_due { "due" } else { "start" }).unwrap_or(rid);
    let touched = ["due", "estimatedDuration", "showWithoutTime"]
        .iter()
        .any(|m| p.contains_key(*m));
    if !touched {
        return (start, None);
    }
    // A member set to `null` is removed.
    let member = |name: &str| p.get(name).map(|v| !v.is_null());
    let due = if anchored_at_due {
        Some(start)
    } else {
        match member("due") {
            Some(true) => at("due"),
            Some(false) => None,
            None => t
                .start
                .zip(t.due)
                .and_then(|(s, d)| start.checked_add(s.until(d).ok()?).ok()),
        }
    };
    let estimated = match member("estimatedDuration") {
        Some(true) => text(p, "estimatedDuration").and_then(|v| v.parse::<Span>().ok()),
        Some(false) => None,
        None => t.estimated_duration.map(|d| d.0),
    };
    let whole_day = p
        .get("showWithoutTime")
        .and_then(Value::as_bool)
        .unwrap_or(t.show_without_time);
    let starts = (!anchored_at_due).then_some(start);
    // Out of range: the occurrence keeps the series length; expansion reports the range.
    let span = match extent(&t.uid, zone, starts, due, estimated, whole_day) {
        Ok(e) => e.map(|(_, s)| s),
        Err(e) => {
            tracing::warn!(uid = %t.uid, %rid, error = %e, "override length out of range; keeps the series'");
            None
        }
    };
    (start, span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use musubi_model::store::SeriesSpan;
    use serde_json::json;

    fn task(v: Value) -> CalendarTask {
        let mut v = v;
        v["uid"] = json!("t");
        serde_json::from_value(v).unwrap()
    }

    const INDEX: Index = Index {
        horizon: Timestamp::constant(1_900_000_000, 0),
        fallback: Timestamp::constant(1_800_000_000, 0),
        limit: 50_000,
    };

    fn occ(v: Value) -> Occurrences {
        task_occurrences(&RemoteKey("k".into()), &task(v), INDEX, None)
    }

    /// `(start, end, done)` of each row, as UTC strings.
    fn rows(o: &Occurrences) -> Vec<(String, String, bool)> {
        o.rows
            .iter()
            .map(|r| (r.start.to_string(), r.end.to_string(), r.done))
            .collect()
    }

    fn empty(o: &Occurrences) -> bool {
        o.rows.is_empty() && o.series.is_none()
    }

    fn row(start: &str, end: &str, done: bool) -> (String, String, bool) {
        (start.to_owned(), end.to_owned(), done)
    }

    /// A timed task across the spring DST change ends at its due instant
    /// (11 real hours), as an event would; a floating one keeps wall hours.
    #[test]
    fn timed_extent_across_dst_ends_at_due() {
        let t = |zone: Option<&str>| {
            occ(
                json!({"start": "2026-03-28T20:00:00", "due": "2026-03-29T08:00:00",
                "timeZone": zone}),
            )
        };
        assert_eq!(
            rows(&t(Some("Europe/Prague"))),
            [row("2026-03-28T19:00:00Z", "2026-03-29T06:00:00Z", false)]
        );
        assert_eq!(
            rows(&t(None)),
            [row("2026-03-28T20:00:00Z", "2026-03-29T08:00:00Z", false)]
        );
    }

    /// The table of plan 12.2: every combination of dates, with times and
    /// with dates only, in a tzdb zone and floating.
    #[test]
    fn extent_follows_the_table() {
        // (start, due, estimatedDuration, expected local start and end)
        let time = [
            (
                Some("2026-11-20T09:00:00"),
                Some("2026-11-20T11:00:00"),
                None,
                "2026-11-20T09:00:00",
                "2026-11-20T11:00:00",
            ),
            (
                Some("2026-11-20T09:00:00"),
                None,
                Some("PT30M"),
                "2026-11-20T09:00:00",
                "2026-11-20T09:30:00",
            ),
            (
                Some("2026-11-20T09:00:00"),
                None,
                None,
                "2026-11-20T09:00:00",
                "2026-11-20T09:00:00",
            ),
            (
                None,
                Some("2026-11-20T09:00:00"),
                None,
                "2026-11-20T09:00:00",
                "2026-11-20T09:00:00",
            ),
        ];
        let date = [
            (
                Some("2026-11-20T00:00:00"),
                Some("2026-11-21T00:00:00"),
                None,
                "2026-11-20T00:00:00",
                "2026-11-22T00:00:00",
            ),
            (
                Some("2026-11-20T00:00:00"),
                None,
                Some("P2D"),
                "2026-11-20T00:00:00",
                "2026-11-22T00:00:00",
            ),
            (
                Some("2026-11-20T00:00:00"),
                None,
                None,
                "2026-11-20T00:00:00",
                "2026-11-21T00:00:00",
            ),
            (
                None,
                Some("2026-11-20T00:00:00"),
                None,
                "2026-11-20T00:00:00",
                "2026-11-21T00:00:00",
            ),
        ];
        let cases = time
            .iter()
            .map(|c| (c, false))
            .chain(date.iter().map(|c| (c, true)));
        for (&(start, due, est, from, to), whole_day) in cases {
            for zone in [Some("Europe/Prague"), None] {
                let o = occ(json!({
                    "start": start, "due": due, "estimatedDuration": est,
                    "timeZone": zone, "showWithoutTime": whole_day
                }));
                let tz = zone.map_or(TimeZone::UTC, |z| TimeZone::get(z).unwrap());
                let at = |s: &str| {
                    s.parse::<DateTime>()
                        .unwrap()
                        .to_zoned(tz.clone())
                        .unwrap()
                        .timestamp()
                };
                let want = OccurrenceRow {
                    start: at(from),
                    end: at(to),
                    recurrence_id: None,
                    floating: zone.is_none(),
                    done: false,
                };
                assert_eq!(
                    o.rows,
                    [want],
                    "{start:?} {due:?} {est:?} {whole_day} {zone:?}"
                );
                assert_eq!(o.series, None);
            }
        }
        // In Prague 09:00 is 08:00 UTC; a due date-only covers its whole day.
        let o = occ(
            json!({"start": "2026-11-20T09:00:00", "due": "2026-11-20T11:00:00", "timeZone": "Europe/Prague"}),
        );
        assert_eq!(
            rows(&o),
            [row("2026-11-20T08:00:00Z", "2026-11-20T10:00:00Z", false)]
        );
        // Google: a due date, no zone, completed and hidden.
        let o = occ(
            json!({"due": "2026-11-20T00:00:00", "showWithoutTime": true,
            "progress": "completed", "hidden": true}),
        );
        assert_eq!(
            rows(&o),
            [row("2026-11-20T00:00:00Z", "2026-11-21T00:00:00Z", true)]
        );
        assert!(o.rows[0].floating);
        // Without any date: no rows, no series, not unindexed.
        assert!(empty(&occ(json!({"title": "someday"}))));
    }

    #[test]
    fn due_before_start_ends_at_start() {
        let o = occ(
            json!({"start": "2026-11-20T10:00:00", "due": "2026-11-20T09:00:00",
            "timeZone": "Etc/UTC"}),
        );
        assert_eq!(
            rows(&o),
            [row("2026-11-20T10:00:00Z", "2026-11-20T10:00:00Z", false)]
        );
    }

    #[test]
    fn progress_decides_done_and_cancelled_has_no_rows() {
        let with = |p: &str| {
            occ(
                json!({"due": "2026-11-20T09:00:00", "timeZone": "Etc/UTC", "progress": p,
                "recurrenceRules": ["FREQ=DAILY;COUNT=2"]}),
            )
        };
        for (p, done) in [
            ("needs-action", false),
            ("in-process", false),
            ("completed", true),
            ("failed", true),
        ] {
            let o = with(p);
            assert_eq!(o.rows.len(), 2);
            assert!(o.rows.iter().all(|r| r.done == done), "{p}");
        }
        assert!(empty(&with("cancelled")));
        let single = occ(json!({"due": "2026-11-20T09:00:00", "progress": "cancelled"}));
        assert!(empty(&single));
    }

    #[test]
    fn series_anchors_at_start_or_due() {
        let o = occ(
            json!({"start": "2026-11-02T09:00:00", "due": "2026-11-02T10:30:00",
            "timeZone": "Europe/Prague", "recurrenceRules": ["FREQ=DAILY;COUNT=3"]}),
        );
        assert_eq!(
            rows(&o),
            [
                row("2026-11-02T08:00:00Z", "2026-11-02T09:30:00Z", false),
                row("2026-11-03T08:00:00Z", "2026-11-03T09:30:00Z", false),
                row("2026-11-04T08:00:00Z", "2026-11-04T09:30:00Z", false),
            ]
        );
        assert!(
            o.rows
                .iter()
                .all(|r| r.recurrence_id == Some(r.start) && !r.floating)
        );
        let first: Timestamp = "2026-11-02T08:00:00Z".parse().unwrap();
        assert_eq!(
            o.series,
            Some(SeriesSpan {
                first,
                last: None,
                indexed_until: INDEX.horizon
            })
        );
        // Only a due date: whole days from the due date, floating.
        let o = occ(
            json!({"due": "2026-11-02T00:00:00", "showWithoutTime": true,
            "recurrenceRules": ["FREQ=WEEKLY;COUNT=2"]}),
        );
        assert_eq!(
            rows(&o),
            [
                row("2026-11-02T00:00:00Z", "2026-11-03T00:00:00Z", false),
                row("2026-11-09T00:00:00Z", "2026-11-10T00:00:00Z", false),
            ]
        );
        assert!(o.rows.iter().all(|r| r.floating));
    }

    #[test]
    fn overrides_move_complete_and_exclude_occurrences() {
        // Due-anchored: moved through `due`, completed only there, one excluded, one added.
        let o = occ(
            json!({"due": "2026-11-02T09:00:00", "timeZone": "Europe/Prague",
            "recurrenceRules": ["FREQ=DAILY;COUNT=4"],
            "recurrenceOverrides": {
                "2026-11-03T09:00:00": {"due": "2026-11-03T15:00:00", "progress": "completed"},
                "2026-11-04T09:00:00": {"excluded": true},
                "2026-11-10T09:00:00": {}
            }}),
        );
        assert_eq!(
            rows(&o),
            [
                row("2026-11-02T08:00:00Z", "2026-11-02T08:00:00Z", false),
                row("2026-11-03T14:00:00Z", "2026-11-03T14:00:00Z", true),
                row("2026-11-05T08:00:00Z", "2026-11-05T08:00:00Z", false),
                row("2026-11-10T08:00:00Z", "2026-11-10T08:00:00Z", false),
            ]
        );
        assert_eq!(
            o.rows[1].recurrence_id,
            Some("2026-11-03T08:00:00Z".parse().unwrap())
        );
        // Start-anchored: `start` moves, `due` sets the length; a completed
        // master is done except where the override says otherwise. Without
        // `due` in the patch the master's due moves with the start; a removed
        // `progress` is the default, not done.
        let o = occ(
            json!({"start": "2026-11-02T09:00:00", "due": "2026-11-02T10:00:00",
            "timeZone": "Europe/Prague", "progress": "completed",
            "recurrenceRules": ["FREQ=DAILY;COUNT=4"],
            "recurrenceOverrides": {
                "2026-11-03T09:00:00": {"start": "2026-11-03T12:00:00", "due": "2026-11-03T15:00:00"},
                "2026-11-04T09:00:00": {"progress": "needs-action", "title": "again"},
                "2026-11-05T09:00:00": {"start": "2026-11-05T12:00:00", "showWithoutTime": false,
                    "progress": null}
            }}),
        );
        assert_eq!(
            rows(&o),
            [
                row("2026-11-02T08:00:00Z", "2026-11-02T09:00:00Z", true),
                row("2026-11-03T11:00:00Z", "2026-11-03T14:00:00Z", true),
                row("2026-11-04T08:00:00Z", "2026-11-04T09:00:00Z", false),
                row("2026-11-05T11:00:00Z", "2026-11-05T12:00:00Z", false),
            ]
        );
    }

    /// Europe/Prague's current rule as a custom zone (plan 3.7).
    const PRAGUE: &str = "CET-1CEST,M3.5.0,M10.5.0/3";

    #[test]
    fn custom_zone_indexes_and_extends_like_its_iana_zone() {
        let series = |zone: &str, zones: Value| {
            task(
                json!({"start": "2026-03-27T09:00:00", "due": "2026-03-27T10:00:00",
                "timeZone": zone, "timeZones": zones, "recurrenceRules": ["FREQ=DAILY"],
                "recurrenceOverrides": {"2026-06-01T09:00:00": {"progress": "completed"}}}),
            )
        };
        let iana = series("Europe/Prague", json!({}));
        let custom = series("/Prague", json!({"/Prague": PRAGUE}));
        let index = Index {
            horizon: "2026-04-01T00:00:00Z".parse().unwrap(),
            fallback: "2026-03-30T00:00:00Z".parse().unwrap(),
            limit: 1000,
        };
        let key = RemoteKey("k".into());
        let a = task_occurrences(&key, &iana, index, None);
        let b = task_occurrences(&key, &custom, index, None);
        // Across the DST change of 2026-03-29.
        assert_eq!(
            rows(&a)[1..4],
            [
                row("2026-03-28T08:00:00Z", "2026-03-28T09:00:00Z", false),
                row("2026-03-29T07:00:00Z", "2026-03-29T08:00:00Z", false),
                row("2026-03-30T07:00:00Z", "2026-03-30T08:00:00Z", false),
            ]
        );
        assert_eq!((&a.rows, a.series), (&b.rows, b.series));
        // Extending from the horizon gives what a fresh expansion does.
        let until: Timestamp = "2026-11-01T00:00:00Z".parse().unwrap();
        let ext = |t: &CalendarTask| {
            extension(
                &serde_json::to_string(t).unwrap(),
                index.horizon,
                until,
                1000,
            )
            .unwrap()
        };
        let (ea, eb) = (ext(&iana), ext(&custom));
        assert_eq!(ea.len(), 214);
        assert_eq!(ea, eb);
        assert_eq!(ea.iter().filter(|r| r.done).count(), 1);
        let fresh = Index {
            horizon: until,
            ..index
        };
        let mut whole = a.rows.clone();
        whole.extend(ea);
        assert_eq!(task_occurrences(&key, &iana, fresh, None).rows, whole);
    }

    #[test]
    fn unresolved_zone_is_unindexed_never_floating() {
        let first: Timestamp = "2026-03-01T19:00:00Z".parse().unwrap();
        let single = occ(
            json!({"start": "2026-03-02T09:00:00", "due": "2026-03-02T10:00:00",
            "timeZone": "/Nowhere"}),
        );
        assert!(single.rows.is_empty());
        let last = Some("2026-03-03T00:00:00Z".parse().unwrap());
        assert_eq!(
            single.series,
            Some(SeriesSpan {
                first,
                last,
                indexed_until: first
            })
        );
        let json = json!({"uid": "t", "due": "2026-03-02T09:00:00", "timeZone": "Not/A_Zone",
            "recurrenceRules": ["FREQ=DAILY"]});
        let series = occ(json.clone());
        assert!(series.rows.is_empty());
        assert_eq!(
            series.series,
            Some(SeriesSpan {
                first,
                last: None,
                indexed_until: first
            })
        );
        assert!(extension(&json.to_string(), first, INDEX.horizon, 1000).is_err());
    }

    #[test]
    fn too_expensive_series_falls_back_to_the_fallback_horizon() {
        // A rule that never matches walks day by day from its start and gives
        // up after about 100 000 empty days: past the horizon, not the fallback.
        let start = (INDEX.horizon - jiff::SignedDuration::from_hours(24 * 100_500))
            .to_zoned(TimeZone::UTC)
            .datetime()
            .round(jiff::Unit::Day)
            .unwrap();
        let rdate = "2026-11-02T09:00:00";
        let t = json!({"start": start.to_string(), "timeZone": "Etc/UTC",
            "recurrenceRules": ["FREQ=DAILY;BYMONTH=2;BYMONTHDAY=30"],
            "recurrenceOverrides": {rdate: {}}});
        let o = occ(t);
        assert_eq!(o.series.unwrap().indexed_until, INDEX.fallback);
        // The start is an occurrence of its own (RFC 5545), then the RDATE.
        let at = |s: &str| row(s, s, false);
        let start = format!("{start}Z");
        assert_eq!(rows(&o), [at(&start), at("2026-11-02T09:00:00Z")]);
    }

    #[test]
    fn ical_fixtures_index_a_completed_override_and_a_due_date() {
        let golden = |name| task(crate::ingest::tests::ical_golden("tasks", name));
        let key = RemoteKey("k".into());
        // Weekly on Mondays 08:00-20:00 UTC, the second occurrence completed.
        let o = task_occurrences(&key, &golden("recurring"), INDEX, None);
        assert_eq!(
            rows(&o)[..3],
            [
                row("2026-11-09T08:00:00Z", "2026-11-09T20:00:00Z", false),
                row("2026-11-16T08:00:00Z", "2026-11-16T20:00:00Z", true),
                row("2026-11-23T08:00:00Z", "2026-11-23T20:00:00Z", false),
            ]
        );
        // Completed, due on 5 Nov as a date: that whole day, floating.
        let o = task_occurrences(&key, &golden("completed"), INDEX, None);
        assert_eq!(
            rows(&o),
            [row("2026-11-05T00:00:00Z", "2026-11-06T00:00:00Z", true)]
        );
        assert!(o.rows[0].floating);
    }
}
