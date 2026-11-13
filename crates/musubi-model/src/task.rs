//! The canonical task: a profile of JSCalendar `Task` (RFC 8984, section 5.2).
//!
//! Same conventions as [`crate::event`] (`camelCase`, `LocalDateTime` without
//! offset, ISO 8601 durations, no `@type`, RFC 5545 `RRULE` values). Sources:
//! iCalendar VTODO and Google Tasks (docs/plan/phase2.md, chapter 3.1). Further
//! deviations from JSCalendar, marked on the fields:
//!
//! - `hidden` is an extension (Google Tasks `hidden`);
//! - `related_to` is the parent's UID, not a JSCalendar `Relation` map;
//! - `keywords` is a JSON array, not a map to `true`.
//!
//! Google's `position`, `links` and `assignmentInfo` stay in the raw layer only.
//! Every change of these types that changes the JSON is a new
//! [`crate::kind::Kind::SCHEMA_VERSION`] of [`crate::kind::Task`].

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use jiff::civil::DateTime;
use serde::{Deserialize, Serialize};

use crate::event::{Alert, Duration, Location, PatchObject, RecurrenceRule};

/// A task (JSCalendar `Task`).
///
/// Non-exhaustive: build it with [`CalendarTask::new`] and set the other fields
/// by assignment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CalendarTask {
    /// iCalendar `UID`, or the Google Tasks `id`.
    pub uid: String,
    /// iCalendar `RECURRENCE-ID` of a single instance, local time in
    /// `time_zone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence_id: Option<DateTime>,
    /// `SUMMARY` / Google `title`.
    #[serde(default)]
    pub title: String,
    /// `DESCRIPTION` / Google `notes`, plain text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `DTSTART`, local time in `time_zone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime>,
    /// `DUE`, local time in `time_zone`. Google Tasks gives a date only
    /// (midnight with `show_without_time`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<DateTime>,
    /// Zone of `start`, `due` and `recurrence_id`: an IANA name or `/<TZID>`
    /// naming an entry of `time_zones`. `None` means floating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    /// Custom zones: `"/<TZID>"` → POSIX TZ string, as
    /// [`crate::event::CalendarEvent::time_zones`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub time_zones: BTreeMap<String, String>,
    /// Dates only (iCalendar `VALUE=DATE`; always `true` for Google Tasks).
    #[serde(default)]
    pub show_without_time: bool,
    /// `DURATION`: expected effort, not the span between `start` and `due`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_duration: Option<Duration>,
    /// `STATUS` / Google `status`.
    #[serde(default)]
    pub progress: TaskProgress,
    /// When `progress` last changed: `COMPLETED` / Google `completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_updated: Option<Timestamp>,
    /// `PERCENT-COMPLETE`, 0–100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_complete: Option<u8>,
    /// Extension, not JSCalendar: Google Tasks `hidden` (cleared completed
    /// task). Always `false` from iCalendar.
    #[serde(default)]
    pub hidden: bool,
    /// `PRIORITY`, JSCalendar scale: 0 undefined, 1 highest … 9 lowest.
    #[serde(default)]
    pub priority: u8,
    /// UID of the parent task: `RELATED-TO;RELTYPE=PARENT` / Google `parent`.
    /// Deviation: a single UID, not a JSCalendar `relatedTo` map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_to: Option<String>,
    /// Recurrence rules, as on an event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recurrence_rules: Vec<RecurrenceRule>,
    /// Overrides keyed by recurrence id, as on an event.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recurrence_overrides: BTreeMap<DateTime, PatchObject>,
    /// Reminders by id; `relative_to: end` means relative to `due`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub alerts: BTreeMap<String, Alert>,
    /// Physical locations by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub locations: BTreeMap<String, Location>,
    /// `CATEGORIES`. Deviation: an array, not a map to `true`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub keywords: BTreeSet<String>,
    /// `CREATED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<Timestamp>,
    /// `LAST-MODIFIED` / Google `updated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<Timestamp>,
    /// `SEQUENCE`.
    #[serde(default)]
    pub sequence: u32,
}

impl CalendarTask {
    /// An untitled, undated task in `needs-action` with every optional field
    /// empty or at its default.
    pub fn new(uid: impl Into<String>) -> Self {
        CalendarTask {
            uid: uid.into(),
            recurrence_id: None,
            title: String::new(),
            description: String::new(),
            start: None,
            due: None,
            time_zone: None,
            time_zones: BTreeMap::new(),
            show_without_time: false,
            estimated_duration: None,
            progress: TaskProgress::default(),
            progress_updated: None,
            percent_complete: None,
            hidden: false,
            priority: 0,
            related_to: None,
            recurrence_rules: Vec::new(),
            recurrence_overrides: BTreeMap::new(),
            alerts: BTreeMap::new(),
            locations: BTreeMap::new(),
            keywords: BTreeSet::new(),
            created: None,
            updated: None,
            sequence: 0,
        }
    }
}

/// JSCalendar task `progress`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TaskProgress {
    /// Not started (default; Google `needsAction`).
    #[default]
    NeedsAction,
    /// In progress.
    InProcess,
    /// Done (Google `completed`).
    Completed,
    /// Could not be done.
    Failed,
    /// Will not be done.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_shape() {
        let mut t = CalendarTask::new("t1");
        t.title = "Buy milk".into();
        t.due = Some("2026-11-20T00:00:00".parse().unwrap());
        t.show_without_time = true;
        t.progress = TaskProgress::InProcess;
        t.related_to = Some("t0".into());
        let value = serde_json::to_value(&t).unwrap();
        assert_eq!(
            value,
            json!({
                "uid": "t1",
                "title": "Buy milk",
                "due": "2026-11-20T00:00:00",
                "showWithoutTime": true,
                "progress": "in-process",
                "hidden": false,
                "priority": 0,
                "relatedTo": "t0",
                "sequence": 0
            })
        );
        assert_eq!(serde_json::from_value::<CalendarTask>(value).unwrap(), t);
    }
}
