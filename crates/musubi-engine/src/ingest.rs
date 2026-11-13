//! Page ingest before the transaction: decode, correlation and occurrence
//! expansion. Pure functions; the engine runs them on a blocking thread, never
//! inside the tenant writer.

use std::collections::{BTreeMap, HashMap};

use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Span, Timestamp};
use musubi_model::error::BoxError;
use musubi_model::event::{CalendarEvent, EventStatus};
use musubi_model::store::{
    CanonicalJson, Correlation, ItemState, ItemView, ItemWrite, OccurrenceRow, Occurrences,
    SeriesSpan,
};
use musubi_model::{Decoded, EntityKind, Kind, RemoteKey, RemoteObject};
use musubi_recurrence::{
    Moment, Override, RecurrenceError, Series, expand, recurrence_id, resolve_tzid,
};
use serde::Serialize;
use serde_json::Value;

use crate::{Codecs, DecodeFn};

/// How far an event in an unresolved zone may be from its wall-clock start
/// read as UTC: the widest UTC offset, as for floating occurrences in `range`.
const FLOAT_WIDEN: SignedDuration = SignedDuration::from_hours(14);

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

/// Schema and mapping version an item of `kind` is decoded with by `codecs`.
/// Mapping 0 without a codec: the item is kept raw only, and a codec added
/// later re-decodes it.
pub(crate) fn versions(codecs: &Codecs, kind: EntityKind) -> (u32, u32) {
    fn of<K: Kind>(d: Option<DecodeFn<K>>) -> (u32, u32) {
        (K::SCHEMA_VERSION, d.map_or(0, |d| d.mapping_version))
    }
    match kind {
        EntityKind::Event => of(codecs.event),
        EntityKind::Task => of(codecs.task),
        EntityKind::Contact => of(codecs.contact),
        EntityKind::ContactGroup => of(codecs.contact_group),
        _ => (0, 0),
    }
}

/// Whether the stored item `s` is up to date with `validator` under `codecs`.
pub(crate) fn current(
    s: &ItemState,
    validator: &musubi_model::Validator,
    kind: EntityKind,
    codecs: &Codecs,
) -> bool {
    !s.tombstone
        && s.validator == *validator
        && (s.schema_version, s.mapping_version) == versions(codecs, kind)
}

/// Splits a page's objects into writes and keys that only need a "seen" mark.
///
/// An object is unchanged when the live item has the same validator and was
/// decoded by the current schema and mapping: nothing is decoded or written.
pub(crate) fn prepare(
    objects: Vec<RemoteObject>,
    known: Vec<ItemState>,
    codecs: &Codecs,
    index: Index,
) -> (Vec<ItemWrite>, Vec<RemoteKey>) {
    let known: HashMap<RemoteKey, ItemState> =
        known.into_iter().map(|s| (s.key.clone(), s)).collect();
    let mut upserts = Vec::new();
    let mut unchanged = Vec::new();
    for obj in objects {
        let state = known.get(&obj.key);
        if state.is_some_and(|s| current(s, &obj.validator, obj.kind, codecs)) {
            unchanged.push(obj.key);
        } else {
            let previous = state.and_then(|s| s.indexed_until);
            upserts.push(item_write(obj, codecs, index, previous));
        }
    }
    (upserts, unchanged)
}

/// The write re-decoding a stored item from its raw content (plan ch. 10,
/// point 1); `None` without raw content. Its `raw_hash` is the stored one, so
/// the store skips it if the item changed since it was read.
///
/// The series master's key comes from the stored correlation (only the
/// provider knows it); the recurrence id is computed afresh, since a new
/// schema may resolve the zone differently. Known limit: an exception that
/// never joined its master (its zone was not resolvable) stays unjoined until
/// the provider sends it again.
pub(crate) fn redecode_write(view: ItemView, codecs: &Codecs, index: Index) -> Option<ItemWrite> {
    let raw = view.raw?;
    let mut obj = RemoteObject::new(view.key, view.kind, view.validator, raw);
    obj.master = view
        .correlation
        .map(|c| RemoteKey(c.uid))
        .filter(|m| *m != obj.key);
    let mut w = item_write(obj, codecs, index, None);
    w.raw_hash = view.raw_hash;
    Some(w)
}

