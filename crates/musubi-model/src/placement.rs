//! Placements: the binding of an entity to an item (proposal 12.2).
//!
//! In phase 1 every entity has exactly one placement with role `primary`,
//! direction `both`, projection `full`, origin `manual` and delivery `synced`.
//! The other values exist so that the API shape does not change in phase 3.

use serde::{Deserialize, Serialize};

/// Role of a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The authoritative copy.
    Primary,
    /// A copy we placed elsewhere.
    Copy,
    /// An equal peer of other placements.
    Peer,
}

/// Which way changes flow through a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Both ways.
    Both,
    /// Out of the core to the provider only.
    Out,
}

/// What of the entity a placement shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    /// Everything.
    Full,
    /// Only a busy block.
    BusyOnly,
}

/// Why a placement exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Observed or created by a user or application (all of phase 1).
    Manual,
    /// Created by a rule (proposal 13).
    Rule,
}

/// Delivery state of a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The provider has the current version.
    Synced,
    /// A change waits for delivery.
    Pending,
    /// The provider changed concurrently.
    Conflict,
    /// Delivery failed permanently.
    Failed,
    /// Sent without a known outcome (crash after send).
    Uncertain,
    /// The copy differs and is not reconciled.
    Diverged,
    /// Delivery paused.
    Suspended,
}
