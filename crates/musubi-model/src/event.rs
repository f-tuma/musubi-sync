//! The canonical event: a profile of JSCalendar (RFC 8984, draft JSCalendar 2.0).
//!
//! The canonical form is a view. The raw layer ([`crate::remote::Raw`]) stays
//! the source of truth for one placement, and fields not listed here are kept
//! there, not in an extension map. Property names and value formats follow
//! JSCalendar (`camelCase`, `LocalDateTime` without offset, ISO 8601 durations)
//! with these deliberate deviations, all marked on the field:
//!
//! - no `@type` members;
//! - recurrence rules are RFC 5545 `RRULE` values, not JSCalendar objects
//!   ([`RecurrenceRule`]);
//! - no `virtualLocations`, `links`, `relatedTo`, `categories`, `color`
//!   in schema version 1.
//!
//! Recurring series in phase 1: the master event carries the rules and the
//! `recurrenceOverrides` it has itself (RDATE as an empty patch, EXDATE as
//! `{"excluded": true}`). A provider exception that is a separate remote object
//! (Google: an event with `recurringEventId`) is its own item and entity in phase 1:
//! its `uid` equals the master's and its `recurrence_id` names the replaced
//! occurrence; a cancelled exception has `status: cancelled`. The engine joins them
//! by (`uid`, `recurrence_id`) when it builds the occurrence index.
//!
//! Every change of these types that changes the JSON is a new
//! [`crate::kind::Kind::SCHEMA_VERSION`].

use std::collections::BTreeMap;
use std::fmt;

use jiff::civil::DateTime;
use jiff::{SignedDuration, Span, Timestamp};
use serde::{Deserialize, Serialize};

/// A calendar event (JSCalendar `Event`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    /// iCalendar UID; with `recurrence_id` the correlation key (proposal 2.4).
    pub uid: String,
    /// Set on a single instance of a series: the original start of the replaced
    /// occurrence, in `recurrence_id_time_zone` (or floating).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence_id: Option<DateTime>,
    /// IANA zone of `recurrence_id`; `None` means floating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence_id_time_zone: Option<String>,
    /// Summary.
    #[serde(default)]
    pub title: String,
    /// Plain-text description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Physical locations by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub locations: BTreeMap<String, Location>,
    /// Local start time in `time_zone`.
    pub start: DateTime,
    /// IANA zone of `start`; `None` means floating (the same wall time everywhere).
    /// All-day events are floating with `show_without_time`.
    #[serde(default)]
    pub time_zone: Option<String>,
    /// Length of the event. Nominal: `P1D` is one calendar day, even across DST.
    #[serde(default)]
    pub duration: Duration,
    /// All-day event: show the date only.
    #[serde(default)]
    pub show_without_time: bool,
    /// Scheduling status.
    #[serde(default)]
    pub status: EventStatus,
    /// Whether the event blocks time.
    #[serde(default)]
    pub free_busy_status: FreeBusyStatus,
    /// Visibility to others.
    #[serde(default)]
    pub privacy: Privacy,
    /// Recurrence rules; empty for a single event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recurrence_rules: Vec<RecurrenceRule>,
    /// Exclusion rules (iCalendar `EXRULE`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_recurrence_rules: Vec<RecurrenceRule>,
    /// Added, excluded or patched occurrences of this master, keyed by
    /// recurrence id (local time in `time_zone`). JSCalendar `PatchObject`s.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recurrence_overrides: BTreeMap<DateTime, PatchObject>,
    /// Organizer and attendees by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub participants: BTreeMap<String, Participant>,
    /// Reminders by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub alerts: BTreeMap<String, Alert>,
    /// The collection's default reminders apply (Google `reminders.useDefault`).
    #[serde(default)]
    pub use_default_alerts: bool,
    /// Creation time reported by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<Timestamp>,
    /// Last modification time reported by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<Timestamp>,
    /// iCalendar `SEQUENCE`.
    #[serde(default)]
    pub sequence: u32,
}

/// JSCalendar `PatchObject`: JSON pointers (relative to the master) to values.
pub type PatchObject = serde_json::Map<String, serde_json::Value>;

/// A JSCalendar duration (ISO 8601, e.g. `PT1H30M`, `P1D`), nominal units kept.
///
/// Wraps [`jiff::Span`]; equality is field-wise (`P1D` ≠ `PT24H`), which is
/// what a document comparison needs.
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Duration(pub Span);

impl PartialEq for Duration {
    fn eq(&self, other: &Self) -> bool {
        self.0.fieldwise() == other.0.fieldwise()
    }
}

impl fmt::Debug for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Duration({})", self.0)
    }
}

/// One RFC 5545 `RRULE` value, e.g. `FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20270101T000000Z`.
///
/// Deviation from JSCalendar (which uses an object): the value is passed to
/// `musubi-recurrence` as is, and RFC 5545 is what providers send. Not validated
/// here; an unparseable rule makes the series unexpandable, not the item invalid.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecurrenceRule(pub String);

/// A physical location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    /// Free-text name or address as the provider gave it.
    pub name: String,
}

