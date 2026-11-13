//! The zone chain for iCalendar `TZID`s (docs/plan/phase2.md, chapter 3.7).
//!
//! In this order:
//!
//! 1. an IANA name (the bundled `jiff` tzdb);
//! 2. `X-LIC-LOCATION` of the object's VTIMEZONE;
//! 3. a Windows name, through the CLDR 49 map in [`windows_zones`];
//! 4. a path prefix (`/mozilla.org/…/`, `/softwarestudio.org/…/`) cut down to
//!    the longest `Area/Location` IANA suffix, the same rule as
//!    `musubi_recurrence::resolve_tzid`;
//! 5. the object's VTIMEZONE as a POSIX TZ string, when it has at most one
//!    STANDARD and one DAYLIGHT and the yearly rules have the shape
//!    `FREQ=YEARLY;BYMONTH=…;BYDAY=…`;
//! 6. otherwise unresolved.
//!
//! Steps 1–4 give the IANA name. Steps 5 and 6 give the custom id `/<TZID>`:
//! with the POSIX string in step 5, without it in step 6. An unresolved zone
//! is never read as UTC or floating time.

use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarFrequency, ICalendarProperty,
    ICalendarRecurrenceRule, ICalendarValue, ICalendarWeekday,
};
use jiff::tz::TimeZone;

mod windows_zones;

/// The zone of one `TZID`.
#[derive(Debug, Clone)]
pub(crate) struct Zone {
    /// The canonical `time_zone`: an IANA name or `/<TZID>`.
    pub id: String,
    /// The POSIX TZ string of a custom zone (step 5), the `time_zones` entry.
    pub posix: Option<String>,
    /// `None`: unresolved (step 6).
    pub tz: Option<TimeZone>,
}

/// Resolves `tzid` against the VTIMEZONE components of `cal`.
pub(crate) fn resolve(cal: &ICalendar, tzid: &str) -> Zone {
    let vtimezone = cal.timezones().find(|c| {
        c.property(&ICalendarProperty::Tzid)
            .and_then(|e| e.values.first())
            .and_then(ICalendarValue::as_text)
            == Some(tzid)
    });
    let named = iana(tzid)
        .or_else(|| vtimezone.and_then(lic_location).and_then(iana))
        .or_else(|| windows(tzid).and_then(iana))
        .or_else(|| path_suffix(tzid));
    if let Some((id, tz)) = named {
        return Zone {
            id,
            posix: None,
            tz: Some(tz),
        };
    }
    let posix = vtimezone.and_then(|v| posix(cal, v));
    let tz = posix.as_deref().and_then(|p| TimeZone::posix(p).ok());
    Zone {
        id: format!("/{tzid}"),
        posix: posix.filter(|_| tz.is_some()),
        tz,
    }
}

/// The zone `Etc/UTC`, for `Z` values.
pub(crate) fn utc() -> Zone {
    Zone {
        id: "Etc/UTC".to_owned(),
        posix: None,
        tz: Some(TimeZone::UTC),
    }
}

/// Step 1: the canonical IANA name and the zone.
fn iana(name: &str) -> Option<(String, TimeZone)> {
    let tz = jiff::tz::db().get(name).ok()?;
    let id = tz.iana_name().unwrap_or(name).to_owned();
    Some((id, tz))
}

/// Step 2.
fn lic_location(vtimezone: &ICalendarComponent) -> Option<&str> {
    vtimezone
        .entries
        .iter()
        .find(|e| e.name.as_str().eq_ignore_ascii_case("X-LIC-LOCATION"))
        .and_then(|e| e.values.first())
        .and_then(ICalendarValue::as_text)
}

/// Step 3.
fn windows(name: &str) -> Option<&'static str> {
    let table = windows_zones::WINDOWS_ZONES;
    let i = table.binary_search_by(|(w, _)| (*w).cmp(name)).ok()?;
    Some(table[i].1)
}

