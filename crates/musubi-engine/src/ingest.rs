//! Page ingest before the transaction: decode, correlation and occurrence
//! expansion. Pure functions; the engine runs them on a blocking thread, never
//! inside the tenant writer.

use std::collections::HashMap;

use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};
use musubi_model::error::BoxError;
use musubi_model::event::{CalendarEvent, EventStatus};
use musubi_model::store::{
    Correlation, ItemState, ItemWrite, OccurrenceRow, Occurrences, SeriesSpan,
};
use musubi_model::{Codec, Decoded, EntityKind, Event, Kind, RemoteKey, RemoteObject};
use musubi_recurrence::{
    Moment, Override, RecurrenceError, Series, expand, recurrence_id, resolve_tzid,
};
use serde_json::Value;

/// How far the occurrence index reaches when an item is written.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Index {
    /// Occurrences starting before this are indexed (`SeriesSpan::indexed_until`).
    pub horizon: Timestamp,
    /// Last horizon tried when a series cannot be expanded up to `horizon`
    /// (the window read's end), after the series' previous horizon.
    pub fallback: Timestamp,
    /// Most rows one series may produce in one expansion.
    pub limit: usize,
}

/// Splits a page's objects into writes and keys that only need a "seen" mark.
///
/// An object is unchanged when the live item has the same validator and was
/// decoded by the current schema and mapping: nothing is decoded or written.
pub(crate) fn prepare<C: Codec<Event>>(
    objects: Vec<RemoteObject>,
    known: Vec<ItemState>,
    index: Index,
) -> (Vec<ItemWrite>, Vec<RemoteKey>) {
    let known: HashMap<RemoteKey, ItemState> =
        known.into_iter().map(|s| (s.key.clone(), s)).collect();
    let mut upserts = Vec::new();
    let mut unchanged = Vec::new();
    for obj in objects {
        let same = known.get(&obj.key).is_some_and(|s| {
            !s.tombstone
                && s.validator == obj.validator
                && s.schema_version == Event::SCHEMA_VERSION
                && s.mapping_version == C::MAPPING_VERSION
        });
        if same {
            unchanged.push(obj.key);
        } else {
            let previous = known.get(&obj.key).and_then(|s| s.indexed_until);
            upserts.push(item_write::<C>(obj, index, previous));
        }
    }
    (upserts, unchanged)
}

fn item_write<C: Codec<Event>>(
    obj: RemoteObject,
    index: Index,
    previous: Option<Timestamp>,
) -> ItemWrite {
    let decoded = match C::decode(&obj.raw) {
        Decoded::Canonical(ev) => serde_json::value::to_raw_value(&ev)
            .map(|json| (ev, json))
            .map_err(|e| e.to_string()),
        Decoded::Unsupported { reason } => Err(reason),
    };
    let (canonical, correlation, occurrences) = match decoded {
        Ok((ev, json)) => {
            let corr = correlation(&obj, &ev);
            let occ = occurrences(&obj.key, &ev, corr.recurrence_id, index, previous);
            (Some(json), Some(corr), occ)
        }
        Err(reason) => {
            // Kept in the raw layer only; a later mapping version can decode it.
            tracing::warn!(key = %obj.key.0, %reason, "item not decodable");
            (None, None, Occurrences::default())
        }
    };
    ItemWrite {
        raw_hash: obj.raw.hash(),
        key: obj.key,
        validator: obj.validator,
        raw: obj.raw,
        kind: EntityKind::Event,
        canonical,
        schema_version: Event::SCHEMA_VERSION,
        mapping_version: C::MAPPING_VERSION,
        correlation,
        occurrences,
    }
}

