//! Tenant storage (docs/plan/phase1.md, chapter 3.3).
//!
//! A narrow interface the engine is written against. It contains no merge or
//! scheduling logic: the single tenant writer reads state, decides, and writes
//! the result with one operation. Every write operation is one transaction:
//! after an error nothing of it is visible.
//!
//! Invariants every implementation must keep (checked by the contract tests of K1):
//!
//! 1. A page is written whole, with its cursor, or not at all.
//! 2. A repeated page with the same validators changes no item, entity or
//!    journal entry and does not advance [`Seq`]; it only records that a full
//!    read saw the items (`PageWrite::read`).
//! 3. `Seq` strictly increases and is never reused, not after `prune`, not after a crash.
//! 4. While the full read `r` is active ([`CollectionState::active_read`] is
//!    `Some(r)`), every page written to the collection, whatever its
//!    `PageWrite::read`, marks the keys of its `upserts`, `unchanged` and
//!    `removals` as seen by `r`. So delta pages written between the last page
//!    of the read and `finish_full_read` (a crash, a finish held back by the
//!    mass-deletion guard) count as seen. `finish_full_read(col, r)` is
//!    accepted only after the page with `last` of `r`
//!    ([`CollectionState::read_complete`]) and tombstones only live items of
//!    `col` not seen by `r`.
//! 5. One item per (collection, remote key); one entity per item and one
//!    placement per entity (phase 1).
//! 6. `range` returns occurrences sorted by (`start`, `entity`, `recurrence_id`),
//!    paged, without duplicates across page boundaries.
//!
//! Synchronous on purpose: implementations block on disk; async callers use
//! `spawn_blocking` or a dedicated thread per tenant writer.
//!
//! Restore is not an operation: `musubi-syncd restore` is an offline command
//! that must not run against an open tenant.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::value::RawValue;

use crate::error::StoreError;
use crate::id::{AccountId, CollectionId, EntityId, ItemId, PlacementId, ReadId, Seq, TenantId};
use crate::journal::ChangesPage;
use crate::kind::EntityKind;
use crate::placement::{Delivery, Direction, Origin, Projection, Role};
use crate::provider::{Pass, RemoteCollection};
use crate::remote::{Cursor, Raw, RawHash, RemoteKey, Validator};
use crate::time::TimeWindow;

/// A canonical document as stored: validated JSON text, passed through to the
/// API without re-parsing.
pub type CanonicalJson = Box<RawValue>;

/// Factory and life cycle of tenant stores. Implemented by `musubi-store-sqlite`
/// (later PostgreSQL). Called by `musubi-syncd` (admin calls, backup) and by the
/// engine (`open`).
pub trait Storage: Send + Sync {
    /// Handle to one tenant. `'static` so the engine can move it into the
    /// tenant writer's thread or task.
    type Tenant: TenantStore + 'static;

    /// Opens a tenant, migrating its schema lazily if it is older.
    ///
    /// Cheap when the tenant is cached (LRU of open files). Many handles to one
    /// tenant may exist; only the tenant writer calls write operations.
    /// Errors: `NotFound` (no such tenant), `SchemaTooNew` (file from a newer
    /// binary; nothing is changed), `Io`/`Backend`.
    fn open(&self, t: TenantId) -> Result<Self::Tenant, StoreError>;

    /// Creates an empty tenant (files, schema). `AlreadyExists` if present.
    fn create_tenant(&self, t: TenantId) -> Result<(), StoreError>;

    /// Closes and deletes all files of the tenant, including `outbox.db` and
    /// WAL files. `NotFound` if absent.
    fn delete_tenant(&self, t: TenantId) -> Result<(), StoreError>;

    /// Consistent online snapshot of the tenant into the directory `dest`
    /// (backup API, not a file copy): first `main.db`, then `outbox.db` (phase 3).
    /// Writes may continue meanwhile. `dest` must exist and be empty.
    fn backup(&self, t: TenantId, dest: &Path) -> Result<BackupInfo, StoreError>;
}

