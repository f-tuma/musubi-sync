//! iCalendar → [`CalendarEvent`] (VEVENT) and [`CalendarTask`] (VTODO).
//!
//! Pure and total: no I/O, no panics, every input gives either a view or
//! [`Decoded::Unsupported`]. Values are read per property from the calcard
//! tree; the object is never serialised back (phase 3 writes patch the raw
//! body).
//!
//! One CalDAV resource carries a series and its exceptions (chapter 3.2): the
//! components that share a UID are folded into one canonical item. The master
//! (no `RECURRENCE-ID`) is the base; without a master the earliest exception is.
//! Every other exception becomes a JSCalendar `PatchObject` in
//! `recurrence_overrides`, keyed by its `RECURRENCE-ID` in the base's zone: the
//! top-level JSON members that differ from the base, `null` for members the
//! exception drops. A key no rule generates is an added occurrence.
//!
//! Zones follow [`crate::tz`]: an IANA name, `/<TZID>` with a `time_zones`
//! entry (POSIX TZ string), or `/<TZID>` without one when unresolved. `…Z`
//! values are `Etc/UTC`, values without `TZID` are floating, `VALUE=DATE`
//! values are floating midnight with `show_without_time`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Display;

use calcard::common::PartialDateTime;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarDuration, ICalendarEntry,
    ICalendarParameterName, ICalendarPeriod, ICalendarProperty, ICalendarValue,
};
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Span, Timestamp, Unit};
use musubi_model::event::{
    Alert, AlertAction, AlertTrigger, Duration, EventStatus, FreeBusyStatus, Location, Participant,
    ParticipantRole, ParticipationStatus, PatchObject, Privacy, RecurrenceRule, RelativeTo,
};
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::task::TaskProgress;
use musubi_model::{CalendarEvent, CalendarTask, Event, Raw, Task};
use serde::Serialize;
use serde_json::Value;

use crate::tz::{self, Zone};

/// `Codec<Event>` (VEVENT) and `Codec<Task>` (VTODO) for CalDAV resources.
#[derive(Debug, Clone, Copy, Default)]
pub struct IcalCodec;

impl Codec<Event> for IcalCodec {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
        decoded(decode::<CalendarEvent>(raw))
    }
}

impl Codec<Task> for IcalCodec {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<CalendarTask> {
        decoded(decode::<CalendarTask>(raw))
    }
}

fn decoded<T>(r: Res<T>) -> Decoded<T> {
    match r {
        Ok(item) => Decoded::Canonical(item),
        Err(reason) => Decoded::Unsupported { reason },
    }
}

type Res<T> = Result<T, String>;

fn err(e: impl Display) -> String {
    format!("{e}")
}

/// Members a recurrence override may not patch (RFC 8984, section 4.3.5), or
/// that only the base carries.
const NOT_PATCHED: &[&str] = &[
    "uid",
    "relatedTo",
    "privacy",
    "recurrenceId",
    "recurrenceIdTimeZone",
    "recurrenceRules",
    "excludedRecurrenceRules",
    "recurrenceOverrides",
    "timeZones",
];

