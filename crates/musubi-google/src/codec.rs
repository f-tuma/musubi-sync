//! Google Calendar event JSON → canonical [`CalendarEvent`].
//!
//! Pure and total: no I/O, no panics, every input gives either a view or
//! [`Decoded::Unsupported`]. Fields the canonical form lacks stay in the raw
//! layer, which this module never touches.
//!
//! Google quirks handled here (see also the old Musubi adapter):
//!
//! - all-day `end.date` is exclusive;
//! - `end.timeZone` may differ from `start.timeZone`; the duration is measured
//!   between the two instants;
//! - a one-off event may carry only an offset (`2026-11-12T10:00:00+01:00`) and
//!   no `timeZone`. Google has no floating times, so the event is put in
//!   `Etc/UTC` at the exact instant rather than in a zone it never named;
//! - recurrence `EXDATE`/`RDATE` come as `TZID=…`, UTC (`…Z`), floating or
//!   `VALUE=DATE` values and are converted to local time in the event zone;
//! - a cancelled exception carries only `id`, `recurringEventId` and
//!   `originalStartTime`: its start is the original start and `uid` falls back
//!   to the master id (see [`uid`]).

use std::collections::BTreeMap;

use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Span, Timestamp, Unit, Zoned};
use musubi_model::event::{
    Alert, AlertAction, AlertTrigger, Duration, EventStatus, FreeBusyStatus, Location, Participant,
    ParticipantRole, ParticipationStatus, PatchObject, Privacy, RecurrenceRule, RelativeTo,
};
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::{CalendarEvent, Event, Raw};
use serde::Deserialize;

/// `Codec<Event>` for Google Calendar `events` resources.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoogleCodec;

impl Codec<Event> for GoogleCodec {
    const MAPPING_VERSION: u32 = 2;

    fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
        match decode(raw) {
            Ok(event) => Decoded::Canonical(event),
            Err(reason) => Decoded::Unsupported { reason },
        }
    }
}

type Res<T> = Result<T, String>;

/// The subset of a Google `Event` the canonical view uses. Everything is
/// optional: a cancelled exception has three fields, a private shell no title.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct GEvent {
    id: Option<String>,
    status: Option<String>,
    #[serde(rename = "iCalUID")]
    ical_uid: Option<String>,
    recurring_event_id: Option<String>,
    original_start_time: Option<GTime>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    start: Option<GTime>,
    end: Option<GTime>,
    end_time_unspecified: Option<bool>,
    recurrence: Option<Vec<String>>,
    transparency: Option<String>,
    visibility: Option<String>,
    organizer: Option<GPerson>,
    attendees: Option<Vec<GPerson>>,
    reminders: Option<GReminders>,
    created: Option<String>,
    updated: Option<String>,
    sequence: Option<u32>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