/// Step 4. Suffixes keep at least one `/`, so a prefix never resolves to a
/// bare `UTC` or `EST`.
fn path_suffix(tzid: &str) -> Option<(String, TimeZone)> {
    let mut rest = tzid;
    while let Some((_, tail)) = rest.split_once('/') {
        if !tail.contains('/') {
            return None;
        }
        if let Some(found) = iana(tail) {
            return Some(found);
        }
        rest = tail;
    }
    None
}

/// Step 5: the VTIMEZONE as a POSIX TZ string (`CET-1CEST-2,M3.5.0/2,M10.5.0/3`).
fn posix(cal: &ICalendar, vtimezone: &ICalendarComponent) -> Option<String> {
    let (mut standard, mut daylight) = (None, None);
    for &i in &vtimezone.component_ids {
        let c = cal.components.get(usize::try_from(i).ok()?)?;
        let slot = match c.component_type {
            ICalendarComponentType::Standard => &mut standard,
            ICalendarComponentType::Daylight => &mut daylight,
            _ => continue,
        };
        if slot.replace(Observance::read(c)?).is_some() {
            return None;
        }
    }
    let std = standard?;
    let Some(dst) = daylight else {
        // A fixed offset, unless a recurring STANDARD changes the offset each
        // year (TZOFFSETFROM differs), which no fixed offset can carry.
        if std.recurring && std.from != std.to {
            return None;
        }
        return Some(format!("{}{}", name(&std), offset(std.to)));
    };
    if dst.from != std.to || std.from != dst.to {
        return None;
    }
    Some(format!(
        "{}{}{}{},{},{}",
        name(&std),
        offset(std.to),
        name(&dst),
        offset(dst.to),
        dst.rule?,
        std.rule?,
    ))
}

/// One STANDARD or DAYLIGHT component.
struct Observance {
    name: Option<String>,
    from: i32,
    to: i32,
    /// `Mm.w.d/time`, `None` without a convertible RRULE.
    rule: Option<String>,
    /// Has an RRULE, convertible or not.
    recurring: bool,
}

impl Observance {
    fn read(c: &ICalendarComponent) -> Option<Observance> {
        let value = |p: ICalendarProperty| c.property(&p).and_then(|e| e.values.first());
        let utc_offset = |p| match value(p)? {
            ICalendarValue::PartialDateTime(d) => {
                let secs = i32::from(d.tz_hour?) * 3600 + i32::from(d.tz_minute.unwrap_or(0)) * 60;
                Some(if d.tz_minus { -secs } else { secs })
            }
            _ => None,
        };
        let start = match value(ICalendarProperty::Dtstart)? {
            ICalendarValue::PartialDateTime(d) => d,
            _ => return None,
        };
        let at = i32::from(start.hour.unwrap_or(0)) * 3600
            + i32::from(start.minute.unwrap_or(0)) * 60
            + i32::from(start.second.unwrap_or(0));
        let rrules = c.properties(&ICalendarProperty::Rrule).count();
        let rule = match rrules {
            0 => None,
            1 => match value(ICalendarProperty::Rrule)? {
                ICalendarValue::RecurrenceRule(r) => yearly(r).map(|m| format!("{m}/{}", hms(at))),
                _ => None,
            },
            _ => return None,
        };
        // An RDATE adds transitions the POSIX rule cannot carry.
        if c.property(&ICalendarProperty::Rdate).is_some() {
            return None;
        }
        Some(Observance {
            name: value(ICalendarProperty::Tzname)
                .and_then(ICalendarValue::as_text)
                .map(str::to_owned),
            from: utc_offset(ICalendarProperty::Tzoffsetfrom)?,
            to: utc_offset(ICalendarProperty::Tzoffsetto)?,
            rule,
            recurring: rrules > 0,
        })
    }
}

