//! Golden fixtures for `IcalCodec` and `VCardCodec` (plan K2, chapter 8.4):
//! one snapshot per shape, plus the explicit K2 acceptance checks.

use musubi_ical::{IcalCodec, VCardCodec};
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::{Contact, ContactGroup, Event, Kind, Raw, Task};

fn raw(path: &str) -> Raw {
    let path = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    let body = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let media_type = if path.ends_with(".vcf") {
        MediaType::VCard
    } else {
        MediaType::Calendar
    };
    Raw { media_type, body }
}

fn decode<K: Kind, C: Codec<K>>(path: &str) -> K::Canonical {
    match C::decode(&raw(path)) {
        Decoded::Canonical(item) => item,
        Decoded::Unsupported { reason } => panic!("{path}: unsupported: {reason}"),
        _ => panic!("{path}: unexpected decode result"),
    }
}

fn event(name: &str) -> musubi_model::CalendarEvent {
    decode::<Event, IcalCodec>(&format!("events/{name}.ics"))
}

fn task(name: &str) -> musubi_model::CalendarTask {
    decode::<Task, IcalCodec>(&format!("tasks/{name}.ics"))
}

fn contact(name: &str) -> musubi_model::ContactCard {
    decode::<Contact, VCardCodec>(&format!("contacts/{name}.vcf"))
}

fn group(name: &str) -> musubi_model::ContactGroupCard {
    decode::<ContactGroup, VCardCodec>(&format!("contacts/{name}.vcf"))
}

macro_rules! golden {
    ($decode:ident: $($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            insta::assert_json_snapshot!($decode(stringify!($name)));
        }
    )*};
}

mod events {
    use super::event;
    golden!(event:
        timed,
        utc,
        all_day,
        dst_crossing,
        floating,
        series,
        rdate_moved,
        rdate_period,
        exceptions_only,
        zone_x_lic,
        zone_path_prefix,
        zone_custom,
        zone_fixed_offset,
        zone_unconvertible,
    );
}

mod tasks {
    use super::task;
    golden!(task: child, completed, recurring);
}

mod contacts {
    use super::contact;
    golden!(contact: vcard3_apple, vcard4, org);
}

mod groups {
    use super::group;
    golden!(group: group_apple, group_v4);
}

/// Every step of the zone chain (chapter 3.7) on its own fixture.
#[test]
fn zone_chain_steps() {
    let cases = [
        ("timed", "Europe/Prague"),
        ("zone_x_lic", "Europe/Vienna"),
        ("series", "Europe/Berlin"),
        ("zone_path_prefix", "Europe/Paris"),
    ];
    for (name, zone) in cases {
        let e = event(name);
        assert_eq!(e.time_zone.as_deref(), Some(zone), "{name}");
        assert!(e.time_zones.is_empty(), "{name}: IANA zone needs no entry");
    }
    for (name, posix) in [
        ("zone_custom", "MCT-1MCST-2,M3.5.0/2,M10.5.0/3"),
        ("zone_fixed_offset", "MFT-5:30"),
    ] {
        let e = event(name);
        let id = e.time_zone.clone().expect("zoned");
        assert!(id.starts_with('/'), "{name}: {id}");
        assert_eq!(e.time_zones.get(&id).map(String::as_str), Some(posix));
        jiff::tz::TimeZone::posix(posix).expect("valid POSIX TZ");
    }
}

#[test]
fn unconvertible_zone_stays_unresolved() {
    let e = event("zone_unconvertible");
    assert_eq!(e.time_zone.as_deref(), Some("/Musubi Historic"));
    assert!(e.time_zones.is_empty(), "no entry: the zone is unresolved");
    assert_eq!(
        e.start,
        "2026-11-16T10:00:00".parse().unwrap(),
        "wall time kept"
    );
}