struct GTime {
    date: Option<String>,
    date_time: Option<String>,
    time_zone: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct GPerson {
    email: Option<String>,
    display_name: Option<String>,
    organizer: Option<bool>,
    optional: Option<bool>,
    response_status: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct GReminders {
    use_default: Option<bool>,
    overrides: Option<Vec<GOverride>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GOverride {
    method: Option<String>,
    minutes: Option<u32>,
}

const UTC_NAME: &str = "Etc/UTC";

fn decode(raw: &Raw) -> Res<CalendarEvent> {
    if raw.media_type != MediaType::Json {
        return Err(format!("unexpected media type {:?}", raw.media_type));
    }
    let ev: GEvent =
        serde_json::from_slice(&raw.body).map_err(|e| format!("not a Google event: {e}"))?;

    // A cancelled exception has no `start`; its identity is the original start.
    let start_src = ev
        .start
        .clone()
        .or_else(|| ev.original_start_time.clone())
        .ok_or("event has neither start nor originalStartTime")?;
    let (start, zone, duration, show_without_time) = start_and_duration(&start_src, &ev)?;
    let time_zone = zone.as_ref().map(|(name, _)| name.clone());

    let (recurrence_id, recurrence_id_time_zone) = match &ev.original_start_time {
        None => (None, None),
        // Without `timeZone` the original start is read in Etc/UTC, never in
        // the exception's own zone (which may differ from the master's): the
        // engine correlates occurrences by instant, and `recurrence_id` with
        // its zone names that instant exactly. Live Google (K8) always sent
        // `timeZone` here, cancelled occurrences included.
        Some(t) => {
            let (dt, tz) = local_time(t)?;
            (Some(dt), tz)
        }
    };

    let mut event = CalendarEvent::new(uid(&ev)?, start);
    event.recurrence_id = recurrence_id;
    event.recurrence_id_time_zone = recurrence_id_time_zone;
    event.title = ev.summary.clone().unwrap_or_default();
    event.description = ev.description.clone().unwrap_or_default();
    event.locations = ev
        .location
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| ("location".to_owned(), Location::new(l.clone())))
        .collect();
    event.time_zone = time_zone;
    event.duration = Duration(duration);
    event.show_without_time = show_without_time;
    event.status = match ev.status.as_deref() {
        Some("tentative") => EventStatus::Tentative,
        Some("cancelled") => EventStatus::Cancelled,
        _ => EventStatus::Confirmed,
    };
    event.free_busy_status = match ev.transparency.as_deref() {
        Some("transparent") => FreeBusyStatus::Free,
        _ => FreeBusyStatus::Busy,
    };
    event.privacy = match ev.visibility.as_deref() {
        Some("private" | "confidential") => Privacy::Private,
        _ => Privacy::Public,
    };
    event.participants = participants(&ev);
    event.created = ev.created.as_deref().and_then(|s| s.parse().ok());
    event.updated = ev.updated.as_deref().and_then(|s| s.parse().ok());
    event.sequence = ev.sequence.unwrap_or(0);
    let zone_tz = zone.map(|(_, tz)| tz);
    for line in ev.recurrence.iter().flatten() {
        recurrence_line(line, zone_tz.as_ref(), start, &mut event)?;
    }
    if let Some(r) = &ev.reminders {
        event.use_default_alerts = r.use_default.unwrap_or(false);
        for (i, o) in r.overrides.iter().flatten().enumerate() {
            let action = match o.method.as_deref() {
                Some("popup") => AlertAction::Display,
                Some("email") => AlertAction::Email,
                // Legacy `sms` and anything newer have no canonical action.
                _ => continue,
            };
            let minutes = i64::from(o.minutes.unwrap_or(0));
            event.alerts.insert((i + 1).to_string(), {
                let mut alert = Alert::new(AlertTrigger::Offset {
                    offset: SignedDuration::from_secs(-minutes * 60),
                    relative_to: RelativeTo::Start,
                });
                alert.action = action;
                alert
            });
        }
    }
    Ok(event)
}

/// `iCalUID`, or for an object without one (a cancelled exception guarantees
/// only `id`, `recurringEventId` and `originalStartTime`) the iCalUID Google
/// gives events created through its API: `<master id>@google.com`.
///
/// Wrong for imported series whose iCalUID is not `<id>@google.com`; the
/// engine never joins exceptions by `uid`, it joins them by
/// `RemoteObject::master` (`recurringEventId`), so this only labels the event.
fn uid(ev: &GEvent) -> Res<String> {
    if let Some(uid) = ev.ical_uid.as_ref().filter(|u| !u.is_empty()) {
        return Ok(uid.clone());
    }
    ev.recurring_event_id
        .as_ref()
        .or(ev.id.as_ref())
        .filter(|id| !id.is_empty())
        .map(|id| format!("{id}@google.com"))
        .ok_or_else(|| "event has no iCalUID and no id".to_owned())
}

/// Start (local), zone (name, resolved), duration, all-day flag.
type StartParts = (DateTime, Option<(String, TimeZone)>, Span, bool);

fn start_and_duration(start: &GTime, ev: &GEvent) -> Res<StartParts> {
    let end = ev.end.as_ref();
    match (&start.date, &start.date_time) {
        (Some(d), _) => {
            let day = parse_date(d)?;
            let days = match end.map(|e| (&e.date, &e.date_time)) {
                None | Some((None, None)) => 1,
                Some((Some(e), _)) => day
                    .until((Unit::Day, parse_date(e)?))
                    .map_err(err)?
                    .get_days(),
                Some((None, Some(_))) => return Err("all-day start with timed end".into()),
            };
            if days <= 0 {
                return Err("all-day end is not after start".into());
            }
            Ok((
                day.to_datetime(Time::midnight()),
                None,
                Span::new().days(days),
                true,
            ))
        }
        (None, Some(dt)) => {
            let name = start
                .time_zone
                .clone()
                .unwrap_or_else(|| UTC_NAME.to_owned());
            let tz = zone(&name)?;
            let begin = instant(dt, &tz)?;
            let span = match end.and_then(|e| e.date_time.as_ref().map(|dt| (e, dt))) {
                Some(_) if ev.end_time_unspecified == Some(true) => Span::new(),
                None => Span::new(),
                Some((e, end_dt)) => {
                    let end_tz = match &e.time_zone {
                        Some(n) => zone(n)?,
                        None => tz.clone(),
                    };
                    let finish = instant(end_dt, &end_tz)?.with_time_zone(tz.clone());
                    let span = begin.until((Unit::Hour, &finish)).map_err(err)?;
                    if span.is_negative() {
                        return Err("end is before start".into());
                    }
                    span
                }
            };
            Ok((begin.datetime(), Some((name, tz)), span, false))
        }
        (None, None) => Err("start has neither date nor dateTime".into()),
    }
}

/// Local time of a `{date|dateTime, timeZone}` (Etc/UTC without `timeZone`);
/// dates are floating midnight.
fn local_time(t: &GTime) -> Res<(DateTime, Option<String>)> {
    match (&t.date, &t.date_time) {
        (Some(d), _) => Ok((parse_date(d)?.to_datetime(Time::midnight()), None)),
        (None, Some(dt)) => {
            let name = t.time_zone.as_deref().unwrap_or(UTC_NAME).to_owned();
            let at = instant(dt, &zone(&name)?)?;
            Ok((at.datetime(), Some(name)))
        }
        (None, None) => Err("time has neither date nor dateTime".into()),
    }
}

/// RFC 3339 with offset (what Google sends), or a local time in `tz`.
fn instant(s: &str, tz: &TimeZone) -> Res<Zoned> {
    if let Ok(ts) = s.parse::<Timestamp>() {
        return Ok(ts.to_zoned(tz.clone()));
    }
    let dt: DateTime = s.parse().map_err(err)?;
    dt.to_zoned(tz.clone()).map_err(err)
}

fn zone(name: &str) -> Res<TimeZone> {
    TimeZone::get(name).map_err(|e| format!("unknown time zone {name:?}: {e}"))
}

fn parse_date(s: &str) -> Res<Date> {
    s.parse().map_err(err)
}

fn err(e: jiff::Error) -> String {
    e.to_string()
}

/// One `recurrence` line: `RRULE`, `EXRULE`, `EXDATE` or `RDATE`.
fn recurrence_line(
    line: &str,
    zone: Option<&TimeZone>,
    start: DateTime,
    event: &mut CalendarEvent,
) -> Res<()> {
    let (head, value) = line
        .split_once(':')
        .ok_or_else(|| format!("recurrence line without value: {line:?}"))?;
    let mut parts = head.split(';');
    let name = parts.next().unwrap_or_default().to_ascii_uppercase();
    let mut tzid = None;
    let mut is_date = false;
    for p in parts {
        match p.split_once('=') {
            Some((k, v)) if k.eq_ignore_ascii_case("TZID") => tzid = Some(v),
            Some((k, v)) if k.eq_ignore_ascii_case("VALUE") => {
                is_date = v.eq_ignore_ascii_case("DATE");
            }
            _ => {}
        }
    }
    match name.as_str() {
        "RRULE" => event
            .recurrence_rules
            .push(RecurrenceRule(value.to_owned())),
        "EXRULE" => event
            .excluded_recurrence_rules
            .push(RecurrenceRule(value.to_owned())),
        "EXDATE" | "RDATE" => {
            for v in value.split(',') {
                let at = date_value(v, tzid, is_date, zone, start)?;
                if name == "EXDATE" {
                    let mut patch = PatchObject::new();
                    patch.insert("excluded".into(), true.into());
                    event.recurrence_overrides.insert(at, patch);
                } else {
                    // An EXDATE on the same instant wins over the RDATE.
                    event.recurrence_overrides.entry(at).or_default();
                }
            }
        }
        _ => return Err(format!("unsupported recurrence line {name:?}")),
    }
    Ok(())
}

/// An RFC 5545 DATE / DATE-TIME as local time in the event zone (or as given
/// for a floating all-day series).
fn date_value(
    v: &str,
    tzid: Option<&str>,
    is_date: bool,
    zone: Option<&TimeZone>,
    start: DateTime,
) -> Res<DateTime> {
    // RDATE;VALUE=PERIOD: the occurrence is keyed by its start.
    let v = v.split('/').next().unwrap_or_default().trim();
    let bad = |e: jiff::Error| format!("bad recurrence date {v:?}: {e}");
    if is_date || v.len() == 8 {
        // A DATE excludes the occurrence on that day, at the series' start time.
        let day = Date::strptime("%Y%m%d", v).map_err(bad)?;
        return Ok(day.to_datetime(start.time()));
    }
    let (local, source) = match v.strip_suffix('Z') {
        Some(utc) => (utc, Some(TimeZone::UTC)),
        None => (v, tzid.map(self::zone).transpose()?),
    };
    let dt = DateTime::strptime("%Y%m%dT%H%M%S", local).map_err(bad)?;
    match (source, zone) {
        (Some(src), Some(target)) => Ok(dt
            .to_zoned(src)
            .map_err(bad)?
            .with_time_zone(target.clone())
            .datetime()),
        // Floating value, or a floating (all-day) series: keep the wall time.
        _ => Ok(dt),
    }
}

fn participants(ev: &GEvent) -> BTreeMap<String, Participant> {
    let mut out = BTreeMap::new();
    for (i, a) in ev.attendees.iter().flatten().enumerate() {
        let key = a.email.clone().unwrap_or_else(|| format!("attendee-{i}"));
        let mut p = person(a);
        let role = if a.optional == Some(true) {
            ParticipantRole::Optional
        } else {
            ParticipantRole::Attendee
        };
        p.roles.insert(role, true);
        if a.organizer == Some(true) {
            p.roles.insert(ParticipantRole::Owner, true);
        }
        out.insert(key, p);
    }
    if let Some(o) = &ev.organizer {
        let key = o.email.clone().unwrap_or_else(|| "organizer".to_owned());
        // Without an attendee entry the organizer is the event's owner, whom
        // Google's UI shows as going: Google sends no `responseStatus` here.
        out.entry(key)
            .or_insert_with(|| {
                let mut p = person(o);
                p.participation_status = ParticipationStatus::Accepted;
                p
            })
            .roles
            .insert(ParticipantRole::Owner, true);
    }
    out
}

fn person(p: &GPerson) -> Participant {
    let mut out = Participant::default();
    out.name = p.display_name.clone();
    out.email = p.email.clone();
    out.participation_status = match p.response_status.as_deref() {
        Some("accepted") => ParticipationStatus::Accepted,
        Some("declined") => ParticipationStatus::Declined,
        Some("tentative") => ParticipationStatus::Tentative,
        _ => ParticipationStatus::NeedsAction,
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_json(v: serde_json::Value) -> Decoded<CalendarEvent> {
        GoogleCodec::decode(&Raw {
            media_type: MediaType::Json,
            body: serde_json::to_vec(&v).unwrap(),
        })
    }

    fn canonical(v: serde_json::Value) -> CalendarEvent {
        match decode_json(v) {
            Decoded::Canonical(e) => e,
            Decoded::Unsupported { reason } => panic!("unsupported: {reason}"),
            _ => panic!("unexpected decode result"),
        }
    }

    #[test]
    fn offset_only_event_is_utc_at_the_same_instant() {
        let e = canonical(serde_json::json!({
            "id": "a", "iCalUID": "a@google.com",
            "start": {"dateTime": "2026-11-12T10:00:00+01:00"},
            "end": {"dateTime": "2026-11-12T11:30:00+01:00"}
        }));
        assert_eq!(e.time_zone.as_deref(), Some("Etc/UTC"));
        assert_eq!(e.start, "2026-11-12T09:00:00".parse().unwrap());
        assert_eq!(e.duration, Duration(Span::new().hours(1).minutes(30)));
    }

    #[test]
    fn exdate_forms_are_converted_to_event_local_time() {
        let e = canonical(serde_json::json!({
            "id": "s", "iCalUID": "s@google.com",
            "start": {"dateTime": "2026-11-02T10:00:00+01:00", "timeZone": "Europe/Prague"},
            "end": {"dateTime": "2026-11-02T11:00:00+01:00", "timeZone": "Europe/Prague"},
            "recurrence": [
                "RRULE:FREQ=WEEKLY",
                "EXDATE;TZID=America/New_York:20261109T040000",
                "EXDATE:20261116T090000Z",
                "EXDATE;VALUE=DATE:20261123",
                "RDATE:20261201T100000"
            ]
        }));
        let keys: Vec<String> = e
            .recurrence_overrides
            .keys()
            .map(|k| k.to_string())
            .collect();
        assert_eq!(
            keys,
            [
                "2026-11-09T10:00:00",
                "2026-11-16T10:00:00",
                "2026-11-23T10:00:00",
                "2026-12-01T10:00:00"
            ]
        );
        assert!(
            e.recurrence_overrides[&"2026-12-01T10:00:00".parse::<DateTime>().unwrap()].is_empty()
        );
    }

    #[test]
    fn end_before_start_and_unknown_zone_are_unsupported() {
        for v in [
            serde_json::json!({"id": "x", "start": {"dateTime": "2026-11-12T10:00:00Z"},
                               "end": {"dateTime": "2026-11-12T09:00:00Z"}}),
            serde_json::json!({"id": "x", "start": {"dateTime": "2026-11-12T10:00:00Z",
                               "timeZone": "Mars/Olympus"}}),
            serde_json::json!({"id": "x", "start": {"date": "2026-11-12"}, "end": {"date": "2026-11-12"}}),
            serde_json::json!({"id": "x", "start": {"date": "2026-11-12"}, "recurrence": ["X-FOO:1"]}),
            serde_json::json!([1, 2]),
        ] {
            assert!(matches!(decode_json(v), Decoded::Unsupported { .. }));
        }
    }

    /// Documents the `uid` shortcut: a cancelled shell without `iCalUID` gets
    /// `<recurringEventId>@google.com`, which only matches masters created
    /// through Google. An imported master keeps its foreign UID, so the two no
    /// longer share a `uid` (K5: join on the master's remote key instead).
    #[test]
    fn cancelled_shell_of_an_imported_series_does_not_share_the_master_uid() {
        let shell = serde_json::json!({
            "id": "m_20261109T080000Z", "status": "cancelled", "recurringEventId": "m",
            "originalStartTime": {"dateTime": "2026-11-09T09:00:00+01:00", "timeZone": "Europe/Prague"},
        });
        let master = |uid: &str| {
            serde_json::json!({
                "id": "m", "iCalUID": uid, "recurrence": ["RRULE:FREQ=WEEKLY"],
                "start": {"dateTime": "2026-11-02T09:00:00+01:00", "timeZone": "Europe/Prague"},
                "end": {"dateTime": "2026-11-02T10:00:00+01:00", "timeZone": "Europe/Prague"},
            })
        };
        let shell = canonical(shell);
        assert_eq!(shell.uid, "m@google.com");
        assert_eq!(canonical(master("m@google.com")).uid, shell.uid);
        assert_ne!(
            canonical(master("040000008200E00074C5B7101A82E008@outlook.com")).uid,
            shell.uid
        );
    }

    #[test]
    fn original_start_without_zone_is_the_same_instant_in_utc() {
        let e = canonical(serde_json::json!({
            "id": "m_20261109T080000Z", "recurringEventId": "m",
            "originalStartTime": {"dateTime": "2026-11-09T09:00:00+01:00"},
            "start": {"dateTime": "2026-11-09T05:00:00-05:00", "timeZone": "America/New_York"},
            "end": {"dateTime": "2026-11-09T06:00:00-05:00", "timeZone": "America/New_York"},
        }));
        assert_eq!(
            e.recurrence_id,
            Some("2026-11-09T08:00:00".parse().unwrap())
        );
        assert_eq!(e.recurrence_id_time_zone.as_deref(), Some(UTC_NAME));
    }
}
