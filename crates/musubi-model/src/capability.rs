//! Provider capabilities (proposal 6.2), exposed in the API so applications can
//! hide a feature before the user tries it.
//!
//! Phases 1 and 2 only read, but the descriptor reports write capabilities
//! from the start. Values are facts about the provider, verified in phase 0
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
///
/// Non-exhaustive: build it with [`ProviderDescriptor::new`] and set the other
/// fields by assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderDescriptor {
    /// Provider name.
    pub id: ProviderId,
    /// Supported kinds with their capabilities, including the default delta
    /// and push mechanism of each.
    pub kinds: Vec<KindCapabilities>,
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

impl ProviderDescriptor {
    /// A descriptor that claims nothing beyond `kinds` and `limits`: no
    /// recurrence, no zones, no scheduling, no field fidelity, no hidden
    /// marker, no reminders, no tasks.
    pub fn new(id: ProviderId, kinds: Vec<KindCapabilities>, limits: Limits) -> Self {
        ProviderDescriptor {
            id,
            kinds,
            recurrence: RecurrenceSupport::None,
            zones: Vec::new(),
            scheduling: SchedulingCapabilities::new(
                SchedulingMode::None,
                InviteSuppression::None,
                false,
            ),
            field_fidelity: BTreeMap::new(),
            hidden_marker: false,
            reminders: ReminderCapabilities::new(false, false, Vec::new(), None),
            tasks: None,
            limits,
        }
    }
}

/// Capabilities for one kind.
///
/// Non-exhaustive: build it with [`KindCapabilities::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KindCapabilities {
    /// The kind.
    pub kind: EntityKind,
    /// How changes of collections of this kind are found, unless the
    /// collection says otherwise ([`crate::provider::RemoteCollection::delta`]).
    pub delta: DeltaSupport,
    /// How changes of collections of this kind are announced, unless the
    /// collection says otherwise ([`crate::provider::RemoteCollection::push`]).
    pub push: PushSupport,
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

impl KindCapabilities {
    /// A readable, not writable kind with unconditional writes and no
    /// client-chosen ids.
    pub fn new(kind: EntityKind, delta: DeltaSupport, push: PushSupport) -> Self {
        KindCapabilities {
            kind,
            delta,
            push,
            read: true,
            write: false,
            conditional_update: ConditionalWrite::None,
            conditional_delete: ConditionalWrite::None,
            client_id: None,
        }
    }
}

/// Strength of conditional writes (`If-Match`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct ClientIdRule {
    /// Allowed characters.
    pub charset: ClientIdCharset,
    /// Minimum length in characters.
    pub min_len: u16,
    /// Maximum length in characters.
    pub max_len: u16,
}

impl ClientIdRule {
    /// A rule with these characters and length bounds.
    pub fn new(charset: ClientIdCharset, min_len: u16, max_len: u16) -> Self {
        ClientIdRule {
            charset,
            min_len,
            max_len,
        }
    }
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
#[non_exhaustive]
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
#[non_exhaustive]
pub enum PushSupport {
    /// HTTPS callback; [`crate::provider::Provider::push`] returns a [`crate::provider::PushSource`].
    Webhook,
    /// Polling only.
    None,
}

/// Recurrence support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
pub struct SchedulingCapabilities {
    /// Who delivers invitations.
    pub mode: SchedulingMode,
    /// Whether a write can avoid sending invitations.
    pub invite_suppression: InviteSuppression,
    /// The organizer's copy can record an attendee's reply.
    pub organizer_writes_reply: bool,
}

impl SchedulingCapabilities {
    /// Scheduling capabilities with these values.
    pub fn new(
        mode: SchedulingMode,
        invite_suppression: InviteSuppression,
        organizer_writes_reply: bool,
    ) -> Self {
        SchedulingCapabilities {
            mode,
            invite_suppression,
            organizer_writes_reply,
        }
    }
}

/// Who delivers invitations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
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

impl ReminderCapabilities {
    /// Reminder capabilities with these values.
    pub fn new(
        relative: bool,
        absolute: bool,
        actions: Vec<AlertAction>,
        max_per_item: Option<u32>,
    ) -> Self {
        ReminderCapabilities {
            relative,
            absolute,
            actions,
            max_per_item,
        }
    }
}

/// Task capabilities; `None` in [`ProviderDescriptor::tasks`] when the provider
/// has no task kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TaskCapabilities {
    /// Due is a date only; a time is dropped (Google Tasks).
    pub due_date_only: bool,
    /// Subtasks are supported.
    pub subtasks: bool,
    /// Recurring tasks are exposed.
    pub recurrence: bool,
}

impl TaskCapabilities {
    /// Task capabilities with these values.
    pub fn new(due_date_only: bool, subtasks: bool, recurrence: bool) -> Self {
        TaskCapabilities {
            due_date_only,
            subtasks,
            recurrence,
        }
    }
}

/// Paging, batching, concurrency and rate limits.
///
/// Non-exhaustive: build it with [`Limits::new`] and set the other fields by
/// assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Limits {
    /// Largest page the provider returns (Google events: 2500). A [`crate::provider::ChangePage`]
    /// never holds more objects plus stubs than this.
    pub max_page_size: u32,
    /// Requests per minute per user, if documented (Google: 600).
    pub requests_per_minute: Option<u32>,
    /// Most refs the engine passes to one [`crate::provider::Provider::fetch`]
    /// call. How the adapter splits that call further (HTTP batch, one DAV
    /// `multiget`) is its own business. `None`: the engine uses 100. Google
    /// 1000, generic DAV 50, iCloud 20.
    pub max_batch: Option<u32>,
    /// Most requests the engine runs at once for one account (all its
    /// collections together). `None`: no limit (Google). DAV and iCloud: 2.
    #[serde(default)]
    pub max_concurrency_per_account: Option<u32>,
}

impl Limits {
    /// Limits with this page size and no rate, batch or concurrency limit.
    pub fn new(max_page_size: u32) -> Self {
        Limits {
            max_page_size,
            requests_per_minute: None,
            max_batch: None,
            max_concurrency_per_account: None,
        }
    }
}
