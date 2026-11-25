//! A recurring VTODO and an event for the range tests over DAV (plan 12.7):
//! the fake DAV server, Radicale and the upgrade from K11 serve the same
//! objects and expect the same occurrences.

use serde_json::Value;

/// Due daily at 09:00 UTC four times, without `DTSTART` (anchored at `DUE`,
/// as clients send it): the second occurrence completed, the third moved to
/// 15:00 through its `DUE`.
pub const RECURRING_TODO: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//test//EN\r\n\
BEGIN:VTODO\r\nUID:water\r\nDTSTAMP:20261101T000000Z\r\nDUE:20261102T090000Z\r\n\
RRULE:FREQ=DAILY;COUNT=4\r\nSUMMARY:water the plants\r\nEND:VTODO\r\n\
BEGIN:VTODO\r\nUID:water\r\nDTSTAMP:20261101T000000Z\r\nRECURRENCE-ID:20261103T090000Z\r\n\
DUE:20261103T090000Z\r\nSTATUS:COMPLETED\r\nSUMMARY:water the plants\r\nEND:VTODO\r\n\
BEGIN:VTODO\r\nUID:water\r\nDTSTAMP:20261101T000000Z\r\nRECURRENCE-ID:20261104T090000Z\r\n\
DUE:20261104T150000Z\r\nSUMMARY:water the plants\r\nEND:VTODO\r\n\
END:VCALENDAR\r\n";

/// One event on 13 November.
pub const EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//test//EN\r\n\
BEGIN:VEVENT\r\nUID:standup\r\nDTSTAMP:20261101T000000Z\r\nDTSTART:20261113T100000Z\r\n\
DTEND:20261113T110000Z\r\nSUMMARY:standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// November 2026: every occurrence of both objects.
pub const NOVEMBER: &str = "from=2026-11-01T00:00:00Z&to=2026-12-01T00:00:00Z";

/// `(kind, recurrence_id, start, done)` of each occurrence of a range page.
pub fn occurrences(page: &Value) -> Vec<(String, Option<String>, String, bool)> {
    page["occurrences"]
        .as_array()
        .expect("occurrences")
        .iter()
        .map(|o| {
            (
                o["kind"].as_str().expect("kind").to_owned(),
                o["recurrence_id"].as_str().map(str::to_owned),
                o["start"].as_str().expect("start").to_owned(),
                o["done"].as_bool().expect("done"),
            )
        })
        .collect()
}

/// The task's occurrences in [`NOVEMBER`]: the moved one keeps its
/// recurrence id, only the completed one is done.
pub fn expected_tasks() -> Vec<(String, Option<String>, String, bool)> {
    [
        ("2026-11-02T09:00:00Z", "2026-11-02T09:00:00Z", false),
        ("2026-11-03T09:00:00Z", "2026-11-03T09:00:00Z", true),
        ("2026-11-04T09:00:00Z", "2026-11-04T15:00:00Z", false),
        ("2026-11-05T09:00:00Z", "2026-11-05T09:00:00Z", false),
    ]
    .map(|(rid, start, done)| ("task".into(), Some(rid.into()), start.into(), done))
    .into()
}

/// Checks the ranges `kind=event,task` (`all`) and `kind=event,task&done=false`
/// (`open`) of [`NOVEMBER`] over both objects.
pub fn check_ranges(all: &Value, open: &Value) {
    let event = (
        "event".to_owned(),
        None,
        "2026-11-13T10:00:00Z".to_owned(),
        false,
    );
    let mut want = expected_tasks();
    want.push(event);
    assert_eq!(occurrences(all), want, "{all}");
    want.remove(1);
    assert_eq!(occurrences(open), want, "{open}");
}

/// Legacy Apple Reminders after completing the 3 November occurrence of a
/// daily reminder (copy-and-advance, plan 12.9 R5; secondary knowledge, not
/// verified live): the master's dates moved to the next occurrence, with
/// Apple's own properties and a location alarm.
pub const APPLE_ADVANCED: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Apple Inc.//iOS 12.4//EN\r\n\
BEGIN:VTODO\r\nUID:bins\r\nDTSTAMP:20261103T183000Z\r\nDTSTART;VALUE=DATE:20261104\r\n\
DUE;VALUE=DATE:20261104\r\nRRULE:FREQ=DAILY;COUNT=2\r\nSUMMARY:take out the bins\r\n\
X-APPLE-SORT-ORDER:576543210\r\n\
BEGIN:VALARM\r\nX-WR-ALARMUID:A1\r\nACTION:DISPLAY\r\nDESCRIPTION:Reminder\r\n\
TRIGGER;VALUE=DATE-TIME:19760401T005545Z\r\nX-APPLE-PROXIMITY:ARRIVE\r\nEND:VALARM\r\n\
END:VTODO\r\nEND:VCALENDAR\r\n";

/// The completed copy [`APPLE_ADVANCED`] leaves behind: its own resource with
/// a new UID and no `RRULE`.
pub const APPLE_COMPLETED_COPY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Apple Inc.//iOS 12.4//EN\r\n\
BEGIN:VTODO\r\nUID:bins-done-1\r\nDTSTAMP:20261103T183000Z\r\nDUE;VALUE=DATE:20261103\r\n\
STATUS:COMPLETED\r\nCOMPLETED:20261103T183000Z\r\nSUMMARY:take out the bins\r\n\
END:VTODO\r\nEND:VCALENDAR\r\n";
