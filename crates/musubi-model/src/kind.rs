//! Kinds of records.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::contact::{ContactCard, ContactGroupCard};
use crate::event::CalendarEvent;
use crate::task::CalendarTask;

/// Runtime tag of a kind, stored next to every item and entity.
///
/// New kinds are added as variants, which is why matches outside this crate need
/// a wildcard arm. The kind belongs to the object, not to the collection: a
/// CalDAV calendar may mix events and tasks and an address book mixes contacts
/// and groups (docs/plan/phase2.md, chapter 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EntityKind {
    /// A calendar event, canonical form [`CalendarEvent`].
    Event,
    /// A task (to-do), canonical form [`CalendarTask`].
    Task,
    /// A contact card of an individual, organisation or location, canonical
    /// form [`ContactCard`].
    Contact,
    /// A contact group, canonical form [`ContactGroupCard`].
    ContactGroup,
}

impl EntityKind {
    /// Stable lowercase name, used in storage and in the API.
    pub const fn as_str(self) -> &'static str {
        match self {
            EntityKind::Event => "event",
            EntityKind::Task => "task",
            EntityKind::Contact => "contact",
            EntityKind::ContactGroup => "contact_group",
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Compile-time description of a kind: its tag and its canonical type.
///
/// Implemented only in this crate (one marker type per kind). Generic code such
/// as [`crate::provider::Codec`] is parameterised by it.
pub trait Kind: Send + Sync + 'static {
    /// The runtime tag.
    const KIND: EntityKind;
    /// Version of the canonical schema. Stored with every decoded item; when it
    /// changes, items are re-decoded from the raw layer (proposal 2.2), never migrated
    /// in place (except multi-placement entities, phase 3).
    const SCHEMA_VERSION: u32;
    /// The canonical document of this kind.
    type Canonical: Serialize
        + DeserializeOwned
        + Clone
        + fmt::Debug
        + PartialEq
        + Send
        + Sync
        + 'static;
}

/// The event kind. Uninhabited: it is a type-level marker only, used as
/// `Codec<Event>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {}

impl Kind for Event {
    const KIND: EntityKind = EntityKind::Event;
    /// 2 since phase 2: custom time zones in [`CalendarEvent::time_zones`].
    const SCHEMA_VERSION: u32 = 2;
    type Canonical = CalendarEvent;
}

/// The task kind, used as `Codec<Task>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {}

impl Kind for Task {
    const KIND: EntityKind = EntityKind::Task;
    const SCHEMA_VERSION: u32 = 1;
    type Canonical = CalendarTask;
}

/// The contact kind, used as `Codec<Contact>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contact {}

impl Kind for Contact {
    const KIND: EntityKind = EntityKind::Contact;
    const SCHEMA_VERSION: u32 = 1;
    type Canonical = ContactCard;
}

/// The contact group kind, used as `Codec<ContactGroup>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactGroup {}

impl Kind for ContactGroup {
    const KIND: EntityKind = EntityKind::ContactGroup;
    const SCHEMA_VERSION: u32 = 1;
    type Canonical = ContactGroupCard;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_names_match_serde() {
        for k in [
            EntityKind::Event,
            EntityKind::Task,
            EntityKind::Contact,
            EntityKind::ContactGroup,
        ] {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
    }
}