/// One tenant's data plane (`main.db`).
///
/// Reads take `&self`, run concurrently with the writer and use short read
/// transactions; results are bounded (`limit`) or paged. Writes take `&mut self`
/// and are called only by the tenant writer of the engine.
pub trait TenantStore: Send {
    /// Last committed journal position (`Seq(0)` for an empty tenant).
    fn head(&self) -> Result<Seq, StoreError>;

    /// All collections of the tenant, including those marked gone.
    fn collections(&self) -> Result<Vec<CollectionState>, StoreError>;

    /// An entity; `None` if it never existed (a deleted one is returned with
    /// `tombstone` until pruned).
    fn entity(&self, id: EntityId, inc: Include) -> Result<Option<EntityView>, StoreError>;

    /// An item, with its raw content if `inc.raw`.
    fn item(&self, id: ItemId, inc: Include) -> Result<Option<ItemView>, StoreError>;

    /// State of the items with these keys in `col`, for the writer's decision
    /// "unchanged / changed / new". Keys without an item are left out; order unspecified.
    fn items_by_remote(
        &self,
        col: CollectionId,
        keys: &[RemoteKey],
    ) -> Result<Vec<ItemState>, StoreError>;

    /// Occurrences overlapping `q.window`, one page.
    fn range(&self, q: &RangeQuery) -> Result<RangePage, StoreError>;

    /// Journal entries with `seq > since`, at most `limit`.
    /// `CursorTooOld` if `since` is older than the pruned journal.
    fn changes(&self, since: Seq, limit: u32) -> Result<ChangesPage, StoreError>;

    /// Live items of `col` not seen by the full read `read` so far: how many
    /// `finish_full_read` would delete. Used by the mass-deletion guard.
    fn unseen_count(&self, col: CollectionId, read: ReadId) -> Result<u64, StoreError>;

    /// Keys of the live items of `col` not seen by the full read `read`, in key
    /// order, starting after `after`, at most `limit` (keyset paging).
    ///
    /// The mass-deletion guard re-reads them with
    /// [`crate::provider::Provider::fetch`]. Objects that still exist are
    /// marked seen with `apply_page` (`read: Some(read)`, `cursor: None`,
    /// `last: false`, the keys in `unchanged` or, if changed, in `upserts`);
    /// the rest are deleted by `finish_full_read`.
    fn unseen(
        &self,
        col: CollectionId,
        read: ReadId,
        after: Option<&RemoteKey>,
        limit: u32,
    ) -> Result<Vec<RemoteKey>, StoreError>;

    /// Series whose occurrence index ends before `until` and that continue past
    /// it, oldest index first, at most `limit`. Used to move the index horizon.
    fn series_to_extend(&self, until: Timestamp, limit: u32) -> Result<Vec<SeriesRef>, StoreError>;

    /// Size and checkpoint metrics (phase 0 condition 8).
    fn stats(&self) -> Result<StoreStats, StoreError>;

    /// Records the result of discovery for one account: adds new collections
    /// (assigning [`CollectionId`]s), updates changed ones and, only if
    /// `d.complete`, marks the account's collections missing from the list as
    /// gone. Journal entries for changed collections.
    fn apply_discovery(&mut self, d: &DiscoveryWrite) -> Result<(), StoreError>;

    /// Writes one ingested page and its cursor in one transaction: raw bodies
    /// (by hash, existing hash = no write), items, entities and placements
    /// (new ones get fresh ids), occurrences, journal and cursor. Returns the
    /// head after the commit (unchanged if nothing changed).
    ///
    /// Full-read bookkeeping (invariant 4), in this order:
    /// - `p.read == Some(r)` with no active read starts `r` (it must be greater
    ///   than `last_read`); with an active read `a < r` it replaces `a`, whose
    ///   seen-marks are dropped (a full read restarted after `CursorExpired`).
    ///   `r` older than the active read or not above `last_read` is
    ///   `StaleRead`, and nothing is written.
    /// - While a read is active, all keys of the page are marked seen by it,
    ///   whatever `p.read` says.
    /// - `p.last` on a page of the active read sets
    ///   [`CollectionState::read_complete`].
    ///
    /// `NotFound` if the collection does not exist.
    fn apply_page(&mut self, p: &PageWrite) -> Result<Seq, StoreError>;