fn item_write(
    obj: RemoteObject,
    codecs: &Codecs,
    index: Index,
    previous: Option<Timestamp>,
) -> ItemWrite {
    let (schema_version, mapping_version) = versions(codecs, obj.kind);
    let mut correlation = None;
    let mut occurrences = Occurrences::default();
    let canonical = match obj.kind {
        EntityKind::Event => decode(codecs.event, &obj).and_then(|ev| {
            let json = to_json(&obj.key, &ev)?;
            let corr = event_correlation(&obj, &ev);
            occurrences = event_occurrences(&obj.key, &ev, corr.recurrence_id, index, previous);
            correlation = Some(corr);
            Some(json)
        }),
        EntityKind::Task => decode(codecs.task, &obj).and_then(|c| to_json(&obj.key, &c)),
        EntityKind::Contact => decode(codecs.contact, &obj).and_then(|c| to_json(&obj.key, &c)),
        EntityKind::ContactGroup => {
            decode(codecs.contact_group, &obj).and_then(|c| to_json(&obj.key, &c))
        }
        _ => None,
    };
    ItemWrite {
        raw_hash: obj.raw.hash(),
        key: obj.key,
        validator: obj.validator,
        raw: obj.raw,
        kind: obj.kind,
        canonical,
        schema_version,
        mapping_version,
        correlation,
        occurrences,
    }
}

/// The canonical form of `obj`; `None` keeps it in the raw layer only, where
/// a later mapping version (or a codec added for its kind) can decode it.
fn decode<K: Kind>(codec: Option<DecodeFn<K>>, obj: &RemoteObject) -> Option<K::Canonical> {
    let Some(codec) = codec else {
        tracing::debug!(key = %obj.key.0, kind = %obj.kind, "no codec for the kind; kept raw only");
        return None;
    };
    let reason = match (codec.decode)(&obj.raw) {
        Decoded::Canonical(c) => return Some(c),
        Decoded::Unsupported { reason } => reason,
        _ => "unsupported decode result".to_owned(),
    };
    tracing::warn!(key = %obj.key.0, %reason, "item not decodable");
    None
}

fn to_json<T: Serialize>(key: &RemoteKey, c: &T) -> Option<CanonicalJson> {
    serde_json::value::to_raw_value(c)
        .inspect_err(
            |e| tracing::warn!(key = %key.0, error = %e, "canonical form not serializable"),
        )
        .ok()
}