/// JSCalendar `status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventStatus {
    /// Confirmed (default).
    #[default]
    Confirmed,
    /// Tentative.
    Tentative,
    /// Cancelled. For an exception: the occurrence does not take place.
    Cancelled,
}

/// JSCalendar `freeBusyStatus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreeBusyStatus {
    /// Blocks time (default).
    #[default]
    Busy,
    /// Does not block time (Google `transparency: transparent`).
    Free,
}

/// JSCalendar `privacy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Privacy {
    /// Visible according to collection sharing (default).
    #[default]
    Public,
    /// Others see only free/busy.
    Private,
    /// Hidden from others entirely.
    Secret,
}

/// A JSCalendar `Participant`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Participant {
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// E-mail address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Roles; a role is present when mapped to `true` (JSCalendar shape).
    #[serde(default)]
    pub roles: BTreeMap<ParticipantRole, bool>,
    /// Reply status.
    #[serde(default)]
    pub participation_status: ParticipationStatus,
    /// The organizer expects a reply.
    #[serde(default)]
    pub expect_reply: bool,
}

/// JSCalendar participant role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParticipantRole {
    /// Owns the event (organizer).
    Owner,
    /// Expected to attend.
    Attendee,
    /// Attendance optional.
    Optional,
    /// Informed only.
    Informational,
    /// Chairs the meeting.
    Chair,
}

/// JSCalendar `participationStatus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParticipationStatus {
    /// No reply yet (default).
    #[default]
    NeedsAction,
    /// Accepted.
    Accepted,
    /// Declined.
    Declined,
    /// Tentatively accepted.
    Tentative,
    /// Delegated to someone else.
    Delegated,
}

/// A JSCalendar `Alert`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    /// When it fires.
    pub trigger: AlertTrigger,
    /// How it is delivered.
    #[serde(default)]
    pub action: AlertAction,
}

/// When an alert fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "@type")]
pub enum AlertTrigger {
    /// Relative to the start or end of the event (negative = before).
    #[serde(rename = "OffsetTrigger", rename_all = "camelCase")]
    Offset {
        /// Signed offset, e.g. `-PT15M`.
        offset: SignedDuration,
        /// Anchor of the offset.
        #[serde(default)]
        relative_to: RelativeTo,
    },
    /// At a fixed instant.
    #[serde(rename = "AbsoluteTrigger")]
    Absolute {
        /// The instant.
        when: Timestamp,
    },
}

/// Anchor of an [`AlertTrigger::Offset`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelativeTo {
    /// Event start (default).
    #[default]
    Start,
    /// Event end.
    End,
}

/// JSCalendar alert `action`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum AlertAction {
    /// Shown by the client (Google `popup`; default).
    #[default]
    Display,
    /// Sent by e-mail.
    Email,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_shape_follows_jscalendar() {
        let mut overrides = BTreeMap::new();
        let mut excluded = PatchObject::new();
        excluded.insert("excluded".into(), json!(true));
        overrides.insert("2026-11-18T09:00:00".parse().unwrap(), excluded);
        let mut alerts = BTreeMap::new();
        alerts.insert(
            "a1".to_string(),
            Alert {
                trigger: AlertTrigger::Offset {
                    offset: SignedDuration::from_mins(-15),
                    relative_to: RelativeTo::Start,
                },
                action: AlertAction::Display,
            },
        );
        let event = CalendarEvent {
            uid: "abc@google.com".into(),
            recurrence_id: None,
            recurrence_id_time_zone: None,
            title: "Standup".into(),
            description: String::new(),
            locations: BTreeMap::new(),
            start: "2026-11-11T09:00:00".parse().unwrap(),
            time_zone: Some("Europe/Prague".into()),
            duration: Duration(Span::new().minutes(15)),
            show_without_time: false,
            status: EventStatus::Confirmed,
            free_busy_status: FreeBusyStatus::Busy,
            privacy: Privacy::Public,
            recurrence_rules: vec![RecurrenceRule("FREQ=WEEKLY;BYDAY=WE".into())],
            excluded_recurrence_rules: vec![],
            recurrence_overrides: overrides,
            participants: BTreeMap::new(),
            alerts,
            use_default_alerts: false,
            created: None,
            updated: None,
            sequence: 0,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            json!({
                "uid": "abc@google.com",
                "title": "Standup",
                "start": "2026-11-11T09:00:00",
                "timeZone": "Europe/Prague",
                "duration": "PT15M",
                "showWithoutTime": false,
                "status": "confirmed",
                "freeBusyStatus": "busy",
                "privacy": "public",
                "recurrenceRules": ["FREQ=WEEKLY;BYDAY=WE"],
                "recurrenceOverrides": {"2026-11-18T09:00:00": {"excluded": true}},
                "alerts": {"a1": {
                    "trigger": {"@type": "OffsetTrigger", "offset": "-PT15M", "relativeTo": "start"},
                    "action": "display"
                }},
                "useDefaultAlerts": false,
                "sequence": 0
            })
        );
        let back: CalendarEvent = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
        assert_ne!(
            Duration(Span::new().days(1)),
            Duration(Span::new().hours(24))
        );
    }
}