    /// Ends the full read `read` of `col`: tombstones live items it did not see
    /// (their entities and occurrences with them), clears
    /// [`CollectionState::active_read`] and `read_complete` and sets
    /// `last_read`. Returns the head. `StaleRead` if `read` is not the active
    /// read or its last page has not been written.
    ///
    /// Phase 1 only: with one placement per entity, an absent item tombstones
    /// its entity. With several placements (phase 3 and 4) absence detaches
    /// the placement and the entity stays (proposal 12.7).
    fn finish_full_read(&mut self, col: CollectionId, read: ReadId) -> Result<Seq, StoreError>;

    /// Drops a collection the engine no longer syncs (gone from a complete
    /// discovery and confirmed, or its account is being removed): tombstones
    /// all its items with their entities and occurrences, writes the journal
    /// entries and deletes the collection. Its [`CollectionId`] is never reused.
    /// Returns the head.
    fn purge_collection(&mut self, col: CollectionId) -> Result<Seq, StoreError>;

    /// Sets the engine-maintained status of a collection (coverage, last error).
    fn set_collection_state(
        &mut self,
        col: CollectionId,
        s: &CollectionStatus,
    ) -> Result<(), StoreError>;

    /// Appends occurrences to series whose index horizon moved. An extension
    /// whose `from` no longer matches the stored horizon (the series was
    /// re-indexed meanwhile) is skipped.
    fn extend_occurrences(&mut self, w: &OccurrenceWrite) -> Result<(), StoreError>;

    /// Removes tombstones and journal entries older than `older_than`, and raw
    /// bodies no item refers to any more, whatever their age. Never makes `Seq`
    /// values reusable.
    fn prune(&mut self, older_than: Timestamp) -> Result<PruneInfo, StoreError>;

    /// Runs a WAL checkpoint (the store's own checkpointer, phase 0 condition 4).
    fn checkpoint(&mut self, mode: CheckpointMode) -> Result<CheckpointInfo, StoreError>;

    // Phase 3 (outbox file), listed only for the shape; see docs/plan/phase1.md 3.3:
    // accept, unprojected, insert_deliveries, project, due_deliveries, record_outcome.
}

/// What to include in a view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Include {
    /// Placements of an entity.
    pub placements: bool,
    /// Raw content of an item.
    pub raw: bool,
}

/// A collection as stored.
#[derive(Debug, Clone)]
pub struct CollectionState {
    /// Our id.
    pub id: CollectionId,
    /// Owning account.
    pub account: AccountId,
    /// Last discovered metadata.
    pub remote: RemoteCollection,
    /// Stored resume position (mid-pass during a full read, otherwise the delta cursor).
    pub cursor: Option<Cursor>,
    /// Full read in progress: started by a page with `read: Some(id)`,
    /// `finish_full_read` not yet called.
    pub active_read: Option<ReadId>,
    /// The page with `last` of `active_read` has been written: all pages are
    /// in, only `finish_full_read` is pending (possibly held by the
    /// mass-deletion guard). After a restart the engine finishes the read if
    /// this is set, and otherwise resumes it with `cursor` and `active_read`.
    pub read_complete: bool,
    /// Last finished full read. A new read uses a larger id.
    pub last_read: Option<ReadId>,
    /// Engine-maintained status.
    pub status: CollectionStatus,
    /// Missing from the last complete discovery. Its items stay until the
    /// engine drops it with [`TenantStore::purge_collection`].
    pub gone: bool,
}

/// Engine-maintained status of a collection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectionStatus {
    /// What part of the collection has been read.
    pub coverage: Coverage,
    /// Last error, cleared on success.
    pub error: Option<CollectionError>,
    /// End of the last successful read of any pass (proposal 3.1).
    pub synced_at: Option<Timestamp>,
    /// End of the last finished full read.
    pub full_read_at: Option<Timestamp>,
}