/// `FREQ=YEARLY;BYMONTH=m;BYDAY=nDD` without an end as POSIX `Mm.w.d`, where
/// `n` is 1–4 or -1 (POSIX week 5 is the last one).
fn yearly(r: &ICalendarRecurrenceRule) -> Option<String> {
    let plain = r.freq == ICalendarFrequency::Yearly
        && r.until.is_none()
        && r.count.is_none()
        && r.interval.unwrap_or(1) == 1
        && r.bysecond.is_empty()
        && r.byminute.is_empty()
        && r.byhour.is_empty()
        && r.bymonthday.is_empty()
        && r.byyearday.is_empty()
        && r.byweekno.is_empty()
        && r.bysetpos.is_empty()
        && r.rscale.is_none()
        && r.skip.is_none();
    let ([month], [day]) = (r.bymonth.as_slice(), r.byday.as_slice()) else {
        return None;
    };
    let week = match day.ordwk? {
        n @ 1..=4 => n,
        -1 => 5,
        _ => return None,
    };
    let weekday = match day.weekday {
        ICalendarWeekday::Sunday => 0,
        ICalendarWeekday::Monday => 1,
        ICalendarWeekday::Tuesday => 2,
        ICalendarWeekday::Wednesday => 3,
        ICalendarWeekday::Thursday => 4,
        ICalendarWeekday::Friday => 5,
        ICalendarWeekday::Saturday => 6,
    };
    (plain && !month.is_leap() && (1..=12).contains(&month.month()))
        .then(|| format!("M{}.{week}.{weekday}", month.month()))
}

/// `TZNAME` when POSIX takes it unquoted, otherwise the quoted offset.
fn name(o: &Observance) -> String {
    match &o.name {
        Some(n) if n.len() >= 3 && n.bytes().all(|b| b.is_ascii_alphabetic()) => n.clone(),
        _ => {
            let sign = if o.to < 0 { '-' } else { '+' };
            let a = o.to.unsigned_abs();
            format!("<{sign}{:02}{:02}>", a / 3600, a / 60 % 60)
        }
    }
}

/// A UTC offset in POSIX form: hours west of UTC (`+01:00` is `-1`).
fn offset(secs: i32) -> String {
    let sign = if secs > 0 { "-" } else { "" };
    format!("{sign}{}", hms(secs.abs()))
}

