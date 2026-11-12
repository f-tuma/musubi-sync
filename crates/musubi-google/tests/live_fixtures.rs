//! Bodies recorded from live Google (K8, 12. 11. 2026, personal data
//! replaced), run through the production mapping.

use jiff::Span;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use musubi_google::GoogleCodec;
use musubi_model::event::{
    Duration, EventStatus, FreeBusyStatus, ParticipantRole, ParticipationStatus,
};
use musubi_model::provider::{Codec, Decoded};
use musubi_model::remote::MediaType;
use musubi_model::{CalendarEvent, Raw};

fn live(name: &str) -> CalendarEvent {
    let path = format!(
        "{}/tests/fixtures/live/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let body = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    match GoogleCodec::decode(&Raw {
        media_type: MediaType::Json,
        body,
    }) {
        Decoded::Canonical(event) => event,
        Decoded::Unsupported { reason } => panic!("{name}: unsupported: {reason}"),
    }
}

fn at(s: &str) -> DateTime {
    s.parse().unwrap()
}

#[test]
fn timed_event_keeps_its_zone_and_local_start() {
    let e = live("timed-edited");
    assert_eq!(e.title, "K8 Test 1 edited");
    assert_eq!(e.start, at("2026-11-12T16:30"));
    assert_eq!(e.time_zone.as_deref(), Some("Europe/Prague"));
    assert_eq!(e.duration, Duration(Span::new().hours(1)));
    assert!(!e.show_without_time);
    assert!(e.recurrence_id.is_none());
}

#[test]
fn all_day_events_are_floating_one_day() {
    for (name, day) in [("allday", "2026-11-12"), ("holiday-allday", "2026-11-17")] {
        let e = live(name);
        assert_eq!(e.start, at(&format!("{day}T00:00")), "{name}");
        assert_eq!(e.time_zone, None, "{name}: floating");
        assert_eq!(e.duration, Duration(Span::new().days(1)), "{name}");
        assert!(e.show_without_time, "{name}");
        assert_eq!(e.free_busy_status, FreeBusyStatus::Free, "{name}");
    }
}

#[test]
fn event_in_another_zone_is_the_same_instant_in_its_own_zone() {
    // `dateTime` comes in the calendar's offset (+01:00), `timeZone` names New York.
    let e = live("other-zone");
    assert_eq!(e.time_zone.as_deref(), Some("America/New_York"));
    assert_eq!(e.start, at("2026-11-12T16:30"));
    let instant = e
        .start
        .to_zoned(TimeZone::get("America/New_York").unwrap())
        .unwrap()
        .timestamp();
    assert_eq!(instant, "2026-11-12T21:30:00Z".parse().unwrap());
    assert_eq!(e.duration, Duration(Span::new().hours(1)));
}

#[test]
fn modified_and_cancelled_occurrences_link_to_the_master() {
    let master = live("series-master");
    assert_eq!(master.recurrence_rules.len(), 1);
    assert_eq!(master.recurrence_rules[0].0, "FREQ=DAILY");
    assert_eq!(master.start, at("2026-11-12T16:30"));
    assert_eq!(master.time_zone.as_deref(), Some("Europe/Prague"));

    let moved = live("modified-occurrence");
    assert_eq!(moved.uid, master.uid);
    assert_eq!(moved.title, "K8 Daily series (moved title)");
    assert_eq!(moved.recurrence_id, Some(at("2026-11-14T16:30")));
    assert_eq!(
        moved.recurrence_id_time_zone.as_deref(),
        Some("Europe/Prague")
    );
    assert_eq!(moved.status, EventStatus::Confirmed);

    // Google sends no `start` for a cancelled occurrence: its start is the original one.
    let gone = live("cancelled-occurrence");
    assert_eq!(gone.uid, master.uid);
    assert_eq!(gone.status, EventStatus::Cancelled);
    assert_eq!(gone.recurrence_id, Some(at("2026-11-16T16:30")));
    assert_eq!(
        gone.recurrence_id_time_zone.as_deref(),
        Some("Europe/Prague")
    );
    assert_eq!(gone.start, at("2026-11-16T16:30"));
    assert_eq!(gone.time_zone.as_deref(), Some("Europe/Prague"));
}

#[test]
fn organizer_without_attendees_is_an_accepted_owner() {
    let e = live("timed-edited");
    assert_eq!(e.participants.len(), 1);
    let p = &e.participants["owner@example.com"];
    assert_eq!(p.participation_status, ParticipationStatus::Accepted);
    assert_eq!(p.roles.get(&ParticipantRole::Owner), Some(&true));
}