/// What part of a collection has been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Coverage {
    /// Nothing yet.
    #[default]
    None,
    /// Only the initial window.
    Window(TimeWindow),
    /// A full read finished; deltas keep it current.
    Full,
}

/// Last error of a collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionError {
    /// Human-readable message (no tokens, no personal data).
    pub message: String,
    /// When it happened.
    pub at: Timestamp,
    /// From [`crate::error::SyncError::Permanent`]: not retried automatically.
    pub permanent: bool,
}

/// Input of [`TenantStore::apply_discovery`].
#[derive(Debug, Clone)]
pub struct DiscoveryWrite {
    /// The account the list belongs to.
    pub account: AccountId,
    /// The listed collections, matched to stored ones by `remote`.
    pub collections: Vec<RemoteCollection>,
    /// The list is complete ([`crate::provider::Provider::discover`] returned `Ok`).
    pub complete: bool,
}

/// Input of [`TenantStore::apply_page`]: the writer's decisions for one page.
/// Everything expensive (decode, hash, occurrence expansion) is computed before.
#[derive(Debug, Clone)]
pub struct PageWrite {
    /// Target collection.
    pub collection: CollectionId,
    /// Pass of the page.
    pub pass: Pass,
    /// `Some` on pages of a full read (and on the guard's re-marking pages):
    /// starts or continues that read. Pages written while a read is active are
    /// marked seen by it even with `None` ([`TenantStore::apply_page`]).
    pub read: Option<ReadId>,
    /// The page ends its pass ([`crate::provider::ChangePage::last`]). On a
    /// page of the active full read it sets [`CollectionState::read_complete`];
    /// `false` never clears it.
    pub last: bool,
    /// New cursor, stored in the same transaction. `None` keeps the stored one.
    pub cursor: Option<Cursor>,
    /// New or changed objects.
    pub upserts: Vec<ItemWrite>,
    /// Keys received with an unchanged validator or raw hash: nothing is written
    /// for them except the seen-mark of `read`.
    pub unchanged: Vec<RemoteKey>,
    /// Keys the provider reported deleted: their items, entities and
    /// occurrences are tombstoned. Unknown keys are ignored.
    pub removals: Vec<RemoteKey>,
}

/// One new or changed item of a page.
#[derive(Debug, Clone)]
pub struct ItemWrite {
    /// Remote key; matched with an existing item of the collection or inserted.
    pub key: RemoteKey,
    /// Validator received with `raw`.
    pub validator: Validator,
    /// Raw content.
    pub raw: Raw,
    /// [`Raw::hash`] of `raw`, computed before the transaction.
    pub raw_hash: RawHash,
    /// Kind of the item.
    pub kind: EntityKind,
    /// Canonical view (`observed`); `None` if the codec returned `Unsupported`.
    pub canonical: Option<CanonicalJson>,
    /// [`crate::kind::Kind::SCHEMA_VERSION`] of `canonical`.
    pub schema_version: u32,
    /// [`crate::provider::Codec::MAPPING_VERSION`] that produced `canonical`.
    pub mapping_version: u32,
    /// Correlation key, used to suppress a master occurrence replaced by a
    /// separate exception item of the same series in the same collection.
    pub correlation: Option<Correlation>,
    /// The occurrence index of the item, replacing the previous one.
    pub occurrences: Occurrences,
}

/// iCalendar UID + RECURRENCE-ID (proposal 2.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Correlation {
    /// UID.
    pub uid: String,
    /// Set for a single instance of a series: the replaced occurrence, in the
    /// convention of [`OccurrenceRow::recurrence_id`].
    pub recurrence_id: Option<Timestamp>,
}

/// Occurrence index rows of one entity.
#[derive(Debug, Clone, Default)]
pub struct Occurrences {
    /// Rows up to `series.indexed_until` (or the single occurrence).
    pub rows: Vec<OccurrenceRow>,
    /// Set for a series: its span and index horizon.
    pub series: Option<SeriesSpan>,
}