fn decode<T: Item>(raw: &Raw) -> Res<T> {
    if raw.media_type != MediaType::Calendar {
        return Err(format!("unexpected media type {:?}", raw.media_type));
    }
    let text = String::from_utf8_lossy(&raw.body);
    let cal = ICalendar::parse(&*text).map_err(|e| format!("not an iCalendar object: {e:?}"))?;
    let mut cx = Cx {
        cal: &cal,
        zones: HashMap::new(),
    };
    let mut parts = Vec::new();
    for c in cal
        .components
        .iter()
        .filter(|c| c.component_type == T::TYPE)
    {
        // JSCalendar has no form for it, and the engine expands each override alone.
        if c.property(&ICalendarProperty::RecurrenceId)
            .is_some_and(|e| e.parameter(&ICalendarParameterName::Range).is_some())
        {
            return Err("RECURRENCE-ID;RANGE=THISANDFUTURE".into());
        }
        parts.push(T::read(&mut cx, c)?);
    }
    if parts.is_empty() {
        return Err(format!("no {:?} component", T::TYPE));
    }
    if parts.iter().any(|p| p.uid != parts[0].uid) {
        return Err("components with different UIDs".into());
    }
    let masters = parts.iter().filter(|p| p.rid.is_none()).count();
    let base = match masters {
        0 => earliest(&parts),
        1 => parts.iter().position(|p| p.rid.is_none()).unwrap_or(0),
        _ => return Err("more than one component without RECURRENCE-ID".into()),
    };
    let base = parts.swap_remove(base);
    let mut item = base.item;
    let base_json = json(&item)?;
    let resolved = |z: Option<&Zone>| z.is_some_and(|z| z.tz.is_some());
    let unresolved = |z: Option<&Zone>| z.is_some_and(|z| z.tz.is_none());
    // Exception keys only: an exception may replace the master's own RDATE or EXDATE.
    let mut seen = HashSet::new();
    for p in parts {
        let rid = p.rid.as_ref().ok_or("exception without RECURRENCE-ID")?;
        // Its key and times could not be placed in the base's zone.
        if resolved(base.zone.as_ref())
            && (unresolved(p.zone.as_ref()) || unresolved(rid.item_zone().as_ref()))
        {
            return Err("exception in an unresolved time zone".into());
        }
        let key = local_in(rid, base.zone.as_ref())?;
        if !seen.insert(key) {
            return Err("two components with the same RECURRENCE-ID".into());
        }
        // Without a resolved base the patch keeps the exception's own `timeZone`.
        if !resolved(base.zone.as_ref()) {
            item.time_zones().extend(time_zones([p.zone.as_ref()]));
        }
        let patch = if p.item.cancelled() {
            // A cancelled occurrence is removed, not shown with a status.
            let mut patch = PatchObject::new();
            patch.insert("excluded".into(), true.into());
            patch
        } else {
            let mut exception = p.item;
            exception.rebase(p.zone.as_ref(), base.zone.as_ref(), &item)?;
            // An override starts at its key unless the patch says otherwise (JSCalendar).
            let mut at_key = base_json.clone();
            if at_key.contains_key("start") {
                at_key.insert("start".into(), serde_json::to_value(key).map_err(err)?);
            }
            diff(&at_key, &json(&exception)?)
        };
        item.overrides().insert(key, patch);
    }
    Ok(item)
}

/// Index of the live exception with the earliest `RECURRENCE-ID` (by instant
/// when the zone is known, else its wall time read as UTC). When every
/// exception is cancelled, the earliest one: the item is then cancelled and
/// shows no occurrence.
fn earliest<T: Item>(parts: &[Part<T>]) -> usize {
    let at = |p: &Part<T>| p.rid.as_ref().map(Moment::instant_or_utc);
    (0..parts.len())
        .min_by_key(|&i| (parts[i].item.cancelled(), at(&parts[i]), i))
        .unwrap_or(0)
}

fn json<T: Serialize>(item: &T) -> Res<serde_json::Map<String, Value>> {
    match serde_json::to_value(item).map_err(err)? {
        Value::Object(m) => Ok(m),
        _ => Err("canonical form is not an object".into()),
    }
}

/// Top-level members of `exception` that differ from `base`; `null` removes.
fn diff(
    base: &serde_json::Map<String, Value>,
    exception: &serde_json::Map<String, Value>,
) -> PatchObject {
    let mut patch = PatchObject::new();
    for (k, v) in exception {
        if !NOT_PATCHED.contains(&k.as_str()) && base.get(k) != Some(v) {
            patch.insert(k.clone(), v.clone());
        }
    }
    for k in base.keys() {
        if !NOT_PATCHED.contains(&k.as_str()) && !exception.contains_key(k) {
            patch.insert(k.clone(), Value::Null);
        }
    }
    patch
}

/// One decoded VEVENT or VTODO.
struct Part<T> {
    item: T,
    uid: String,
    /// `RECURRENCE-ID`.
    rid: Option<Moment>,
    /// Zone of the item's times; `None` floating.
    zone: Option<Zone>,
}