#[test]
fn master_with_two_exceptions_is_one_item() {
    let e = event("series");
    assert!(e.recurrence_id.is_none());
    let at = |key: &str| &e.recurrence_overrides[&key.parse().unwrap()];
    let moved = at("2026-11-16T09:00:00");
    assert_eq!(moved["start"], "2026-11-16T10:00:00");
    assert_eq!(moved["title"], "Standup (moved)");
    // A cancelled exception removes its occurrence, as an EXDATE does.
    let cancelled = at("2026-11-30T09:00:00");
    assert_eq!(cancelled["excluded"], true);
    assert_eq!(cancelled.len(), 1);
    // EXDATE and RDATE share the map (JSCalendar), next to the two exceptions.
    assert_eq!(at("2026-11-23T09:00:00")["excluded"], true);
    assert!(at("2026-11-25T09:00:00").is_empty());
    assert_eq!(e.recurrence_overrides.len(), 4);
}

#[test]
fn exceptions_only_resource_uses_the_earliest_exception() {
    let e = event("exceptions_only");
    assert_eq!(e.title, "Sync");
    assert_eq!(e.start, "2026-12-03T15:00:00".parse().unwrap());
    assert_eq!(
        e.recurrence_id,
        Some("2026-12-03T15:00:00".parse().unwrap())
    );
    assert_eq!(e.recurrence_overrides.len(), 1);
    let later = &e.recurrence_overrides[&"2026-12-10T15:00:00".parse().unwrap()];
    assert_eq!(later["start"], "2026-12-10T16:00:00");
}

fn unsupported<T>(d: Decoded<T>) -> bool {
    matches!(d, Decoded::Unsupported { .. })
}

#[test]
fn wrong_kind_is_unsupported() {
    assert!(unsupported(<IcalCodec as Codec<Task>>::decode(&raw(
        "events/timed.ics"
    ))));
    assert!(unsupported(<IcalCodec as Codec<Event>>::decode(&raw(
        "tasks/child.ics"
    ))));
    assert!(unsupported(<VCardCodec as Codec<ContactGroup>>::decode(
        &raw("contacts/vcard4.vcf")
    )));
    assert!(unsupported(<VCardCodec as Codec<Contact>>::decode(&raw(
        "contacts/group_v4.vcf"
    ))));
    let mut vcard_as_calendar = raw("contacts/vcard4.vcf");
    vcard_as_calendar.media_type = MediaType::Calendar;
    assert!(unsupported(<VCardCodec as Codec<Contact>>::decode(
        &vcard_as_calendar
    )));
}

fn decode_str<K: Kind, C: Codec<K>>(body: String) -> Decoded<K::Canonical> {
    C::decode(&Raw {
        media_type: if body.starts_with("BEGIN:VCARD") {
            MediaType::VCard
        } else {
            MediaType::Calendar
        },
        body: body.into_bytes(),
    })
}

/// The duration counts real hours: the night of the autumn change has 13.
#[test]
fn duration_spans_a_dst_change_in_real_time() {
    let e = event("dst_crossing");
    assert_eq!(e.duration.0.get_hours(), 13, "{:?}", e.duration.0);
}

#[test]
fn this_and_future_is_unsupported() {
    match <IcalCodec as Codec<Event>>::decode(&raw("events/this_and_future.ics")) {
        Decoded::Unsupported { reason } => {
            assert_eq!(reason, "RECURRENCE-ID;RANGE=THISANDFUTURE")
        }
        other => panic!("{other:?}"),
    }
}