/// One occurrence in the index `(start_utc, end_utc, entity_id, recurrence_id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccurrenceRow {
    /// Start. For floating and all-day occurrences: the local wall time read as UTC.
    pub start: Timestamp,
    /// End (`>= start`), same convention.
    pub end: Timestamp,
    /// Occurrence of a series: its recurrence id, the original start of the
    /// occurrence as an instant. For floating and all-day series the local
    /// wall time read as UTC, as in `start`. Comparable across zones.
    pub recurrence_id: Option<Timestamp>,
    /// Floating or all-day: the store widens the search by ±14 hours and filters
    /// exactly with [`RangeQuery::zone`].
    pub floating: bool,
}

/// Span of a series (proposal 2.5, "index of series spans").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesSpan {
    /// Start of the first occurrence.
    pub first: Timestamp,
    /// End of the last occurrence; `None` if unbounded.
    pub last: Option<Timestamp>,
    /// Rows exist for all occurrences starting before this instant.
    pub indexed_until: Timestamp,
}

/// A series whose index has to be extended (or expanded at query time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesRef {
    /// The series' entity.
    pub entity: EntityId,
    /// Current index horizon.
    pub indexed_until: Timestamp,
}

/// Input of [`TenantStore::extend_occurrences`].
#[derive(Debug, Clone, Default)]
pub struct OccurrenceWrite {
    /// One extension per series.
    pub extensions: Vec<SeriesExtension>,
}

/// New index rows for one series.
#[derive(Debug, Clone)]
pub struct SeriesExtension {
    /// The series' entity.
    pub entity: EntityId,
    /// The horizon the rows were computed from; must equal the stored one.
    pub from: Timestamp,
    /// Rows starting in `[from, indexed_until)`.
    pub rows: Vec<OccurrenceRow>,
    /// New horizon.
    pub indexed_until: Timestamp,
}

/// Input of [`TenantStore::range`].
#[derive(Debug, Clone)]
pub struct RangeQuery {
    /// Occurrences overlapping this window.
    pub window: TimeWindow,
    /// Only entities placed in these collections; `None` means all. An entity in
    /// several of them is returned once.
    pub collections: Option<Vec<CollectionId>>,
    /// Zone in which floating and all-day occurrences are placed for filtering.
    pub zone: TimeZone,
    /// Continue after this position (from [`RangePage::next`]).
    pub after: Option<RangeCursor>,
    /// Page size, at least 1.
    pub limit: u32,
    /// Leave out canonical documents (the "ids only" variant).
    pub ids_only: bool,
}

/// Position in a range result: the sort key of the last returned occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RangeCursor {
    /// Index start of the last occurrence.
    pub start: Timestamp,
    /// Its entity.
    pub entity: EntityId,
    /// Its recurrence id.
    pub recurrence_id: Option<Timestamp>,
}

/// One page of [`TenantStore::range`].
#[derive(Debug, Clone)]
pub struct RangePage {
    /// Occurrences in sort order.
    pub occurrences: Vec<OccurrenceView>,
    /// `Some` if more may follow.
    pub next: Option<RangeCursor>,
    /// Series overlapping the window whose index ends before the window does.
    /// The caller expands them itself (`musubi-recurrence`). Only on the first
    /// page (`after: None`).
    pub unindexed: Vec<SeriesRef>,
}

/// One occurrence in a range result.
#[derive(Debug, Clone)]
pub struct OccurrenceView {
    /// The entity (the master for occurrences of a series).
    pub entity: EntityId,
    /// Recurrence id for occurrences of a series ([`OccurrenceRow::recurrence_id`]).
    pub recurrence_id: Option<Timestamp>,
    /// Start (index convention, see [`OccurrenceRow`]).
    pub start: Timestamp,
    /// End.
    pub end: Timestamp,
    /// Floating or all-day.
    pub floating: bool,
    /// The entity's canonical document; `None` with `ids_only` or without a view.
    pub canonical: Option<CanonicalJson>,
}