/// What [`decode`] needs from a canonical type.
trait Item: Serialize + Sized {
    const TYPE: ICalendarComponentType;
    fn read(cx: &mut Cx<'_>, c: &ICalendarComponent) -> Res<Part<Self>>;
    fn overrides(&mut self) -> &mut BTreeMap<DateTime, PatchObject>;
    fn time_zones(&mut self) -> &mut BTreeMap<String, String>;
    /// Moves the times of an exception from its own zone into the base's,
    /// so the patch holds only real changes.
    fn rebase(&mut self, from: Option<&Zone>, to: Option<&Zone>, base: &Self) -> Res<()>;
    /// `STATUS:CANCELLED`.
    fn cancelled(&self) -> bool;
}

/// Per-decode state: the object and the zones resolved so far.
struct Cx<'a> {
    cal: &'a ICalendar,
    zones: HashMap<String, Zone>,
}

impl Cx<'_> {
    fn zone(&mut self, tzid: &str) -> Zone {
        if let Some(z) = self.zones.get(tzid) {
            return z.clone();
        }
        let z = tz::resolve(self.cal, tzid);
        self.zones.insert(tzid.to_owned(), z.clone());
        z
    }

    /// The first value of `prop` as a date or date-time.
    fn moment(&mut self, c: &ICalendarComponent, prop: ICalendarProperty) -> Res<Option<Moment>> {
        let Some(e) = c.property(&prop) else {
            return Ok(None);
        };
        match e.values.first() {
            Some(ICalendarValue::PartialDateTime(d)) => self.read_moment(e, d).map(Some),
            _ => Err(format!("{} is not a date or date-time", prop.as_str())),
        }
    }

    fn read_moment(&mut self, e: &ICalendarEntry, d: &PartialDateTime) -> Res<Moment> {
        let date = Date::new(
            i16::try_from(d.year.ok_or("date without year")?).map_err(err)?,
            i8::try_from(d.month.ok_or("date without month")?).map_err(err)?,
            i8::try_from(d.day.ok_or("date without day")?).map_err(err)?,
        )
        .map_err(err)?;
        let Some(hour) = d.hour else {
            return Ok(Moment {
                local: date.to_datetime(Time::midnight()),
                date: true,
                zone: None,
            });
        };
        let small = |v: Option<u8>| i8::try_from(v.unwrap_or(0)).map_err(err);
        // A leap second (`…T235960`) is read as the second before it.
        let time = Time::new(
            small(Some(hour))?,
            small(d.minute)?,
            small(d.second)?.min(59),
            0,
        )
        .map_err(err)?;
        let local = date.to_datetime(time);
        if let Some(h) = d.tz_hour {
            // `Z`, or a numeric offset (not RFC 5545, but parsed): an instant.
            let secs = i32::from(h) * 3600 + i32::from(d.tz_minute.unwrap_or(0)) * 60;
            let offset =
                Offset::from_seconds(if d.tz_minus { -secs } else { secs }).map_err(err)?;
            let utc = local
                .to_zoned(TimeZone::fixed(offset))
                .map_err(err)?
                .with_time_zone(TimeZone::UTC)
                .datetime();
            return Ok(Moment {
                local: utc,
                date: false,
                zone: Some(tz::utc()),
            });
        }
        let zone = text_param(e, &ICalendarParameterName::Tzid).map(|t| self.zone(t));
        Ok(Moment {
            local,
            date: false,
            zone,
        })
    }
}

/// A DATE or DATE-TIME value.
#[derive(Clone)]
struct Moment {
    local: DateTime,
    date: bool,
    /// `None`: floating (always for a date).
    zone: Option<Zone>,
}

impl Moment {
    fn instant(&self) -> Option<Res<Timestamp>> {
        let tz = self.zone.as_ref()?.tz.as_ref()?;
        Some(
            self.local
                .to_zoned(tz.clone())
                .map(|z| z.timestamp())
                .map_err(err),
        )
    }

    /// Floating and unresolved times are read as UTC.
    fn instant_or_utc(&self) -> Option<Timestamp> {
        match self.instant() {
            Some(r) => r.ok(),
            None => self
                .local
                .to_zoned(TimeZone::UTC)
                .ok()
                .map(|z| z.timestamp()),
        }
    }

    fn zone_id(&self) -> Option<String> {
        self.zone
            .as_ref()
            .filter(|_| !self.date)
            .map(|z| z.id.clone())
    }