/// The series key: the master's remote key for an exception, the item's own
/// key otherwise. Never the iCalendar UID (plan ch. 9, question 3).
fn event_correlation(obj: &RemoteObject, ev: &CalendarEvent) -> Correlation {
    if let (Some(master), Some(rid)) = (&obj.master, ev.recurrence_id) {
        match zone(ev.recurrence_id_time_zone.as_deref(), &ev.time_zones)
            .and_then(|z| Ok(recurrence_id(rid, z.as_ref())?))
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

/// The zone `name` stands for (plan 3.7): a `/`-prefixed name is a custom zone
/// whose POSIX TZ rule is in `zones`, anything else a tzdb name (or one with
/// a known path prefix). `None` without a name (floating). An error for a zone
/// that cannot be resolved: never UTC or floating in its place.
fn zone(
    name: Option<&str>,
    zones: &BTreeMap<String, String>,
) -> Result<Option<TimeZone>, BoxError> {
    let Some(name) = name else { return Ok(None) };
    if name.starts_with('/') {
        let rule = zones
            .get(name)
            .ok_or_else(|| format!("custom zone `{name}` without a rule in time_zones"))?;
        return Ok(Some(TimeZone::posix(rule)?));
    }
    Ok(Some(resolve_tzid(name)?))
}

/// The event's zone, see [`zone`].
fn event_zone(ev: &CalendarEvent) -> Result<Option<TimeZone>, BoxError> {
    zone(ev.time_zone.as_deref(), &ev.time_zones)
}

/// Index rows of one item. `rid` is set for an exception of a series.
///
/// A cancelled item has none: a cancelled exception stays a live item whose
/// correlation hides the master's occurrence. A series that cannot be expanded
/// up to the horizon (e.g. [`RecurrenceError::TooExpensive`]) is indexed up to
/// the largest shorter horizon that works: its `previous` one, then the
/// window read's end. Only when none works does it get no rows and
/// `indexed_until` at its first occurrence. Either way `range` reports it in
/// `unindexed` past its horizon instead of showing it as empty. So is an
/// item in a zone that cannot be resolved ([`unresolved`]).
fn event_occurrences(
    key: &RemoteKey,
    ev: &CalendarEvent,
    rid: Option<Timestamp>,
    index: Index,
    previous: Option<Timestamp>,
) -> Occurrences {
    if ev.status == EventStatus::Cancelled {
        return Occurrences::default();
    }
    let recurring =
        rid.is_none() && !(ev.recurrence_rules.is_empty() && ev.recurrence_overrides.is_empty());
    let zone = match event_zone(ev) {
        Ok(z) => z,
        Err(e) => {
            tracing::warn!(key = %key.0, error = %e, "zone not resolvable; reported as unindexed");
            return unresolved(ev, recurring);
        }
    };
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

/// No rows, only a span for `range` to report the item in `unindexed`
/// (plan 3.7): from the wall-clock start read as UTC widened by
/// [`FLOAT_WIDEN`], open-ended for a series, to the widened end for a single
/// event or an exception, so it shows only in windows that may contain it.
fn unresolved(ev: &CalendarEvent, recurring: bool) -> Occurrences {
    let span = || -> Result<SeriesSpan, jiff::Error> {
        let w = ev.start.to_zoned(TimeZone::UTC)?;
        let first = w.timestamp().checked_sub(FLOAT_WIDEN)?;
        let last = match recurring {
            true => None,
            false => Some(
                w.checked_add(ev.duration.0)?
                    .timestamp()
                    .checked_add(FLOAT_WIDEN)?,
            ),
        };
        Ok(SeriesSpan {
            first,
            last,
            indexed_until: first,
        })
    };
    Occurrences {
        rows: Vec::new(),
        series: span().ok(),
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
    // An unresolved zone keeps the item in `unindexed`, never floating rows.
    let zone = event_zone(&ev)?;
    Ok(expand_rows(&ev, zone, from, until, limit)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use musubi_model::remote::MediaType;
    use musubi_model::{Codec, Event, Raw, Validator};
    use serde_json::json;

    struct Json;
    impl Codec<Event> for Json {
        const MAPPING_VERSION: u32 = 1;
        fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
            Decoded::Canonical(serde_json::from_slice(&raw.body).unwrap())
        }
    }

    fn codecs() -> Codecs {
        Codecs {
            event: Some(DecodeFn::of::<Json>()),
            ..Codecs::default()
        }
    }

    fn ev(v: Value) -> CalendarEvent {
        serde_json::from_value(v).unwrap()
    }

    fn obj(key: &str, ev: &CalendarEvent, master: Option<&str>) -> RemoteObject {
        let raw = Raw {
            media_type: MediaType::Json,
            body: serde_json::to_vec(ev).unwrap(),
        };
        let mut o = RemoteObject::new(
            RemoteKey(key.into()),
            EntityKind::Event,
            Validator("v1".into()),
            raw,
        );
        o.master = master.map(|m| RemoteKey(m.into()));
        o
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
        let w = item_write(obj("m1_20260302", &e, Some("m1")), &codecs(), INDEX, None);
        let c = w.correlation.unwrap();
        assert_eq!(c.uid, "m1");
        assert_eq!(
            c.recurrence_id,
            Some("2026-03-02T08:00:00Z".parse().unwrap())
        );
        assert!(w.occurrences.rows.is_empty() && w.occurrences.series.is_none());

        // Without a master the item is its own series, whatever its uid.
        let w = item_write(obj("solo", &e, None), &codecs(), INDEX, None);
        assert_eq!(w.correlation.unwrap().uid, "solo");
    }

    #[test]
    fn too_expensive_series_is_unindexed_not_empty() {
        let e = ev(json!({
            "uid": "u", "start": "2000-01-01T00:00:00", "timeZone": "Etc/UTC",
            "duration": "PT1M", "recurrenceRules": ["FREQ=MINUTELY"]
        }));
        let zone = event_zone(&e).unwrap();
        assert_eq!(
            expand_rows(&e, zone, Timestamp::MIN, INDEX.horizon, INDEX.limit),
            Err(RecurrenceError::TooExpensive)
        );
        let w = item_write(obj("k", &e, None), &codecs(), INDEX, None);
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
        let w = item_write(obj("m", &e, None), &codecs(), INDEX, None);
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
        let (up, same) = prepare(vec![o.clone()], vec![known.clone()], &codecs(), INDEX);
        assert!(up.is_empty());
        assert_eq!(same, std::slice::from_ref(&o.key));
        let stale = ItemState {
            mapping_version: 0,
            ..known
        };
        let (up, same) = prepare(vec![o], vec![stale], &codecs(), INDEX);
        assert_eq!((up.len(), same.len()), (1, 0));
    }

    fn starts(w: &ItemWrite) -> Vec<String> {
        w.occurrences
            .rows
            .iter()
            .map(|r| r.start.to_string())
            .collect()
    }

    /// Europe/Prague's current rule as a custom zone (plan 3.7).
    const PRAGUE: &str = "CET-1CEST,M3.5.0,M10.5.0/3";

    #[test]
    fn custom_zone_indexes_and_extends_like_its_iana_zone() {
        let series = |zone: Value, zones: Value| {
            ev(json!({
                "uid": "u", "start": "2026-03-27T09:00:00", "timeZone": zone,
                "timeZones": zones, "duration": "PT1H",
                "recurrenceRules": ["FREQ=DAILY"]
            }))
        };
        let iana = series(json!("Europe/Prague"), json!({}));
        let custom = series(json!("/Prague"), json!({"/Prague": PRAGUE}));
        let index = Index {
            horizon: "2026-04-01T00:00:00Z".parse().unwrap(),
            fallback: "2026-03-30T00:00:00Z".parse().unwrap(),
            limit: 1000,
        };
        let a = item_write(obj("a", &iana, None), &codecs(), index, None);
        let b = item_write(obj("b", &custom, None), &codecs(), index, None);
        // Across the DST change of 2026-03-29.
        assert_eq!(
            starts(&a)[1..4],
            [
                "2026-03-28T08:00:00Z",
                "2026-03-29T07:00:00Z",
                "2026-03-30T07:00:00Z"
            ]
        );
        assert_eq!(a.occurrences.rows, b.occurrences.rows);
        assert_eq!(a.occurrences.series, b.occurrences.series);
        let (from, until) = (index.horizon, "2026-11-01T00:00:00Z".parse().unwrap());
        let ext = |e: &CalendarEvent| {
            let json = serde_json::to_string(e).unwrap();
            extension(&json, from, until, 1000).unwrap()
        };
        let (ea, eb) = (ext(&iana), ext(&custom));
        assert_eq!(ea.len(), 214);
        assert_eq!(ea, eb);
    }

    #[test]
    fn unresolved_zone_is_unindexed_never_floating() {
        let w = |v: Value, master: Option<&str>| {
            item_write(obj("k", &ev(v), master), &codecs(), INDEX, None).occurrences
        };
        let first: Timestamp = "2026-03-01T19:00:00Z".parse().unwrap();
        // A single event: a span ending 14 h after its wall-clock end.
        let single = w(
            json!({"uid": "u", "start": "2026-03-02T09:00:00",
            "timeZone": "/Nowhere", "duration": "PT1H"}),
            None,
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
        // A series (no tzdb name either): open-ended.
        let series = w(
            json!({"uid": "u", "start": "2026-03-02T09:00:00",
            "timeZone": "Not/A_Zone", "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY"]}),
            None,
        );
        assert!(series.rows.is_empty());
        assert_eq!(
            series.series,
            Some(SeriesSpan {
                first,
                last: None,
                indexed_until: first
            })
        );
        // An exception of a master: like a single event, and not joined.
        let exc = w(
            json!({"uid": "u", "start": "2026-03-02T09:00:00",
            "timeZone": "/Nowhere", "duration": "PT1H",
            "recurrenceId": "2026-03-02T09:00:00", "recurrenceIdTimeZone": "/Nowhere"}),
            Some("m"),
        );
        assert!(exc.rows.is_empty());
        assert_eq!(
            exc.series,
            Some(SeriesSpan {
                first,
                last,
                indexed_until: first
            })
        );
        // Extending the index fails rather than making floating rows.
        let json = json!({"uid": "u", "start": "2026-03-02T09:00:00",
            "timeZone": "/Nowhere", "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY"]});
        assert!(extension(&json.to_string(), first, INDEX.horizon, 1000).is_err());
    }

    #[test]
    fn multi_component_overrides_index_moved_cancelled_and_added() {
        // The shapes `musubi-ical` gives a resource of several components:
        // EXDATE and a cancelled exception as `excluded`, RDATE as an empty
        // patch, a moved exception as a patch, and an exception whose key no
        // rule generates as an added occurrence.
        let e = ev(json!({
            "uid": "u", "start": "2026-03-02T09:00:00", "timeZone": "Europe/Prague",
            "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY;COUNT=4"],
            "recurrenceOverrides": {
                "2026-03-03T09:00:00": {"excluded": true},
                "2026-03-04T09:00:00": {"start": "2026-03-04T15:00:00"},
                "2026-03-05T09:00:00": {"excluded": true},
                "2026-03-08T09:00:00": {},
                "2026-03-09T11:00:00": {"start": "2026-03-09T12:00:00", "title": "extra"},
                "2026-03-10T09:00:00": {"duration": "PT3H"}
            }
        }));
        let w = item_write(obj("m", &e, None), &codecs(), INDEX, None);
        assert_eq!(
            starts(&w),
            [
                "2026-03-02T08:00:00Z",
                "2026-03-04T14:00:00Z",
                "2026-03-08T08:00:00Z",
                "2026-03-09T11:00:00Z",
                "2026-03-10T08:00:00Z"
            ]
        );
        // An RDATE period of its own length: added with that length.
        let last = w.occurrences.rows.last().unwrap();
        assert_eq!(last.end.to_string(), "2026-03-10T11:00:00Z");
        let rids: Vec<_> = w
            .occurrences
            .rows
            .iter()
            .map(|r| r.recurrence_id.unwrap().to_string())
            .collect();
        assert_eq!(rids[1], "2026-03-04T08:00:00Z");
        assert_eq!(rids[3], "2026-03-09T10:00:00Z");
    }

    #[test]
    fn redecode_takes_master_from_correlation_and_recomputes_the_rid() {
        // A Google-shaped master and its moved exception, as stored.
        let master = ev(json!({
            "uid": "x@google.com", "start": "2026-03-02T09:00:00", "timeZone": "Europe/Prague",
            "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY;COUNT=3"]
        }));
        let moved = ev(json!({
            "uid": "x@google.com", "start": "2026-03-03T15:00:00", "timeZone": "Europe/Prague",
            "duration": "PT1H",
            "recurrenceId": "2026-03-03T09:00:00", "recurrenceIdTimeZone": "Europe/Prague"
        }));
        let view = |o: RemoteObject, corr: Correlation| ItemView {
            id: musubi_model::ItemId::from_u128(1),
            entity: musubi_model::EntityId::from_u128(2),
            collection: musubi_model::CollectionId::from_u128(3),
            kind: EntityKind::Event,
            key: o.key,
            validator: o.validator,
            raw_hash: musubi_model::RawHash([7; 32]),
            schema_version: 1,
            mapping_version: 1,
            observed: None,
            tombstone: false,
            raw: Some(o.raw),
            correlation: Some(corr),
        };
        let first = item_write(obj("m1_x", &moved, Some("m1")), &codecs(), INDEX, None);
        let stored = first.correlation.clone().unwrap();
        // A stale recurrence id in the store is not reused.
        let stale = Correlation {
            recurrence_id: None,
            ..stored.clone()
        };
        let w = redecode_write(view(obj("m1_x", &moved, None), stale), &codecs(), INDEX).unwrap();
        assert_eq!(w.correlation, Some(stored.clone()));
        assert_eq!(w.raw_hash, musubi_model::RawHash([7; 32]));
        assert_eq!(w.occurrences.rows, first.occurrences.rows);
        // The master stays its own series with the moved occurrence's key.
        let own = Correlation {
            uid: "m1".into(),
            recurrence_id: None,
        };
        let m = redecode_write(
            view(obj("m1", &master, None), own.clone()),
            &codecs(),
            INDEX,
        )
        .unwrap();
        assert_eq!(m.correlation, Some(own));
        assert!(
            m.occurrences
                .rows
                .iter()
                .any(|r| r.recurrence_id == stored.recurrence_id)
        );
    }
}
