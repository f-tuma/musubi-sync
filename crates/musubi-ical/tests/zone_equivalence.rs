//! A custom two-rule VTIMEZONE gives the same occurrences as the equivalent
//! IANA zone over 20 years (plan K2, chapter 3.7), expanded by
//! `musubi-recurrence` exactly as the engine will.

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
