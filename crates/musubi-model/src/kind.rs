//! Kinds of records.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::event::CalendarEvent;

/// Runtime tag of a kind, stored next to every item and entity.
///
/// Phase 1 knows only events. New kinds (tasks, contacts) are added as variants,
/// which is why matches outside this crate need a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EntityKind {
    /// A calendar event, canonical form [`CalendarEvent`].
    Event,
}

impl EntityKind {
    /// Stable lowercase name, used in storage and in the API.
    pub const fn as_str(self) -> &'static str {
        match self {
            EntityKind::Event => "event",
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
    const SCHEMA_VERSION: u32 = 1;
    type Canonical = CalendarEvent;
}