/// The series key: the master's remote key for an exception, the item's own
/// key otherwise. Never the iCalendar UID (plan ch. 9, question 3).
fn correlation(obj: &RemoteObject, ev: &CalendarEvent) -> Correlation {
    if let (Some(master), Some(rid)) = (&obj.master, ev.recurrence_id) {
        match zone(ev.recurrence_id_time_zone.as_deref())
            .and_then(|z| recurrence_id(rid, z.as_ref()))
        {
            Ok(key) => {
                return Correlation {
                    uid: master.0.clone(),
                    recurrence_id: Some(key),
                };
            }
            Err(e) => {
                tracing::warn!(key = %obj.key.0, error = %e, "exception not joinable to its master");
            }
        }
    }
    Correlation {
        uid: obj.key.0.clone(),
        recurrence_id: None,
    }
}

fn zone(name: Option<&str>) -> Result<Option<TimeZone>, RecurrenceError> {
    name.map(resolve_tzid).transpose()
}

/// The event's zone; an unknown one indexes the event as floating.
fn event_zone(key: &RemoteKey, ev: &CalendarEvent) -> Option<TimeZone> {
    zone(ev.time_zone.as_deref()).unwrap_or_else(|e| {
        tracing::warn!(key = %key.0, error = %e, "indexed as floating");
        None
    })
}

/// Index rows of one item. `rid` is set for an exception of a series.
///
/// A cancelled item has none: a cancelled exception stays a live item whose
/// correlation hides the master's occurrence. A series that cannot be expanded
/// up to the horizon (e.g. [`RecurrenceError::TooExpensive`]) is indexed up to
/// the largest shorter horizon that works: its `previous` one, then the
/// window read's end. Only when none works does it get no rows and
/// `indexed_until` at its first occurrence. Either way `range` reports it in
/// `unindexed` past its horizon instead of showing it as empty.
fn occurrences(
    key: &RemoteKey,
    ev: &CalendarEvent,
    rid: Option<Timestamp>,
    index: Index,
    previous: Option<Timestamp>,
) -> Occurrences {
    if ev.status == EventStatus::Cancelled {
        return Occurrences::default();
    }
    let zone = event_zone(key, ev);
    let recurring =
        rid.is_none() && !(ev.recurrence_rules.is_empty() && ev.recurrence_overrides.is_empty());
    if !recurring {
        return match single(ev, zone.as_ref(), rid) {
            Ok(row) => Occurrences {
                rows: vec![row],
                series: None,
            },
            Err(e) => {
                tracing::warn!(key = %key.0, error = %e, "event time out of range; not indexed");
                Occurrences::default()
            }
        };
    }
    let first = match recurrence_id(ev.start, zone.as_ref()) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(key = %key.0, error = %e, "series start out of range; not indexed");
            return Occurrences::default();
        }
    };
    let mut horizons = vec![index.horizon];
    horizons.extend(previous);
    horizons.push(index.fallback);
    horizons.retain(|&h| h > first && h <= index.horizon);
    horizons.sort_unstable_by(|a, b| b.cmp(a));
    horizons.dedup();
    for until in horizons {
        match expand_rows(ev, zone.clone(), Timestamp::MIN, until, index.limit) {
            // shortcut: `last` stays unknown (None), so ended series are re-expanded by
            // every index extension; compute it from COUNT/UNTIL if that shows in profiles.
            Ok(rows) => {
                return Occurrences {
                    rows,
                    series: Some(SeriesSpan {
                        first,
                        last: None,
                        indexed_until: until,
                    }),
                };
            }
            Err(e) => {
                tracing::warn!(key = %key.0, error = %e, %until, "series not expandable up to the horizon");
            }
        }
    }
    tracing::warn!(key = %key.0, "series not expandable; reported as unindexed");
    Occurrences {
        rows: Vec::new(),
        series: Some(SeriesSpan {
            first,
            last: None,
            indexed_until: first,
        }),
    }
}

