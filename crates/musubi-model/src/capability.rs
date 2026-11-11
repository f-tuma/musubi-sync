//! Provider capabilities (proposal 6.2), exposed in the API so applications can
//! hide a feature before the user tries it.
//!
//! Phase 1 only reads, but the descriptor reports write capabilities from the
//! start. Values are facts about the provider, verified in phase 0
//! (docs/research/phase0/providers.md) or marked `Unproven`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::event::AlertAction;
use crate::kind::EntityKind;

/// Stable name of a provider (`"google"`), stored with each account.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

/// What a provider can do. Returned by [`crate::provider::Provider::descriptor`];
/// constant for the lifetime of the adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    /// Provider name.
    pub id: ProviderId,
    /// Supported kinds with read/write capabilities each.
    pub kinds: Vec<KindCapabilities>,
    /// How changes are found.
    pub delta: DeltaSupport,
    /// How changes are announced.
    pub push: PushSupport,
    /// How much of RFC 5545 recurrence round-trips.
    pub recurrence: RecurrenceSupport,
    /// Time-zone identifier systems the provider accepts and returns.
    pub zones: Vec<ZoneSupport>,
    /// Invitations and replies.
    pub scheduling: SchedulingCapabilities,
    /// Fidelity of each shared canonical field when an entity is placed here
    /// (proposal 12.8), keyed by JSCalendar property name (`"title"`, `"alerts"`).
    /// A field not listed is [`Fidelity::Missing`].
    pub field_fidelity: BTreeMap<String, Fidelity>,
    /// The provider has a hidden field that can carry our entity marker.
    pub hidden_marker: bool,
    /// Reminders.
    pub reminders: ReminderCapabilities,
    /// Tasks; `None` when the provider has no task kind.
    pub tasks: Option<TaskCapabilities>,
    /// Paging and rate limits the adapter respects.
    pub limits: Limits,
}

/// Capabilities for one kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindCapabilities {
    /// The kind.
    pub kind: EntityKind,
    /// Items of this kind can be read.
    pub read: bool,
    /// Items of this kind can be written (phase 3).
    pub write: bool,
    /// Whether updates can be made conditional on the stored validator.
    pub conditional_update: ConditionalWrite,
    /// Whether deletes can be made conditional on the stored validator.
    pub conditional_delete: ConditionalWrite,
    /// Create with a client-chosen remote id; `None` if not supported.
    pub client_id: Option<ClientIdRule>,
}

/// Strength of conditional writes (`If-Match`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionalWrite {
    /// The provider rejects a write with a stale validator (`412`), verified.
    StrongValidator,
    /// The provider accepts a validator but enforcement is not proven.
    Unproven,
    /// Writes are unconditional.
    None,
}

/// Format the provider requires for client-chosen remote ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientIdRule {
    /// Allowed characters.
    pub charset: ClientIdCharset,
    /// Minimum length in characters.
    pub min_len: u16,
    /// Maximum length in characters.
    pub max_len: u16,
}

/// Character sets of [`ClientIdRule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientIdCharset {
    /// Lowercase base32hex, `0-9a-v` (Google Calendar event ids). Our 128-bit ids
    /// in text form (26 characters) satisfy it.
    Base32HexLower,
}

/// Delta mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaSupport {
    /// Server sync token (Google `syncToken`, DAV sync-collection).
    SyncToken,
    /// Changes since a time with an overlapping window (Google Tasks `updatedMin`).
    WindowedDelta,
    /// Compare collection tag, then validators of all objects (plain CalDAV).
    CtagEtag,
    /// Full reads only.
    None,
}

/// Push mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushSupport {
    /// HTTPS callback; [`crate::provider::Provider::push`] returns a [`crate::provider::PushSource`].
    Webhook,
    /// Polling only.
    None,
}

/// Recurrence support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecurrenceSupport {
    /// Any RFC 5545 rule round-trips.
    Full,
    /// Only finite or restricted rules.
    Finite,
    /// Series can be read, not written.
    ReadOnly,
    /// No recurrence.
    None,
}

/// Time-zone identifier systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneSupport {
    /// IANA names (`Europe/Prague`).
    Iana,
    /// Windows zone ids (`Central Europe Standard Time`).
    WindowsIds,
    /// Custom `VTIMEZONE` definitions.
    CustomVtimezone,
}

/// Invitations and replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SchedulingCapabilities {
    /// Who delivers invitations.
    pub mode: SchedulingMode,
    /// Whether a write can avoid sending invitations.
    pub invite_suppression: InviteSuppression,
    /// The organizer's copy can record an attendee's reply.
    pub organizer_writes_reply: bool,
}

/// Who delivers invitations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingMode {
    /// The provider sends them (Google, Graph).
    ProviderSide,
    /// The core's own inbox (proposal 14).
    CoreInbox,
    /// No scheduling.
    None,
}

/// Invitation suppression on write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InviteSuppression {
    /// Not possible.
    None,
    /// For the whole request (Google `sendUpdates=none`).
    WholeRequest,
    /// Per attendee.
    PerAttendee,
}

/// Fidelity of one field on this provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Round-trips unchanged.
    Exact,
    /// Stored with loss (e.g. absolute alerts made relative).
    Approximate,
    /// Not stored.
    Missing,
}

/// Reminder capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderCapabilities {
    /// Offsets relative to start are supported.
    pub relative: bool,
    /// Absolute instants are supported.
    pub absolute: bool,
    /// Supported delivery actions.
    pub actions: Vec<AlertAction>,
    /// Maximum reminders per item, if limited.
    pub max_per_item: Option<u32>,
}

/// Task capabilities (no task kind in phase 1; reported for completeness).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskCapabilities {
    /// Due is a date only; a time is dropped (Google Tasks).
    pub due_date_only: bool,
    /// Subtasks are supported.
    pub subtasks: bool,
    /// Recurring tasks are exposed.
    pub recurrence: bool,
}

/// Paging and rate limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Limits {
    /// Largest page the provider returns (Google events: 2500). A [`crate::provider::ChangePage`]
    /// never holds more objects than this.
    pub max_page_size: u32,
    /// Requests per minute per user, if documented (Google: 600).
    pub requests_per_minute: Option<u32>,
    /// Calls per batch request, if batching exists (Google: 1000).
    pub max_batch: Option<u32>,
}
