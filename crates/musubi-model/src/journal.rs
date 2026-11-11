//! The change journal: what `/v1/changes` and the SSE stream deliver.

use serde::{Deserialize, Serialize};

use crate::id::{CollectionId, EntityId, PlacementId, Seq};

/// One journal entry. Written by the tenant store in the same transaction as
/// the change it describes; one entry per changed target per transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Position; strictly increasing, never reused.
    pub seq: Seq,
    /// What changed.
    pub target: ChangeTarget,
    /// How it changed.
    pub op: ChangeOp,
}

/// The changed object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum ChangeTarget {
    /// An entity's canonical view or occurrences.
    Entity(EntityId),
    /// A placement's state (delivery, phase 3).
    Placement(PlacementId),
    /// A collection's metadata or sync state.
    Collection(CollectionId),
}

/// Kind of change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    /// First seen.
    Created,
    /// Changed.
    Updated,
    /// Tombstoned.
    Deleted,
}

/// A page of [`crate::store::TenantStore::changes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesPage {
    /// Entries with `seq > since`, ascending, at most `limit`.
    pub changes: Vec<Change>,
    /// Pass as `since` for the next page: the last returned `seq`, or `since`
    /// unchanged when the page is empty.
    pub next: Seq,
    /// More entries exist after `next` (at the time of the read).
    pub more: bool,
}