fn single(
    ev: &CalendarEvent,
    zone: Option<&TimeZone>,
    rid: Option<Timestamp>,
) -> Result<OccurrenceRow, jiff::Error> {
    let start = ev.start.to_zoned(zone.cloned().unwrap_or(TimeZone::UTC))?;
    let end = start.checked_add(ev.duration.0)?;
    Ok(OccurrenceRow {
        start: start.timestamp(),
        end: end.timestamp(),
        recurrence_id: rid,
        floating: zone.is_none(),
    })
}

fn series(ev: &CalendarEvent, zone: Option<TimeZone>) -> Series {
    let rules = |r: &[musubi_model::event::RecurrenceRule]| r.iter().map(|r| r.0.clone()).collect();
    let mut s = Series {
        start: ev.start,
        zone,
        duration: ev.duration.0,
        rules: rules(&ev.recurrence_rules),
        exrules: rules(&ev.excluded_recurrence_rules),
        rdates: Vec::new(),
        exdates: Vec::new(),
        overrides: Vec::new(),
    };
    for (rid, patch) in &ev.recurrence_overrides {
        let at = Moment::Local(*rid);
        if patch.is_empty() {
            s.rdates.push(at);
        } else if patch.get("excluded").and_then(Value::as_bool) == Some(true) {
            s.exdates.push(at);
        } else {
            let text = |name| patch.get(name).and_then(Value::as_str);
            let start = text("start").and_then(|v| v.parse::<DateTime>().ok());
            s.overrides.push(Override {
                recurrence_id: at,
                start: Moment::Local(start.unwrap_or(*rid)),
                duration: text("duration").and_then(|v| v.parse::<Span>().ok()),
                this_and_future: false,
            });
        }
    }
    s
}

/// Rows of the occurrences of a master that start in `[from, until)`.
fn expand_rows(
    ev: &CalendarEvent,
    zone: Option<TimeZone>,
    from: Timestamp,
    until: Timestamp,
    limit: usize,
) -> Result<Vec<OccurrenceRow>, RecurrenceError> {
    let floating = zone.is_none();
    Ok(expand(&series(ev, zone), from..until, limit)?
        .into_iter()
        .filter(|i| i.start >= from)
        .map(|i| OccurrenceRow {
            start: i.start,
            end: i.end,
            recurrence_id: Some(i.recurrence_id),
            floating,
        })
        .collect())
}