    fn item_zone(&self) -> Option<Zone> {
        self.zone.clone().filter(|_| !self.date)
    }
}

/// `m` as local time in `target`. Wall time is kept when either side is
/// floating or unresolved, or both are the same zone.
fn local_in(m: &Moment, target: Option<&Zone>) -> Res<DateTime> {
    let src = m.zone.as_ref().filter(|_| !m.date);
    match (src, target) {
        (Some(s), Some(t)) if s.id != t.id => match (&s.tz, &t.tz) {
            (Some(stz), Some(ttz)) => Ok(m
                .local
                .to_zoned(stz.clone())
                .map_err(err)?
                .with_time_zone(ttz.clone())
                .datetime()),
            _ => Ok(m.local),
        },
        _ => Ok(m.local),
    }
}

/// Moves `at` (local in `from`) into `to`, when both are resolved.
fn shift(at: DateTime, from: Option<&Zone>, to: Option<&Zone>) -> Res<DateTime> {
    local_in(
        &Moment {
            local: at,
            date: false,
            zone: from.cloned(),
        },
        to,
    )
}

/// The POSIX entries of the custom zones among `zones`.
fn time_zones<'a>(zones: impl IntoIterator<Item = Option<&'a Zone>>) -> BTreeMap<String, String> {
    zones
        .into_iter()
        .flatten()
        .filter_map(|z| Some((z.id.clone(), z.posix.clone()?)))
        .collect()
}

fn text<'a>(c: &'a ICalendarComponent, prop: &ICalendarProperty) -> Option<&'a str> {
    c.property(prop)
        .and_then(|e| e.values.first())
        .and_then(ICalendarValue::as_text)
}

fn text_param<'a>(e: &'a ICalendarEntry, name: &ICalendarParameterName) -> Option<&'a str> {
    e.parameter(name).and_then(|v| v.as_text())
}

fn integer(c: &ICalendarComponent, prop: &ICalendarProperty) -> Option<i64> {
    c.property(prop)
        .and_then(|e| e.values.first())
        .and_then(ICalendarValue::as_integer)
}

fn is(value: Option<&str>, expected: &str) -> bool {
    value.is_some_and(|v| v.eq_ignore_ascii_case(expected))
}

fn uid(c: &ICalendarComponent) -> Res<String> {
    c.uid()
        .filter(|u| !u.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "component without UID".to_owned())
}

/// `CREATED` / `LAST-MODIFIED` / `COMPLETED` (floating read as UTC).
fn timestamp(
    cx: &mut Cx<'_>,
    c: &ICalendarComponent,
    prop: ICalendarProperty,
) -> Option<Timestamp> {
    cx.moment(c, prop).ok().flatten()?.instant_or_utc()
}