/// `h[:mm[:ss]]`.
fn hms(secs: i32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    match (m, s) {
        (0, 0) => format!("{h}"),
        (_, 0) => format!("{h}:{m:02}"),
        _ => format!("{h}:{m:02}:{s:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(body: &str, tzid: &str) -> Zone {
        let cal = ICalendar::parse(format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n"
        ))
        .unwrap();
        resolve(&cal, tzid)
    }

    fn vtimezone(tzid: &str, inner: &str) -> String {
        format!("BEGIN:VTIMEZONE\r\nTZID:{tzid}\r\n{inner}END:VTIMEZONE\r\n")
    }

    const EU_RULES: &str = "BEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nTZOFFSETFROM:+0200\r\n\
        TZOFFSETTO:+0100\r\nTZNAME:CET\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nEND:STANDARD\r\n\
        BEGIN:DAYLIGHT\r\nDTSTART:19700329T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\n\
        TZNAME:CEST\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nEND:DAYLIGHT\r\n";

    #[test]
    fn named_steps_give_the_iana_name() {
        // 1: IANA, whatever the VTIMEZONE says.
        assert_eq!(zone("", "Europe/Prague").id, "Europe/Prague");
        assert_eq!(zone("", "europe/prague").id, "Europe/Prague");
        // 2: X-LIC-LOCATION.
        let lic = vtimezone("Prague local", "X-LIC-LOCATION:Europe/Prague\r\n");
        assert_eq!(zone(&lic, "Prague local").id, "Europe/Prague");
        // 3: Windows.
        assert_eq!(zone("", "W. Europe Standard Time").id, "Europe/Berlin");
        assert_eq!(
            zone("", "Central Europe Standard Time").id,
            "Europe/Budapest"
        );
        // 4: path prefixes.
        let moz = "/mozilla.org/20050126_1/Europe/Prague";
        assert_eq!(zone("", moz).id, "Europe/Prague");
        let ss = "/softwarestudio.org/Olson_20011030_5/America/Argentina/Buenos_Aires";
        assert_eq!(zone("", ss).id, "America/Argentina/Buenos_Aires");
        assert!(zone("", moz).posix.is_none());
    }

    #[test]
    fn custom_vtimezone_becomes_posix() {
        let z = zone(&vtimezone("Custom EU", EU_RULES), "Custom EU");
        assert_eq!(z.id, "/Custom EU");
        assert_eq!(z.posix.as_deref(), Some("CET-1CEST-2,M3.5.0/2,M10.5.0/3"));
        assert!(z.tz.is_some());

        let fixed = "BEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:+0530\r\n\
            TZOFFSETTO:+0530\r\nEND:STANDARD\r\n";
        let z = zone(&vtimezone("Fixed", fixed), "Fixed");
        assert_eq!(z.posix.as_deref(), Some("<+0530>-5:30"));
        assert!(z.tz.is_some());

        // Southern hemisphere, the US week ordinal, a negative offset.
        let south = "BEGIN:STANDARD\r\nDTSTART:19700405T030000\r\nTZOFFSETFROM:-0300\r\n\
            TZOFFSETTO:-0400\r\nRRULE:FREQ=YEARLY;BYMONTH=4;BYDAY=1SU\r\nEND:STANDARD\r\n\
            BEGIN:DAYLIGHT\r\nDTSTART:19700906T000000\r\nTZOFFSETFROM:-0400\r\n\
            TZOFFSETTO:-0300\r\nRRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=1SU\r\nEND:DAYLIGHT\r\n";
        let z = zone(&vtimezone("South", south), "South");
        assert_eq!(
            z.posix.as_deref(),
            Some("<-0400>4<-0300>3,M9.1.0/0,M4.1.0/3")
        );
        assert!(z.tz.is_some());
    }

    #[test]
    fn unconvertible_vtimezone_stays_unresolved() {
        let until = EU_RULES.replace(
            "BYDAY=-1SU\r\nEND:DAYLIGHT",
            "BYDAY=-1SU;UNTIL=20001029T010000Z\r\nEND:DAYLIGHT",
        );
        let two_standards = format!(
            "{EU_RULES}BEGIN:STANDARD\r\nDTSTART:19600101T000000\r\nTZOFFSETFROM:+0100\r\n\
             TZOFFSETTO:+0100\r\nEND:STANDARD\r\n"
        );
        let monthday = EU_RULES.replace(
            "BYDAY=-1SU\r\nEND:STANDARD",
            "BYMONTHDAY=25\r\nEND:STANDARD",
        );
        let fifth = EU_RULES.replace("BYDAY=-1SU\r\nEND:STANDARD", "BYDAY=5SU\r\nEND:STANDARD");
        let daylight_only = "BEGIN:DAYLIGHT\r\nDTSTART:19700329T020000\r\nTZOFFSETFROM:+0100\r\n\
            TZOFFSETTO:+0200\r\nEND:DAYLIGHT\r\n";
        // A yearly change with no DAYLIGHT to change back: no fixed offset fits.
        let standard_only = &EU_RULES[..EU_RULES.find("BEGIN:DAYLIGHT").unwrap()];
        for inner in [
            &until,
            &two_standards,
            &monthday,
            &fifth,
            daylight_only,
            standard_only,
            "",
        ] {
            let z = zone(&vtimezone("Odd", inner), "Odd");
            assert_eq!(z.id, "/Odd", "{inner}");
            assert!(z.posix.is_none() && z.tz.is_none(), "{inner}");
        }
        // No VTIMEZONE at all.
        let z = zone("", "Nowhere/Special");
        assert_eq!(z.id, "/Nowhere/Special");
        assert!(z.tz.is_none());
    }

    #[test]
    fn windows_table_is_sorted_and_resolvable() {
        let t = windows_zones::WINDOWS_ZONES;
        assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        for (w, iana_name) in t {
            assert!(iana(iana_name).is_some(), "{w} -> {iana_name}");
        }
    }
}