/// A cancelled earliest exception does not become the base: the item would
/// then be cancelled and show no occurrence at all.
#[test]
fn exceptions_only_base_skips_a_cancelled_exception() {
    let body = std::fs::read_to_string(format!(
        "{}/tests/fixtures/events/exceptions_only.ics",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace("SUMMARY:Sync\n", "SUMMARY:Sync\nSTATUS:CANCELLED\n");
    let Decoded::Canonical(e) = decode_str::<Event, IcalCodec>(body) else {
        panic!("decodes");
    };
    assert_eq!(e.title, "Sync (later)");
    assert_ne!(e.status, musubi_model::event::EventStatus::Cancelled);
    assert_eq!(e.start, "2026-12-10T16:00:00".parse().unwrap());
    let early = &e.recurrence_overrides[&"2026-12-03T15:00:00".parse().unwrap()];
    assert_eq!(early["excluded"], true);
}

#[test]
fn exception_in_an_unresolved_zone_is_unsupported() {
    let body = std::fs::read_to_string(format!(
        "{}/tests/fixtures/events/series.ics",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replacen(
        "RECURRENCE-ID;TZID=\"W. Europe Standard Time\"",
        "RECURRENCE-ID;TZID=Nowhere/Special",
        1,
    );
    assert!(body.contains("Nowhere/Special"), "fixture changed");
    assert!(unsupported(decode_str::<Event, IcalCodec>(body)));
}

/// Mixed KIND entries: any group entry makes a group, as in `kind_of`.
#[test]
fn any_group_kind_makes_a_group() {
    let body = std::fs::read_to_string(format!(
        "{}/tests/fixtures/contacts/group_apple.vcf",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace(
        "X-ADDRESSBOOKSERVER-KIND",
        "KIND:individual\nX-ADDRESSBOOKSERVER-KIND",
    );
    assert_eq!(
        musubi_ical::kind_of(&Raw {
            media_type: MediaType::VCard,
            body: body.clone().into_bytes()
        }),
        Some(musubi_model::EntityKind::ContactGroup)
    );
    assert!(matches!(
        decode_str::<ContactGroup, VCardCodec>(body.clone()),
        Decoded::Canonical(_)
    ));
    assert!(unsupported(decode_str::<Contact, VCardCodec>(body)));
}

/// An exception may replace the master's own RDATE (moved) or EXDATE
/// (cancelled) at the same time (RFC 5545 allows both).
#[test]
fn exceptions_replace_the_masters_rdate_and_exdate() {
    let e = event("rdate_moved");
    let at = |key: &str| &e.recurrence_overrides[&key.parse().unwrap()];
    assert_eq!(at("2026-11-25T09:00:00")["start"], "2026-11-25T10:00:00");
    assert_eq!(at("2026-11-16T09:00:00")["excluded"], true);
    assert_eq!(e.recurrence_overrides.len(), 2);

    let body = std::fs::read_to_string(format!(
        "{}/tests/fixtures/events/exceptions_only.ics",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let (head, rest) = body.split_once("BEGIN:VEVENT").unwrap();
    let twice =
        format!("{head}BEGIN:VEVENT{rest}").replace("END:VCALENDAR\n", "") + "BEGIN:VEVENT" + rest;
    match decode_str::<Event, IcalCodec>(twice) {
        Decoded::Unsupported { reason } => {
            assert_eq!(reason, "two components with the same RECURRENCE-ID")
        }
        other => panic!("{other:?}"),
    }
}

/// An RDATE period carries its own length when it differs from the series'.
#[test]
fn rdate_period_patches_its_duration() {
    let e = event("rdate_period");
    let at = |key: &str| &e.recurrence_overrides[&key.parse().unwrap()];
    assert_eq!(at("2026-11-20T09:00:00")["duration"], "PT3H");
    assert_eq!(at("2026-11-21T09:00:00")["duration"], "PT2H30M");
    assert!(at("2026-11-22T09:00:00").is_empty(), "same as the series");
}

/// A floating master keeps an exception's own zone in its patch, so the
/// custom zone's POSIX entry must be on the item too.
#[test]
fn floating_master_lists_an_exceptions_custom_zone() {
    let body = std::fs::read_to_string(format!(
        "{}/tests/fixtures/events/zone_fixed_offset.ics",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace(
        "DTSTART;TZID=Musubi Fixed:20261116T100000",
        "DTSTART:20261116T100000",
    )
    .replace(
        "END:VEVENT\n",
        "END:VEVENT\nBEGIN:VEVENT\nUID:fixed-zone-1@example.com\nDTSTAMP:20261111T120000Z\n\
         RECURRENCE-ID:20261117T100000\nDTSTART;TZID=Musubi Fixed:20261117T110000\n\
         DURATION:PT1H\nSUMMARY:Fixed offset zone\nEND:VEVENT\n",
    );
    let Decoded::Canonical(e) = decode_str::<Event, IcalCodec>(body) else {
        panic!("decodes");
    };
    assert!(e.time_zone.is_none(), "floating master");
    let patch = &e.recurrence_overrides[&"2026-11-17T10:00:00".parse().unwrap()];
    assert_eq!(patch["timeZone"], "/Musubi Fixed");
    assert_eq!(
        e.time_zones.get("/Musubi Fixed").map(String::as_str),
        Some("MFT-5:30")
    );
}