/// Rows extending a stored master's index from `from` to `until`.
pub(crate) fn extension(
    canonical: &str,
    from: Timestamp,
    until: Timestamp,
    limit: usize,
) -> Result<Vec<OccurrenceRow>, BoxError> {
    let ev: CalendarEvent = serde_json::from_str(canonical)?;
    let zone = zone(ev.time_zone.as_deref()).unwrap_or(None);
    Ok(expand_rows(&ev, zone, from, until, limit)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use musubi_model::remote::MediaType;
    use musubi_model::{Raw, Validator};
    use serde_json::json;

    struct Json;
    impl Codec<Event> for Json {
        const MAPPING_VERSION: u32 = 1;
        fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
            Decoded::Canonical(serde_json::from_slice(&raw.body).unwrap())
        }
    }

    fn ev(v: Value) -> CalendarEvent {
        serde_json::from_value(v).unwrap()
    }

    fn obj(key: &str, ev: &CalendarEvent, master: Option<&str>) -> RemoteObject {
        RemoteObject {
            key: RemoteKey(key.into()),
            validator: Validator("v1".into()),
            raw: Raw {
                media_type: MediaType::Json,
                body: serde_json::to_vec(ev).unwrap(),
            },
            master: master.map(|m| RemoteKey(m.into())),
        }
    }

    const INDEX: Index = Index {
        horizon: Timestamp::constant(1_900_000_000, 0),
        fallback: Timestamp::constant(1_800_000_000, 0),
        limit: 50_000,
    };

    #[test]
    fn exception_joins_master_by_remote_key_not_uid() {
        let e = ev(json!({
            "uid": "foreign@example.com", "start": "2026-03-02T09:00:00",
            "timeZone": "Europe/Prague", "duration": "PT1H",
            "recurrenceId": "2026-03-02T09:00:00", "recurrenceIdTimeZone": "Europe/Prague",
            "status": "cancelled"
        }));
        let w = item_write::<Json>(obj("m1_20260302", &e, Some("m1")), INDEX, None);
        let c = w.correlation.unwrap();
        assert_eq!(c.uid, "m1");
        assert_eq!(
            c.recurrence_id,
            Some("2026-03-02T08:00:00Z".parse().unwrap())
        );
        assert!(w.occurrences.rows.is_empty() && w.occurrences.series.is_none());

        // Without a master the item is its own series, whatever its uid.
        let w = item_write::<Json>(obj("solo", &e, None), INDEX, None);
        assert_eq!(w.correlation.unwrap().uid, "solo");
    }

    #[test]
    fn too_expensive_series_is_unindexed_not_empty() {
        let e = ev(json!({
            "uid": "u", "start": "2000-01-01T00:00:00", "timeZone": "Etc/UTC",
            "duration": "PT1M", "recurrenceRules": ["FREQ=MINUTELY"]
        }));
        let zone = event_zone(&RemoteKey("k".into()), &e);
        assert_eq!(
            expand_rows(&e, zone, Timestamp::MIN, INDEX.horizon, INDEX.limit),
            Err(RecurrenceError::TooExpensive)
        );
        let w = item_write::<Json>(obj("k", &e, None), INDEX, None);
        let first: Timestamp = "2000-01-01T00:00:00Z".parse().unwrap();
        assert!(w.occurrences.rows.is_empty());
        assert_eq!(
            w.occurrences.series,
            Some(SeriesSpan {
                first,
                last: None,
                indexed_until: first
            })
        );
    }

    #[test]
    fn master_rows_carry_recurrence_ids_and_overrides() {
        let e = ev(json!({
            "uid": "u", "start": "2026-03-02T09:00:00", "timeZone": "Europe/Prague",
            "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY;COUNT=3"],
            "recurrenceOverrides": {
                "2026-03-03T09:00:00": {"excluded": true},
                "2026-03-10T09:00:00": {}
            }
        }));
        let w = item_write::<Json>(obj("m", &e, None), INDEX, None);
        let starts: Vec<String> = w
            .occurrences
            .rows
            .iter()
            .map(|r| r.start.to_string())
            .collect();
        assert_eq!(
            starts,
            [
                "2026-03-02T08:00:00Z",
                "2026-03-04T08:00:00Z",
                "2026-03-10T08:00:00Z"
            ]
        );
        assert!(
            w.occurrences
                .rows
                .iter()
                .all(|r| r.recurrence_id == Some(r.start))
        );
        assert_eq!(w.occurrences.series.unwrap().indexed_until, INDEX.horizon);
    }

    #[test]
    fn same_validator_is_unchanged_without_decoding() {
        let e = ev(json!({"uid": "u", "start": "2026-03-02T09:00:00"}));
        let o = obj("k", &e, None);
        let known = ItemState {
            key: o.key.clone(),
            item: musubi_model::ItemId::from_u128(1),
            entity: musubi_model::EntityId::from_u128(2),
            validator: o.validator.clone(),
            raw_hash: musubi_model::RawHash([0; 32]),
            schema_version: Event::SCHEMA_VERSION,
            mapping_version: 1,
            tombstone: false,
            indexed_until: None,
        };
        let (up, same) = prepare::<Json>(vec![o.clone()], vec![known.clone()], INDEX);
        assert!(up.is_empty());
        assert_eq!(same, std::slice::from_ref(&o.key));
        let stale = ItemState {
            mapping_version: 0,
            ..known
        };
        let (up, same) = prepare::<Json>(vec![o], vec![stale], INDEX);
        assert_eq!((up.len(), same.len()), (1, 0));
    }
}