/// An entity.
#[derive(Debug, Clone)]
pub struct EntityView {
    /// Id.
    pub id: EntityId,
    /// Kind.
    pub kind: EntityKind,
    /// Revision, incremented by every change.
    pub rev: u64,
    /// Journal position of the last change.
    pub seq: Seq,
    /// Deleted.
    pub tombstone: bool,
    /// Canonical view (phase 1: `observed` of its only item).
    pub canonical: Option<CanonicalJson>,
    /// Placements if requested, otherwise empty.
    pub placements: Vec<PlacementView>,
}

/// A placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementView {
    /// Id.
    pub id: PlacementId,
    /// The item.
    pub item: ItemId,
    /// The item's collection.
    pub collection: CollectionId,
    /// Role.
    pub role: Role,
    /// Direction.
    pub direction: Direction,
    /// Projection.
    pub projection: Projection,
    /// Origin.
    pub origin: Origin,
    /// Delivery state.
    pub delivery: Delivery,
}

/// An item.
#[derive(Debug, Clone)]
pub struct ItemView {
    /// Id.
    pub id: ItemId,
    /// Its entity.
    pub entity: EntityId,
    /// Its collection.
    pub collection: CollectionId,
    /// Kind.
    pub kind: EntityKind,
    /// Remote key.
    pub key: RemoteKey,
    /// Validator.
    pub validator: Validator,
    /// Hash of the raw content.
    pub raw_hash: RawHash,
    /// Schema version of `observed`.
    pub schema_version: u32,
    /// Mapping version of `observed`.
    pub mapping_version: u32,
    /// Canonical view decoded from the raw content.
    pub observed: Option<CanonicalJson>,
    /// Deleted at the provider.
    pub tombstone: bool,
    /// Raw content if requested.
    pub raw: Option<Raw>,
}

/// Stored state of an item, for the writer's change detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemState {
    /// Remote key.
    pub key: RemoteKey,
    /// Item id.
    pub item: ItemId,
    /// Entity id.
    pub entity: EntityId,
    /// Stored validator.
    pub validator: Validator,
    /// Stored raw hash.
    pub raw_hash: RawHash,
    /// Schema version of the stored view.
    pub schema_version: u32,
    /// Mapping version of the stored view (re-decode if older).
    pub mapping_version: u32,
    /// Tombstoned.
    pub tombstone: bool,
}

/// Storage metrics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Journal head.
    pub head: Seq,
    /// Size of `main.db` in bytes.
    pub main_bytes: u64,
    /// Current WAL size in bytes.
    pub wal_bytes: u64,
    /// Largest WAL size seen since the tenant was opened.
    pub wal_max_bytes: u64,
    /// `TRUNCATE` checkpoints that reset the WAL.
    pub truncates: u64,
    /// Longest `TRUNCATE` checkpoint.
    pub truncate_max: Duration,
    /// `TRUNCATE` attempts that could not reset the WAL (readers active).
    pub reset_failures: u64,
    /// Live items.
    pub items: u64,
    /// Live entities.
    pub entities: u64,
}

/// Checkpoint mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointMode {
    /// Copy what is possible without blocking (background).
    Passive,
    /// Copy everything and reset the WAL; waits for readers at most briefly.
    Truncate,
}

/// Result of [`TenantStore::checkpoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointInfo {
    /// Mode that ran.
    pub mode: CheckpointMode,
    /// For `Truncate`: whether the WAL was reset.
    pub reset: bool,
    /// WAL size before, in bytes.
    pub wal_bytes_before: u64,
    /// Duration.
    pub took: Duration,
}

/// Result of [`TenantStore::prune`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneInfo {
    /// Journal entries removed.
    pub changes: u64,
    /// Tombstoned rows removed.
    pub tombstones: u64,
    /// Raw bodies removed.
    pub raw: u64,
    /// Oldest `since` [`TenantStore::changes`] still accepts.
    pub oldest: Seq,
}

/// Result of [`Storage::backup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupInfo {
    /// Files written, in the order they were snapshotted.
    pub files: Vec<PathBuf>,
    /// Total bytes.
    pub bytes: u64,
    /// Journal head contained in the snapshot.
    pub head: Seq,
}