fn sequence(c: &ICalendarComponent) -> u32 {
    integer(c, &ICalendarProperty::Sequence)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

fn locations(c: &ICalendarComponent) -> BTreeMap<String, Location> {
    text(c, &ICalendarProperty::Location)
        .filter(|l| !l.is_empty())
        .map(|l| ("location".to_owned(), Location::new(l)))
        .into_iter()
        .collect()
}

/// `RRULE` / `EXRULE` values as RFC 5545 text.
fn rules(c: &ICalendarComponent, prop: &ICalendarProperty) -> Vec<RecurrenceRule> {
    c.properties(prop)
        .flat_map(|e| &e.values)
        .filter_map(|v| match v {
            // Display of one value, not of the object.
            ICalendarValue::RecurrenceRule(r) => Some(RecurrenceRule(format!("{r}"))),
            // calcard could not parse it: keep the text, the series will fail to expand.
            other => other.as_text().map(|t| RecurrenceRule(t.to_owned())),
        })
        .collect()
}

/// `RDATE` (empty patch) and `EXDATE` (`excluded`) as overrides keyed by local
/// time in `zone`. A date value keeps the series' time of day; an `RDATE`
/// period is keyed by its start and patches `duration` when it differs from
/// the series' `duration` (`None` for tasks, which have no occurrence
/// length). An `EXDATE` wins over an `RDATE` on the same key.
fn dates(
    cx: &mut Cx<'_>,
    c: &ICalendarComponent,
    start: DateTime,
    zone: Option<&Zone>,
    duration: Option<Span>,
) -> Res<BTreeMap<DateTime, PatchObject>> {
    let mut out = BTreeMap::new();
    for prop in [ICalendarProperty::Rdate, ICalendarProperty::Exdate] {
        for e in c.properties(&prop) {
            for v in &e.values {
                let (d, period) = match v {
                    ICalendarValue::PartialDateTime(d) => (d.as_ref(), None),
                    ICalendarValue::Period(
                        p @ (ICalendarPeriod::Range { start, .. }
                        | ICalendarPeriod::Duration { start, .. }),
                    ) => (start, Some(p)),
                    _ => return Err(format!("bad {} value", prop.as_str())),
                };
                let m = cx.read_moment(e, d)?;
                let length = match period {
                    Some(ICalendarPeriod::Range { end, .. }) => {
                        Some(between(&m, &cx.read_moment(e, end)?)?)
                    }
                    Some(ICalendarPeriod::Duration { duration, .. }) => {
                        Some(span(duration)?).filter(|s| !s.is_negative())
                    }
                    None => None,
                };
                // shortcut: a DATE value on a timed series stands for DTSTART's time of
                // day only, so a rule with several times a day (BYHOUR=9,15) keeps the
                // others; the canonical keys are date-times. Upgrade if a provider sends it.
                let at = if m.date {
                    m.local.date().to_datetime(start.time())
                } else {
                    local_in(&m, zone)?
                };
                if prop == ICalendarProperty::Exdate {
                    let mut patch = PatchObject::new();
                    patch.insert("excluded".into(), true.into());
                    out.insert(at, patch);
                } else {
                    let patch = out.entry(at).or_default();
                    if let (Some(length), Some(series)) = (length, duration) {
                        let length = serde_json::to_value(Duration(length)).map_err(err)?;
                        if length != serde_json::to_value(Duration(series)).map_err(err)? {
                            patch.insert("duration".into(), length);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn span(d: &ICalendarDuration) -> Res<Span> {
    let s = Span::new()
        .try_weeks(i64::from(d.weeks))
        .and_then(|s| s.try_days(i64::from(d.days)))
        .and_then(|s| s.try_hours(i64::from(d.hours)))
        .and_then(|s| s.try_minutes(i64::from(d.minutes)))
        .and_then(|s| s.try_seconds(i64::from(d.seconds)))
        .map_err(err)?;
    Ok(if d.neg { s.negate() } else { s })
}

fn signed(d: &ICalendarDuration) -> SignedDuration {
    let secs = i64::from(d.weeks) * 604_800
        + i64::from(d.days) * 86_400
        + i64::from(d.hours) * 3600
        + i64::from(d.minutes) * 60
        + i64::from(d.seconds);
    SignedDuration::from_secs(if d.neg { -secs } else { secs })
}

/// Length from `start` to `end`: days between dates, else hours between the
/// instants (wall times when a zone is unknown). Never negative.
fn between(start: &Moment, end: &Moment) -> Res<Span> {
    let s = if start.date && end.date {
        Span::new().days(
            start
                .local
                .date()
                .until((Unit::Day, end.local.date()))
                .map_err(err)?
                .get_days(),
        )
    } else if let (false, false, Some(a), Some(b)) =
        (start.date, end.date, start.instant(), end.instant())
    {
        a?.until((Unit::Hour, b?)).map_err(err)?
    } else {
        let finish = local_in(end, start.zone.as_ref())?;
        start.local.until((Unit::Hour, finish)).map_err(err)?
    };
    Ok(if s.is_negative() { Span::new() } else { s })
}

/// VALARMs: `DISPLAY` and `AUDIO` are shown by the client, `EMAIL` is mailed,
/// anything else is skipped. Keys are the VALARM's position, from 1.
fn alerts(cx: &mut Cx<'_>, c: &ICalendarComponent) -> Res<BTreeMap<String, Alert>> {
    let mut out = BTreeMap::new();
    let alarms = c
        .component_ids
        .iter()
        .filter_map(|&i| cx.cal.component_by_id(i))
        .filter(|a| a.component_type == ICalendarComponentType::VAlarm);
    for (i, a) in alarms.enumerate() {
        let action = match text(a, &ICalendarProperty::Action).map(str::to_ascii_uppercase) {
            Some(s) if s == "DISPLAY" || s == "AUDIO" => AlertAction::Display,
            Some(s) if s == "EMAIL" => AlertAction::Email,
            _ => continue,
        };
        let Some(e) = a.property(&ICalendarProperty::Trigger) else {
            continue;
        };
        let trigger = match e.values.first() {
            Some(ICalendarValue::Duration(d)) => AlertTrigger::Offset {
                offset: signed(d),
                relative_to: if is(text_param(e, &ICalendarParameterName::Related), "END") {
                    RelativeTo::End
                } else {
                    RelativeTo::Start
                },
            },
            Some(ICalendarValue::PartialDateTime(d)) => {
                let m = cx.read_moment(e, d)?;
                match m.instant_or_utc() {
                    Some(when) => AlertTrigger::Absolute { when },
                    None => continue,
                }
            }
            _ => continue,
        };
        let mut alert = Alert::new(trigger);
        alert.action = action;
        out.insert(format!("{}", i + 1), alert);
    }
    Ok(out)
}

/// ATTENDEE and ORGANIZER by lowercase e-mail (`mailto:` stripped), else by
/// the URI. The organizer gets the `owner` role; without an ATTENDEE line it is
/// `accepted`, as in the Google codec.
fn participants(c: &ICalendarComponent) -> BTreeMap<String, Participant> {
    let mut out: BTreeMap<String, Participant> = BTreeMap::new();
    for (i, e) in c.properties(&ICalendarProperty::Attendee).enumerate() {
        let (key, mut p) = person(e, || format!("attendee-{i}"));
        let role = match text_param(e, &ICalendarParameterName::Role).map(str::to_ascii_uppercase) {
            Some(r) if r == "CHAIR" => ParticipantRole::Chair,
            Some(r) if r == "OPT-PARTICIPANT" => ParticipantRole::Optional,
            Some(r) if r == "NON-PARTICIPANT" => ParticipantRole::Informational,
            _ => ParticipantRole::Attendee,
        };
        p.roles.insert(role, true);
        p.participation_status =
            match text_param(e, &ICalendarParameterName::Partstat).map(str::to_ascii_uppercase) {
                Some(s) if s == "ACCEPTED" => ParticipationStatus::Accepted,
                Some(s) if s == "DECLINED" => ParticipationStatus::Declined,
                Some(s) if s == "TENTATIVE" => ParticipationStatus::Tentative,
                Some(s) if s == "DELEGATED" => ParticipationStatus::Delegated,
                _ => ParticipationStatus::NeedsAction,
            };
        p.expect_reply = is(text_param(e, &ICalendarParameterName::Rsvp), "TRUE");
        out.insert(key, p);
    }
    if let Some(e) = c.property(&ICalendarProperty::Organizer) {
        let (key, organizer) = person(e, || "organizer".to_owned());
        out.entry(key)
            .or_insert_with(|| {
                let mut p = organizer;
                p.participation_status = ParticipationStatus::Accepted;
                p
            })
            .roles
            .insert(ParticipantRole::Owner, true);
    }
    out
}

fn person(e: &ICalendarEntry, fallback: impl FnOnce() -> String) -> (String, Participant) {
    let uri = e
        .values
        .first()
        .and_then(ICalendarValue::as_text)
        .unwrap_or("");
    let email = uri
        .split_once(':')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("mailto"))
        .map(|(_, a)| a)
        .filter(|a| !a.is_empty());
    let key = match email {
        Some(a) => a.to_ascii_lowercase(),
        None if !uri.is_empty() => uri.to_owned(),
        None => fallback(),
    };
    let mut p = Participant::default();
    p.name = text_param(e, &ICalendarParameterName::Cn)
        .filter(|n| !n.is_empty())
        .map(str::to_owned);
    p.email = email.map(str::to_owned);
    (key, p)
}

impl Item for CalendarEvent {
    const TYPE: ICalendarComponentType = ICalendarComponentType::VEvent;

    fn read(cx: &mut Cx<'_>, c: &ICalendarComponent) -> Res<Part<Self>> {
        let uid = uid(c)?;
        let start = cx
            .moment(c, ICalendarProperty::Dtstart)?
            .ok_or("VEVENT without DTSTART")?;
        let zone = start.item_zone();
        let duration = match (
            cx.moment(c, ICalendarProperty::Dtend)?,
            c.property(&ICalendarProperty::Duration),
        ) {
            (Some(end), _) => between(&start, &end)?,
            (None, Some(e)) => match e.values.first() {
                Some(ICalendarValue::Duration(d)) => {
                    let s = span(d)?;
                    if s.is_negative() { Span::new() } else { s }
                }
                _ => return Err("bad DURATION".into()),
            },
            // RFC 5545, 3.6.1: a date lasts one day, a date-time no time.
            (None, None) if start.date => Span::new().days(1),
            (None, None) => Span::new(),
        };
        let rid = cx.moment(c, ICalendarProperty::RecurrenceId)?;

        let mut ev = CalendarEvent::new(uid.clone(), start.local);
        if let Some(r) = &rid {
            ev.recurrence_id = Some(r.local);
            ev.recurrence_id_time_zone = r.zone_id();
        }
        ev.title = text(c, &ICalendarProperty::Summary)
            .unwrap_or_default()
            .to_owned();
        ev.description = text(c, &ICalendarProperty::Description)
            .unwrap_or_default()
            .to_owned();
        ev.locations = locations(c);
        ev.time_zone = start.zone_id();
        let rid_zone = rid.as_ref().and_then(Moment::item_zone);
        ev.time_zones = time_zones([zone.as_ref(), rid_zone.as_ref()]);
        ev.duration = Duration(duration);
        ev.show_without_time = start.date;
        let status = text(c, &ICalendarProperty::Status);
        ev.status = if is(status, "TENTATIVE") {
            EventStatus::Tentative
        } else if is(status, "CANCELLED") {
            EventStatus::Cancelled
        } else {
            EventStatus::Confirmed
        };
        if is(text(c, &ICalendarProperty::Transp), "TRANSPARENT") {
            ev.free_busy_status = FreeBusyStatus::Free;
        }
        let class = text(c, &ICalendarProperty::Class);
        ev.privacy = if is(class, "PRIVATE") {
            Privacy::Private
        } else if is(class, "CONFIDENTIAL") {
            Privacy::Secret
        } else {
            Privacy::Public
        };
        ev.recurrence_rules = rules(c, &ICalendarProperty::Rrule);
        ev.excluded_recurrence_rules = rules(c, &ICalendarProperty::Exrule);
        ev.recurrence_overrides = dates(cx, c, start.local, zone.as_ref(), Some(duration))?;
        ev.participants = participants(c);
        ev.alerts = alerts(cx, c)?;
        ev.created = timestamp(cx, c, ICalendarProperty::Created);
        ev.updated = timestamp(cx, c, ICalendarProperty::LastModified);
        ev.sequence = sequence(c);
        Ok(Part {
            item: ev,
            uid,
            rid,
            zone,
        })
    }

    fn overrides(&mut self) -> &mut BTreeMap<DateTime, PatchObject> {
        &mut self.recurrence_overrides
    }

    fn time_zones(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.time_zones
    }

    fn rebase(&mut self, from: Option<&Zone>, to: Option<&Zone>, base: &Self) -> Res<()> {
        if self.show_without_time || from.and_then(|z| z.tz.as_ref()).is_none() {
            return Ok(());
        }
        if to.and_then(|z| z.tz.as_ref()).is_some() {
            self.start = shift(self.start, from, to)?;
            self.time_zone.clone_from(&base.time_zone);
        }
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.status == EventStatus::Cancelled
    }
}

impl Item for CalendarTask {
    const TYPE: ICalendarComponentType = ICalendarComponentType::VTodo;

    fn read(cx: &mut Cx<'_>, c: &ICalendarComponent) -> Res<Part<Self>> {
        let uid = uid(c)?;
        let start = cx.moment(c, ICalendarProperty::Dtstart)?;
        let due = cx.moment(c, ICalendarProperty::Due)?;
        // The task's zone is DTSTART's, else DUE's; DUE is shown in it. A
        // date-time wins over a date (RFC 5545 wants both of one type).
        let anchor = [start.as_ref(), due.as_ref()]
            .into_iter()
            .flatten()
            .min_by_key(|m| m.date);
        let zone = anchor.and_then(Moment::item_zone);
        let rid = cx.moment(c, ICalendarProperty::RecurrenceId)?;

        let mut t = CalendarTask::new(uid.clone());
        t.recurrence_id = rid
            .as_ref()
            .map(|r| local_in(r, zone.as_ref()))
            .transpose()?;
        t.title = text(c, &ICalendarProperty::Summary)
            .unwrap_or_default()
            .to_owned();
        t.description = text(c, &ICalendarProperty::Description)
            .unwrap_or_default()
            .to_owned();
        t.start = start.as_ref().map(|m| m.local);
        t.due = due
            .as_ref()
            .map(|m| local_in(m, zone.as_ref()))
            .transpose()?;
        t.time_zone = anchor.and_then(Moment::zone_id);
        t.time_zones = time_zones([zone.as_ref()]);
        t.show_without_time = anchor.is_some_and(|m| m.date);
        t.estimated_duration = match c
            .property(&ICalendarProperty::Duration)
            .and_then(|e| e.values.first())
        {
            Some(ICalendarValue::Duration(d)) => {
                let s = span(d)?;
                Some(Duration(if s.is_negative() { Span::new() } else { s }))
            }
            _ => None,
        };
        let status = text(c, &ICalendarProperty::Status);
        t.progress = if is(status, "IN-PROCESS") {
            TaskProgress::InProcess
        } else if is(status, "COMPLETED") {
            TaskProgress::Completed
        } else if is(status, "CANCELLED") {
            TaskProgress::Cancelled
        } else if is(status, "FAILED") {
            TaskProgress::Failed
        } else {
            TaskProgress::NeedsAction
        };
        t.progress_updated = timestamp(cx, c, ICalendarProperty::Completed);
        t.percent_complete = integer(c, &ICalendarProperty::PercentComplete)
            .map(|p| u8::try_from(p.clamp(0, 100)).unwrap_or(100));
        t.priority = integer(c, &ICalendarProperty::Priority)
            .map_or(0, |p| u8::try_from(p.clamp(0, 9)).unwrap_or(0));
        t.related_to = c
            .properties(&ICalendarProperty::RelatedTo)
            .find(|e| {
                text_param(e, &ICalendarParameterName::Reltype)
                    .is_none_or(|r| r.eq_ignore_ascii_case("PARENT"))
            })
            .and_then(|e| e.values.first())
            .and_then(ICalendarValue::as_text)
            .map(str::to_owned);
        t.recurrence_rules = rules(c, &ICalendarProperty::Rrule);
        let series_start = t.start.or(t.due).unwrap_or_default();
        t.recurrence_overrides = dates(cx, c, series_start, zone.as_ref(), None)?;
        t.alerts = alerts(cx, c)?;
        t.locations = locations(c);
        t.keywords = c
            .properties(&ICalendarProperty::Categories)
            .flat_map(|e| &e.values)
            .filter_map(ICalendarValue::as_text)
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
            .collect();
        t.created = timestamp(cx, c, ICalendarProperty::Created);
        t.updated = timestamp(cx, c, ICalendarProperty::LastModified);
        t.sequence = sequence(c);
        Ok(Part {
            item: t,
            uid,
            rid,
            zone,
        })
    }

    fn overrides(&mut self) -> &mut BTreeMap<DateTime, PatchObject> {
        &mut self.recurrence_overrides
    }

    fn time_zones(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.time_zones
    }

    fn rebase(&mut self, from: Option<&Zone>, to: Option<&Zone>, base: &Self) -> Res<()> {
        if self.show_without_time
            || from.and_then(|z| z.tz.as_ref()).is_none()
            || to.and_then(|z| z.tz.as_ref()).is_none()
        {
            return Ok(());
        }
        self.start = self.start.map(|s| shift(s, from, to)).transpose()?;
        self.due = self.due.map(|d| shift(d, from, to)).transpose()?;
        self.time_zone.clone_from(&base.time_zone);
        Ok(())
    }
    fn cancelled(&self) -> bool {
        self.progress == TaskProgress::Cancelled
    }
}
