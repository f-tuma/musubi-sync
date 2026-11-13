//! A custom two-rule VTIMEZONE gives the same occurrences as the equivalent
//! IANA zone over 20 years (plan K2, chapter 3.7), expanded by
//! `musubi-recurrence` exactly as the engine will, and the same offset around
//! every IANA transition, which catches a wrong transition time of day.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use musubi_ical::IcalCodec;
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::{CalendarEvent, Event, Raw};
use musubi_recurrence::{Instance, Series, expand};

fn decode(body: String) -> CalendarEvent {
    let raw = Raw {
        media_type: MediaType::Calendar,
        body: body.into_bytes(),
    };
    match <IcalCodec as Codec<Event>>::decode(&raw) {
        Decoded::Canonical(e) => e,
        other => panic!("{other:?}"),
    }
}

/// The zone the engine would use: IANA by name, `/TZID` via its POSIX entry.
fn zone(e: &CalendarEvent) -> TimeZone {
    let id = e.time_zone.as_deref().expect("zoned");
    match e.time_zones.get(id) {
        Some(posix) => TimeZone::posix(posix).expect("valid POSIX"),
        None => jiff::tz::db().get(id).expect("IANA zone"),
    }
}

fn occurrences(e: &CalendarEvent) -> Vec<Instance> {
    let series = Series {
        start: e.start,
        zone: Some(zone(e)),
        duration: e.duration.0,
        rules: e.recurrence_rules.iter().map(|r| r.0.clone()).collect(),
        exrules: Vec::new(),
        rdates: Vec::new(),
        exdates: Vec::new(),
        overrides: Vec::new(),
    };
    let from: Timestamp = "2026-11-01T00:00:00Z".parse().unwrap();
    let to: Timestamp = "2046-11-01T00:00:00Z".parse().unwrap();
    expand(&series, from..to, 2_000).expect("expands")
}

#[test]
fn custom_two_rule_zone_matches_iana_for_20_years() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/events/zone_custom.ics"
    );
    let custom = std::fs::read_to_string(path).unwrap();
    let iana = custom.replace("Musubi Central", "Europe/Berlin");

    let custom = decode(custom);
    let iana = decode(iana);
    assert_eq!(custom.time_zone.as_deref(), Some("/Musubi Central"));
    assert_eq!(custom.time_zones.len(), 1, "custom zone is converted");
    assert_eq!(iana.time_zone.as_deref(), Some("Europe/Berlin"));

    let a = occurrences(&custom);
    let b = occurrences(&iana);
    assert!(
        a.len() > 1_000,
        "20 years of weekly occurrences: {}",
        a.len()
    );
    assert_eq!(a, b);
}

/// A custom VTIMEZONE named `Musubi Custom` holding `rules`.
fn custom(rules: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi-sync//zone equivalence//EN\r\n\
         BEGIN:VTIMEZONE\r\nTZID:Musubi Custom\r\n{rules}END:VTIMEZONE\r\n\
         BEGIN:VEVENT\r\nUID:zone-1@example.com\r\nDTSTAMP:20261101T000000Z\r\n\
         DTSTART;TZID=Musubi Custom:20261102T090000\r\nDURATION:PT1H\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn observance(kind: &str, start: &str, from: &str, to: &str, rule: &str) -> String {
    format!(
        "BEGIN:{kind}\r\nDTSTART:{start}\r\nTZOFFSETFROM:{from}\r\nTZOFFSETTO:{to}\r\n\
         RRULE:FREQ=YEARLY;{rule}\r\nEND:{kind}\r\n"
    )
}

#[test]
fn custom_zones_match_iana_at_every_transition_for_20_years() {
    let berlin = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/events/zone_custom.ics"
    ))
    .unwrap();
    // Southern hemisphere: DST spans the new year.
    let sydney = custom(&format!(
        "{}{}",
        observance(
            "STANDARD",
            "19700405T030000",
            "+1100",
            "+1000",
            "BYMONTH=4;BYDAY=1SU"
        ),
        observance(
            "DAYLIGHT",
            "19701004T020000",
            "+1000",
            "+1100",
            "BYMONTH=10;BYDAY=1SU"
        ),
    ));
    // Negative offsets.
    let new_york = custom(&format!(
        "{}{}",
        observance(
            "DAYLIGHT",
            "19700308T020000",
            "-0500",
            "-0400",
            "BYMONTH=3;BYDAY=2SU"
        ),
        observance(
            "STANDARD",
            "19701101T020000",
            "-0400",
            "-0500",
            "BYMONTH=11;BYDAY=1SU"
        ),
    ));
    let from: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
    let to: Timestamp = "2046-01-01T00:00:00Z".parse().unwrap();
    let second = jiff::SignedDuration::from_secs(1);
    for (body, iana) in [
        (berlin, "Europe/Berlin"),
        (sydney, "Australia/Sydney"),
        (new_york, "America/New_York"),
    ] {
        let e = decode(body);
        assert_eq!(e.time_zones.len(), 1, "{iana}: custom zone is converted");
        let custom = zone(&e);
        let iana_tz = jiff::tz::db().get(iana).unwrap();
        let mut transitions = 0;
        for t in iana_tz.following(from).take_while(|t| t.timestamp() < to) {
            transitions += 1;
            let at = t.timestamp();
            for ts in [at - second, at, at + second] {
                assert_eq!(
                    custom.to_offset(ts),
                    iana_tz.to_offset(ts),
                    "{iana} at {ts}"
                );
            }
        }
        assert_eq!(transitions, 40, "{iana}: two transitions a year");
    }
}
