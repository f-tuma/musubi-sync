//! [`TenantStore`] over `main.db`.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use musubi_model::capability::ProviderId;
use musubi_model::error::StoreError;
use musubi_model::id::{AccountId, CollectionId, EntityId, ItemId, PlacementId, ReadId, Seq};
use musubi_model::journal::{Change, ChangeOp, ChangeTarget, ChangesPage};
use musubi_model::kind::EntityKind;
use musubi_model::placement::{Delivery, Direction, Origin, Projection, Role};
use musubi_model::remote::{Cursor, MediaType, Raw, RawHash, RemoteKey, Validator};
use musubi_model::store::{
    CanonicalJson, CheckpointInfo, CheckpointMode, CollectionError, CollectionState,
    CollectionStatus, Coverage, DiscoveryWrite, EntityView, Include, ItemState, ItemView,
    ItemWrite, OccurrenceRow, OccurrenceView, OccurrenceWrite, PageWrite, PlacementView, PruneInfo,
    RangeCursor, RangePage, RangeQuery, SeriesRef, StoreStats, TenantStore,
};
use musubi_model::time::TimeWindow;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde_json::value::RawValue;

use crate::schema::LONG_SPAN;
use crate::storage::SqliteTenant;
use crate::{Result, corrupt, enum_text, opt_ts, parse_enum, ts, us};

/// `occurrences.rid` of an occurrence without a recurrence id; sorts first, as `None` does.
const NO_RID: i64 = i64::MIN;
/// Floating and all-day occurrences are stored in wall time; real offsets lie
/// within ±14 hours of it.
const FLOAT_WIDEN: i64 = 14 * 3600 * 1_000_000;

/// Last committed journal position.
pub(crate) fn head(c: &Connection) -> Result<Seq> {
    let v: Option<i64> = c
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'changes'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Seq(v.unwrap_or(0) as u64))
}

fn meta(c: &Connection, k: &str) -> Result<i64> {
    Ok(c.query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))?)
}

/// `meta` keys of the re-decode versions of `kind` items of `provider`
/// (docs/plan/phase2.md 4.1).
fn redecode_keys(provider: &ProviderId, kind: EntityKind) -> (String, String) {
    let base = format!("mapping:{}:{}", provider.0, kind.as_str());
    (format!("{base}:schema"), format!("{base}:mapping"))
}

fn canonical(s: Option<String>) -> Result<Option<CanonicalJson>> {
    s.map(|s| RawValue::from_string(s).map_err(corrupt))
        .transpose()
}

fn read_id(v: Option<i64>) -> Option<ReadId> {
    v.map(|v| ReadId(v as u64))
}

/// Journal entries of one write transaction: one per changed target, numbered
/// from the head at the start of the transaction (`AUTOINCREMENT` never hands
/// out a lower number, so explicit numbers never collide or repeat).
struct Journal {
    base: u64,
    entries: Vec<(ChangeTarget, ChangeOp)>,
    index: HashMap<ChangeTarget, usize>,
}

impl Journal {
    fn new(c: &Connection) -> Result<Self> {
        Ok(Self {
            base: head(c)?.0,
            entries: Vec::new(),
            index: HashMap::new(),
        })
    }

    /// Records a change and returns its journal position.
    fn record(&mut self, target: ChangeTarget, op: ChangeOp) -> u64 {
        if let Some(&i) = self.index.get(&target) {
            let old = &mut self.entries[i].1;
            *old = match (*old, op) {
                (_, ChangeOp::Deleted) => ChangeOp::Deleted,
                (ChangeOp::Deleted, _) => ChangeOp::Updated,
                (old, _) => old,
            };
            return self.base + i as u64 + 1;
        }
        self.index.insert(target, self.entries.len());
        self.entries.push((target, op));
        self.base + self.entries.len() as u64
    }

    fn flush(self, c: &Connection, now: Timestamp) -> Result<Seq> {
        let mut st = c.prepare_cached(
            "INSERT INTO changes(seq, kind, target, op, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (i, (target, op)) in self.entries.iter().enumerate() {
            let (kind, id) = match *target {
                ChangeTarget::Entity(id) => (0, id.to_bytes()),
                ChangeTarget::Placement(id) => (1, id.to_bytes()),
                ChangeTarget::Collection(id) => (2, id.to_bytes()),
            };
            let op = match op {
                ChangeOp::Created => 0,
                ChangeOp::Updated => 1,
                ChangeOp::Deleted => 2,
            };
            st.execute(params![
                (self.base + i as u64 + 1) as i64,
                kind,
                id,
                op,
                us(now)
            ])?;
        }
        Ok(Seq(self.base + self.entries.len() as u64))
    }
}

fn random_id(c: &Connection) -> Result<[u8; 16]> {
    Ok(c.prepare_cached("SELECT randomblob(16)")?
        .query_row([], |r| r.get(0))?)
}

fn insert_occurrences(
    c: &Connection,
    entity: [u8; 16],
    item: [u8; 16],
    col: [u8; 16],
    rows: &[OccurrenceRow],
) -> Result<()> {
    let mut st = c.prepare_cached(
        "INSERT INTO occurrences(start, end, entity, rid, floating, collection, item)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for r in rows {
        st.execute(params![
            us(r.start),
            us(r.end),
            entity,
            r.recurrence_id.map_or(NO_RID, us),
            r.floating,
            col,
            item
        ])?;
    }
    Ok(())
}

/// Tombstones a live item with its entity and placement and drops its occurrences.
fn tombstone(
    c: &Connection,
    j: &mut Journal,
    item: [u8; 16],
    entity: [u8; 16],
    now: Timestamp,
) -> Result<()> {
    let now = us(now);
    c.prepare_cached("UPDATE items SET tombstone = 1, deleted_at = ?2 WHERE id = ?1")?
        .execute(params![item, now])?;
    let seq = j.record(
        ChangeTarget::Entity(EntityId::from_bytes(entity)),
        ChangeOp::Deleted,
    );
    c.prepare_cached(
        "UPDATE entities SET tombstone = 1, deleted_at = ?2, rev = rev + 1, seq = ?3 WHERE id = ?1",
    )?
    .execute(params![entity, now, seq as i64])?;
    let placement: [u8; 16] = c
        .prepare_cached(
            "UPDATE placements SET tombstone = 1, deleted_at = ?2 WHERE item = ?1 RETURNING id",
        )?
        .query_row(params![item, now], |r| r.get(0))?;
    j.record(
        ChangeTarget::Placement(PlacementId::from_bytes(placement)),
        ChangeOp::Deleted,
    );
    c.prepare_cached("DELETE FROM occurrences WHERE entity = ?1")?
        .execute([entity])?;
    Ok(())
}

/// Live items of a collection matching `filter` (an SQL condition on `items`).
fn live_items(
    c: &Connection,
    col: [u8; 16],
    filter: &str,
    read: Option<i64>,
) -> Result<Vec<([u8; 16], [u8; 16])>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT id, entity FROM items WHERE collection = ?1 AND tombstone = 0 AND {filter}"
    ))?;
    let rows = if filter.contains("?2") {
        st.query_map(params![col, read], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    } else {
        st.query_map(params![col], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    Ok(rows)
}

const UNSEEN: &str = "(seen IS NULL OR seen <> ?2)";

struct ReadState {
    active: Option<i64>,
    complete: bool,
    last: Option<i64>,
}

fn read_state(c: &Connection, col: [u8; 16]) -> Result<ReadState> {
    c.prepare_cached("SELECT active_read, read_complete, last_read FROM collections WHERE id = ?1")?
        .query_row([col], |r| {
            Ok(ReadState {
                active: r.get(0)?,
                complete: r.get(1)?,
                last: r.get(2)?,
            })
        })
        .optional()?
        .ok_or_else(|| StoreError::NotFound.into())
}

fn upsert(
    c: &Connection,
    j: &mut Journal,
    col: [u8; 16],
    w: &ItemWrite,
    mark: Option<i64>,
) -> Result<()> {
    struct Existing {
        id: [u8; 16],
        entity: [u8; 16],
        validator: String,
        raw_hash: [u8; 32],
        schema_version: u32,
        mapping_version: u32,
        tombstone: bool,
    }
    let existing = c
        .prepare_cached(
            "SELECT id, entity, validator, raw_hash, schema_version, mapping_version, tombstone
             FROM items WHERE collection = ?1 AND key = ?2",
        )?
        .query_row(params![col, w.key.0], |r| {
            Ok(Existing {
                id: r.get(0)?,
                entity: r.get(1)?,
                validator: r.get(2)?,
                raw_hash: r.get(3)?,
                schema_version: r.get(4)?,
                mapping_version: r.get(5)?,
                tombstone: r.get(6)?,
            })
        })
        .optional()?;
    if let Some(e) = &existing
        && !e.tombstone
        && e.validator == w.validator.0
        && e.raw_hash == w.raw_hash.0
        && e.schema_version == w.schema_version
        && e.mapping_version == w.mapping_version
    {
        // Same content as stored (invariant 2): only the seen-mark.
        if mark.is_some() {
            c.prepare_cached("UPDATE items SET seen = ?2 WHERE id = ?1")?
                .execute(params![e.id, mark])?;
        }
        return Ok(());
    }
    c.prepare_cached("INSERT OR IGNORE INTO raw(hash, media_type, body) VALUES (?1, ?2, ?3)")?
        .execute(params![
            w.raw_hash.0,
            enum_text(&w.raw.media_type)?,
            w.raw.body
        ])?;
    let observed = w.canonical.as_deref().map(RawValue::get);
    let (uid, rid) = match &w.correlation {
        Some(k) => (Some(k.uid.as_str()), k.recurrence_id.map(us)),
        None => (None, None),
    };
    let kind = w.kind.as_str();
    let (item, entity) = match existing {
        Some(e) => {
            c.prepare_cached(
                "UPDATE items SET kind = ?2, validator = ?3, raw_hash = ?4, schema_version = ?5,
                   mapping_version = ?6, observed = ?7, uid = ?8, recurrence_id = ?9,
                   tombstone = 0, deleted_at = NULL, seen = coalesce(?10, seen)
                 WHERE id = ?1",
            )?
            .execute(params![
                e.id,
                kind,
                w.validator.0,
                w.raw_hash.0,
                w.schema_version,
                w.mapping_version,
                observed,
                uid,
                rid,
                mark
            ])?;
            let op = if e.tombstone {
                ChangeOp::Created
            } else {
                ChangeOp::Updated
            };
            let seq = j.record(ChangeTarget::Entity(EntityId::from_bytes(e.entity)), op);
            c.prepare_cached(
                "UPDATE entities SET kind = ?2, rev = rev + 1, seq = ?3, tombstone = 0, deleted_at = NULL
                 WHERE id = ?1",
            )?
            .execute(params![e.entity, kind, seq as i64])?;
            if e.tombstone {
                let p: [u8; 16] = c
                    .prepare_cached(
                        "UPDATE placements SET tombstone = 0, deleted_at = NULL WHERE item = ?1
                         RETURNING id",
                    )?
                    .query_row([e.id], |r| r.get(0))?;
                j.record(
                    ChangeTarget::Placement(PlacementId::from_bytes(p)),
                    ChangeOp::Created,
                );
            }
            c.prepare_cached("DELETE FROM occurrences WHERE entity = ?1")?
                .execute([e.entity])?;
            (e.id, e.entity)
        }
        None => {
            let (item, entity, placement) = (random_id(c)?, random_id(c)?, random_id(c)?);
            let seq = j.record(
                ChangeTarget::Entity(EntityId::from_bytes(entity)),
                ChangeOp::Created,
            );
            c.prepare_cached("INSERT INTO entities(id, kind, rev, seq) VALUES (?1, ?2, 1, ?3)")?
                .execute(params![entity, kind, seq as i64])?;
            c.prepare_cached(
                "INSERT INTO items(id, collection, key, entity, kind, validator, raw_hash,
                   schema_version, mapping_version, observed, uid, recurrence_id, seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?
            .execute(params![
                item,
                col,
                w.key.0,
                entity,
                kind,
                w.validator.0,
                w.raw_hash.0,
                w.schema_version,
                w.mapping_version,
                observed,
                uid,
                rid,
                mark
            ])?;
            // Phase 1: every placement is the primary, synced, both-way, full one.
            c.prepare_cached(
                "INSERT INTO placements(id, entity, item, collection, role, direction, projection,
                   origin, delivery)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                placement,
                entity,
                item,
                col,
                enum_text(&Role::Primary)?,
                enum_text(&Direction::Both)?,
                enum_text(&Projection::Full)?,
                enum_text(&Origin::Manual)?,
                enum_text(&Delivery::Synced)?
            ])?;
            j.record(
                ChangeTarget::Placement(PlacementId::from_bytes(placement)),
                ChangeOp::Created,
            );
            (item, entity)
        }
    };
    let s = w.occurrences.series;
    c.prepare_cached(
        "UPDATE entities SET series_first = ?2, series_last = ?3, indexed_until = ?4 WHERE id = ?1",
    )?
    .execute(params![
        entity,
        s.map(|s| us(s.first)),
        s.and_then(|s| s.last).map(us),
        s.map(|s| us(s.indexed_until))
    ])?;
    insert_occurrences(c, entity, item, col, &w.occurrences.rows)
}

fn apply_page(c: &Connection, p: &PageWrite) -> Result<Seq> {
    let col = p.collection.to_bytes();
    let mut st = read_state(c, col)?;
    if let Some(r) = p.read {
        let r = r.0 as i64;
        let replaced = st.active != Some(r);
        match st.active {
            Some(a) if r == a => {}
            // A newer read replaces the active one; marks of `a` no longer count.
            Some(a) if r > a => st.active = Some(r),
            None if st.last.is_none_or(|l| r > l) => st.active = Some(r),
            _ => return Err(StoreError::StaleRead.into()),
        }
        // A new read starts incomplete; the read in progress only ever completes.
        if replaced {
            st.complete = p.last;
        } else if p.last {
            st.complete = true;
        }
    }
    let mark = st.active;
    let now = Timestamp::now();
    let mut j = Journal::new(c)?;
    for w in &p.upserts {
        upsert(c, &mut j, col, w, mark)?;
    }
    if let Some(m) = mark {
        let mut seen =
            c.prepare_cached("UPDATE items SET seen = ?3 WHERE collection = ?1 AND key = ?2")?;
        for k in p.unchanged.iter().chain(&p.removals) {
            seen.execute(params![col, k.0, m])?;
        }
    }
    for k in &p.removals {
        let live: Option<([u8; 16], [u8; 16])> = c
            .prepare_cached(
                "SELECT id, entity FROM items WHERE collection = ?1 AND key = ?2 AND tombstone = 0",
            )?
            .query_row(params![col, k.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((item, entity)) = live {
            tombstone(c, &mut j, item, entity, now)?;
        }
    }
    c.prepare_cached(
        "UPDATE collections SET cursor = coalesce(?2, cursor), active_read = ?3, read_complete = ?4
         WHERE id = ?1",
    )?
    .execute(params![
        col,
        p.cursor.as_ref().map(|c| &c.0),
        st.active,
        st.complete
    ])?;
    let seq = j.flush(c, now)?;
    // Everything of the page is written but not committed.
    #[cfg(feature = "crash-points")]
    crate::crash::point("apply-page", true);
    Ok(seq)
}

fn collection_state(r: &Row<'_>) -> rusqlite::Result<Result<CollectionState>> {
    let meta: String = r.get(2)?;
    let coverage: i64 = r.get(8)?;
    let (cov_start, cov_end): (Option<i64>, Option<i64>) = (r.get(9)?, r.get(10)?);
    let (err_message, err_at, err_permanent): (Option<String>, Option<i64>, Option<bool>) =
        (r.get(11)?, r.get(12)?, r.get(13)?);
    let (synced_at, full_read_at): (Option<i64>, Option<i64>) = (r.get(14)?, r.get(15)?);
    let id = CollectionId::from_bytes(r.get(0)?);
    let account = AccountId::from_bytes(r.get(1)?);
    let cursor: Option<Vec<u8>> = r.get(4)?;
    let (active, complete, last): (Option<i64>, bool, Option<i64>) =
        (r.get(5)?, r.get(6)?, r.get(7)?);
    let gone: bool = r.get(16)?;
    let push_resource: Option<String> = r.get(17)?;
    Ok((|| {
        let coverage = match (coverage, cov_start, cov_end) {
            (0, _, _) => Coverage::None,
            (1, Some(s), Some(e)) => Coverage::Window(
                TimeWindow::new(ts(s)?, ts(e)?).ok_or_else(|| corrupt("empty coverage window"))?,
            ),
            (2, _, _) => Coverage::Full,
            _ => return Err(corrupt("bad coverage")),
        };
        let error = match (err_message, err_at) {
            (Some(message), Some(at)) => Some(CollectionError {
                message,
                at: ts(at)?,
                permanent: err_permanent.unwrap_or(false),
            }),
            _ => None,
        };
        Ok(CollectionState {
            id,
            account,
            remote: serde_json::from_str(&meta)?,
            cursor: cursor.map(Cursor),
            active_read: read_id(active),
            read_complete: complete,
            last_read: read_id(last),
            status: CollectionStatus {
                coverage,
                error,
                synced_at: opt_ts(synced_at)?,
                full_read_at: opt_ts(full_read_at)?,
            },
            gone,
            push_resource,
        })
    })())
}

fn entity(c: &Connection, id: EntityId, inc: Include) -> Result<Option<EntityView>> {
    let e = c
        .prepare_cached(
            "SELECT kind, rev, seq, tombstone,
               (SELECT observed FROM items WHERE entity = e.id LIMIT 1)
             FROM entities e WHERE id = ?1",
        )?
        .query_row([id.to_bytes()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .optional()?;
    let Some((kind, rev, seq, tombstone, doc)) = e else {
        return Ok(None);
    };
    let mut placements = Vec::new();
    if inc.placements {
        let mut st = c.prepare_cached(
            "SELECT id, item, collection, role, direction, projection, origin, delivery
             FROM placements WHERE entity = ?1 AND tombstone = 0 ORDER BY id",
        )?;
        let mut rows = st.query([id.to_bytes()])?;
        while let Some(r) = rows.next()? {
            placements.push(PlacementView {
                id: PlacementId::from_bytes(r.get(0)?),
                item: ItemId::from_bytes(r.get(1)?),
                collection: CollectionId::from_bytes(r.get(2)?),
                role: parse_enum(r.get(3)?)?,
                direction: parse_enum(r.get(4)?)?,
                projection: parse_enum(r.get(5)?)?,
                origin: parse_enum(r.get(6)?)?,
                delivery: parse_enum(r.get(7)?)?,
            });
        }
    }
    Ok(Some(EntityView {
        id,
        kind: parse_enum(kind)?,
        rev: rev as u64,
        seq: Seq(seq as u64),
        tombstone,
        canonical: canonical(doc)?,
        placements,
    }))
}

fn item(c: &Connection, id: ItemId, inc: Include) -> Result<Option<ItemView>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT i.entity, i.collection, i.kind, i.key, i.validator, i.raw_hash,
           i.schema_version, i.mapping_version, i.observed, i.tombstone, {}
         FROM items i LEFT JOIN raw r ON r.hash = i.raw_hash WHERE i.id = ?1",
        if inc.raw {
            "r.media_type, r.body"
        } else {
            "NULL, NULL"
        }
    ))?;
    let mut rows = st.query([id.to_bytes()])?;
    let Some(r) = rows.next()? else {
        return Ok(None);
    };
    let media: Option<String> = r.get(10)?;
    let raw = match media {
        Some(m) => Some(Raw {
            media_type: parse_enum::<MediaType>(m)?,
            body: r.get(11)?,
        }),
        None => None,
    };
    Ok(Some(ItemView {
        id,
        entity: EntityId::from_bytes(r.get(0)?),
        collection: CollectionId::from_bytes(r.get(1)?),
        kind: parse_enum(r.get(2)?)?,
        key: RemoteKey(r.get(3)?),
        validator: Validator(r.get(4)?),
        raw_hash: RawHash(r.get(5)?),
        schema_version: r.get(6)?,
        mapping_version: r.get(7)?,
        observed: canonical(r.get(8)?)?,
        tombstone: r.get(9)?,
        raw,
    }))
}

/// The ids `sql` selects: `args` bind `?1..`, and `{cols}` in `sql` becomes the
/// placeholders of `cols`, bound after them.
fn ids_page(
    c: &Connection,
    sql: &str,
    args: Vec<rusqlite::types::Value>,
    cols: &[CollectionId],
) -> Result<Vec<[u8; 16]>> {
    let sql = sql.replace(
        "{cols}",
        &(args.len() + 1..=args.len() + cols.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    let mut args = args;
    args.extend(cols.iter().map(|c| c.to_bytes().to_vec().into()));
    let mut st = c.prepare_cached(&sql)?;
    let ids = st
        .query_map(params_from_iter(&args), |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Re-decodes one item from its stored raw content (see [`TenantStore::redecode`]).
fn redecode(c: &Connection, j: &mut Journal, col: [u8; 16], w: &ItemWrite) -> Result<()> {
    // A changed raw body means the item changed since it was read: skipped.
    let live: Option<([u8; 16], [u8; 16], Option<String>)> = c
        .prepare_cached(
            "SELECT id, entity, observed FROM items
             WHERE collection = ?1 AND key = ?2 AND raw_hash = ?3 AND tombstone = 0",
        )?
        .query_row(params![col, w.key.0, w.raw_hash.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let Some((item, entity, old)) = live else {
        return Ok(());
    };
    let observed = w.canonical.as_deref().map(RawValue::get);
    let (uid, rid) = match &w.correlation {
        Some(k) => (Some(k.uid.as_str()), k.recurrence_id.map(us)),
        None => (None, None),
    };
    c.prepare_cached(
        "UPDATE items SET schema_version = ?2, mapping_version = ?3, observed = ?4, uid = ?5,
           recurrence_id = ?6
         WHERE id = ?1",
    )?
    .execute(params![
        item,
        w.schema_version,
        w.mapping_version,
        observed,
        uid,
        rid
    ])?;
    if old.as_deref() != observed {
        let seq = j.record(
            ChangeTarget::Entity(EntityId::from_bytes(entity)),
            ChangeOp::Updated,
        );
        c.prepare_cached("UPDATE entities SET rev = rev + 1, seq = ?2 WHERE id = ?1")?
            .execute(params![entity, seq as i64])?;
    }
    let s = w.occurrences.series;
    c.prepare_cached(
        "UPDATE entities SET series_first = ?2, series_last = ?3, indexed_until = ?4 WHERE id = ?1",
    )?
    .execute(params![
        entity,
        s.map(|s| us(s.first)),
        s.and_then(|s| s.last).map(us),
        s.map(|s| us(s.indexed_until))
    ])?;
    c.prepare_cached("DELETE FROM occurrences WHERE entity = ?1")?
        .execute([entity])?;
    insert_occurrences(c, entity, item, col, &w.occurrences.rows)
}

/// Wall time read as UTC, placed in `zone`.
fn place(t: Timestamp, zone: &TimeZone) -> Result<Timestamp> {
    let wall = t.to_zoned(TimeZone::UTC).datetime();
    zone.to_ambiguous_timestamp(wall)
        .compatible()
        .map_err(corrupt)
}

fn range(c: &Connection, q: &RangeQuery) -> Result<RangePage> {
    // `IN ()` folds the query to nothing, which `INDEXED BY` below refuses to plan.
    if q.collections.as_ref().is_some_and(Vec::is_empty) {
        return Ok(RangePage {
            occurrences: Vec::new(),
            next: None,
            unindexed: Vec::new(),
        });
    }
    let (ws, we) = (us(q.window.start()), us(q.window.end()));
    let cols: Vec<[u8; 16]> = q
        .collections
        .iter()
        .flatten()
        .map(|c| c.to_bytes())
        .collect();
    // Collections bind as ?first, ?first+1, … (numbered: the range query uses them twice).
    let col_filter = |alias: &str, first: usize| match &q.collections {
        None => String::new(),
        Some(_) => format!(
            "AND {alias}.collection IN ({})",
            (first..first + cols.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    };
    let after = q.after.map(|a| {
        (
            us(a.start),
            a.entity.to_bytes(),
            a.recurrence_id.map_or(NO_RID, us),
        )
    });
    let floor = after.map_or(i64::MIN, |a| a.0);
    let (lo, hi) = (
        ws.saturating_sub(FLOAT_WIDEN),
        we.saturating_add(FLOAT_WIDEN),
    );
    // Candidates by index (widened for floating rows), exact overlap below. Short
    // occurrences by start, which an overlap bounds to [lo - LONG_SPAN, hi); long
    // ones by end through their partial index (forced: for the merge's order the
    // planner would rather walk `occurrences_time` from ?3). Both halves share
    // the arguments.
    let rest = format!(
        "{keyset} {cols}
           AND NOT (o.rid <> {NO_RID} AND i.uid IS NOT NULL AND EXISTS (
             SELECT 1 FROM items x WHERE x.collection = i.collection AND x.uid = i.uid
               AND x.recurrence_id = o.rid AND x.tombstone = 0 AND x.id <> i.id))",
        keyset = if after.is_some() {
            "AND (o.start, o.entity, o.rid) > (?4, ?5, ?6)"
        } else {
            ""
        },
        cols = col_filter("o", 7),
    );
    let sql = format!(
        "SELECT o.start, o.end, o.entity, o.rid, o.floating, {doc}
         FROM occurrences o JOIN items i ON i.id = o.item
         WHERE o.start >= max(?1 - {LONG_SPAN}, ?3) AND o.start < ?2 AND o.end >= ?1
           AND o.end - o.start <= {LONG_SPAN} {rest}
         UNION ALL
         SELECT o.start, o.end, o.entity, o.rid, o.floating, {doc}
         FROM occurrences o INDEXED BY occurrences_long JOIN items i ON i.id = o.item
         WHERE o.end - o.start > {LONG_SPAN} AND o.end >= ?1 AND o.start < ?2 AND o.start >= ?3
           {rest}
         ORDER BY 1, 3, 4",
        doc = if q.ids_only { "NULL" } else { "i.observed" },
    );
    let mut args: Vec<rusqlite::types::Value> = vec![lo.into(), hi.into(), floor.into()];
    // ?4..?6 are the keyset; unused (and NULL) on the first page.
    match after {
        Some((s, e, r)) => args.extend([s.into(), e.to_vec().into(), r.into()]),
        None => args.extend(std::iter::repeat_n(rusqlite::types::Value::Null, 3)),
    }
    args.extend(cols.iter().map(|c| c.to_vec().into()));
    let limit = q.limit.max(1) as usize;
    let mut st = c.prepare_cached(&sql)?;
    // Without collections the statement ends at the last parameter it uses.
    args.truncate(st.parameter_count());
    let mut rows = st.query(params_from_iter(&args))?;
    let mut occurrences = Vec::new();
    let mut more = false;
    while let Some(r) = rows.next()? {
        let (start, end, floating): (Timestamp, Timestamp, bool) =
            (ts(r.get(0)?)?, ts(r.get(1)?)?, r.get(4)?);
        let (s, e) = if floating {
            (place(start, &q.zone)?, place(end, &q.zone)?)
        } else {
            (start, end)
        };
        if !q.window.overlaps(s, e) {
            continue;
        }
        if occurrences.len() == limit {
            more = true;
            break;
        }
        let rid: i64 = r.get(3)?;
        occurrences.push(OccurrenceView {
            entity: EntityId::from_bytes(r.get(2)?),
            recurrence_id: if rid == NO_RID { None } else { Some(ts(rid)?) },
            start,
            end,
            floating,
            canonical: canonical(r.get(5)?)?,
        });
    }
    let next = more.then(|| {
        let l = occurrences.last().expect("limit is at least 1");
        RangeCursor {
            start: l.start,
            entity: l.entity,
            recurrence_id: l.recurrence_id,
        }
    });
    let mut unindexed = Vec::new();
    if q.after.is_none() {
        // Widened like the occurrences: the caller expands from `indexed_until` on.
        let sql = format!(
            // Driven by `entities_horizon`; a join from `items` would scan the collection.
            "SELECT e.id, e.indexed_until FROM entities e
             WHERE e.indexed_until IS NOT NULL AND e.tombstone = 0 AND e.indexed_until < ?1
               AND e.series_first < ?1 AND (e.series_last IS NULL OR e.series_last >= ?2)
               AND (e.series_last IS NULL OR e.series_last > e.indexed_until)
               AND EXISTS (SELECT 1 FROM items i WHERE i.entity = e.id {})
             ORDER BY e.indexed_until, e.id LIMIT {UNINDEXED_MAX}",
            col_filter("i", 3)
        );
        let mut args: Vec<rusqlite::types::Value> = vec![
            we.saturating_add(FLOAT_WIDEN).into(),
            ws.saturating_sub(FLOAT_WIDEN).into(),
        ];
        args.extend(cols.iter().map(|c| c.to_vec().into()));
        let mut st = c.prepare_cached(&sql)?;
        let mut rows = st.query(params_from_iter(&args))?;
        while let Some(r) = rows.next()? {
            unindexed.push(SeriesRef {
                entity: EntityId::from_bytes(r.get(0)?),
                indexed_until: ts(r.get(1)?)?,
            });
        }
    }
    Ok(RangePage {
        occurrences,
        next,
        unindexed,
    })
}

/// Most series in `RangePage::unindexed`.
/// shortcut: series past this are left out until `extend_index` catches up;
/// signal truncation if the model grows a field for it.
const UNINDEXED_MAX: u32 = 10_000;
/// Most entries in one `changes` page, whatever the caller asks.
const CHANGES_MAX: u32 = 10_000;

fn changes(c: &Connection, since: Seq, limit: u32) -> Result<ChangesPage> {
    let limit = limit.min(CHANGES_MAX);
    let oldest = meta(c, "oldest")? as u64;
    if since.0 < oldest {
        return Err(StoreError::CursorTooOld {
            oldest: Seq(oldest),
        }
        .into());
    }
    let mut st = c.prepare_cached(
        "SELECT seq, kind, target, op FROM changes WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let mut rows = st.query(params![since.0 as i64, i64::from(limit) + 1])?;
    let mut changes = Vec::new();
    let mut more = false;
    while let Some(r) = rows.next()? {
        if changes.len() == limit as usize {
            more = true;
            break;
        }
        let (seq, kind, id, op): (i64, i64, [u8; 16], i64) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        let target = match kind {
            0 => ChangeTarget::Entity(EntityId::from_bytes(id)),
            1 => ChangeTarget::Placement(PlacementId::from_bytes(id)),
            2 => ChangeTarget::Collection(CollectionId::from_bytes(id)),
            k => return Err(corrupt(format!("change kind {k}"))),
        };
        let op = match op {
            0 => ChangeOp::Created,
            1 => ChangeOp::Updated,
            2 => ChangeOp::Deleted,
            o => return Err(corrupt(format!("change op {o}"))),
        };
        changes.push(Change {
            seq: Seq(seq as u64),
            target,
            op,
        });
    }
    let next = changes.last().map_or(since, |c| c.seq);
    Ok(ChangesPage {
        changes,
        next,
        more,
    })
}

impl TenantStore for SqliteTenant {
    fn head(&self) -> Result<Seq, StoreError> {
        Ok(self.shared.read(|c| head(c))?)
    }

    fn collections(&self) -> Result<Vec<CollectionState>, StoreError> {
        Ok(self.shared.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT id, account, meta, remote, cursor, active_read, read_complete, last_read,
                   coverage, cov_start, cov_end, err_message, err_at, err_permanent,
                   synced_at, full_read_at, gone, push_resource
                 FROM collections ORDER BY id",
            )?;
            st.query_map([], collection_state)?.map(|r| r?).collect()
        })?)
    }

    fn entity(&self, id: EntityId, inc: Include) -> Result<Option<EntityView>, StoreError> {
        Ok(self.shared.read(|c| entity(c, id, inc))?)
    }

    fn item(&self, id: ItemId, inc: Include) -> Result<Option<ItemView>, StoreError> {
        Ok(self.shared.read(|c| item(c, id, inc))?)
    }

    fn items_by_remote(
        &self,
        col: CollectionId,
        keys: &[RemoteKey],
    ) -> Result<Vec<ItemState>, StoreError> {
        Ok(self.shared.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT i.id, i.entity, i.validator, i.raw_hash, i.schema_version,
                        i.mapping_version, i.tombstone, e.indexed_until
                 FROM items i LEFT JOIN entities e ON e.id = i.entity
                 WHERE i.collection = ?1 AND i.key = ?2",
            )?;
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                let s = st
                    .query_row(params![col.to_bytes(), k.0], |r| {
                        Ok((
                            ItemState {
                                key: k.clone(),
                                item: ItemId::from_bytes(r.get(0)?),
                                entity: EntityId::from_bytes(r.get(1)?),
                                validator: Validator(r.get(2)?),
                                raw_hash: RawHash(r.get(3)?),
                                schema_version: r.get(4)?,
                                mapping_version: r.get(5)?,
                                tombstone: r.get(6)?,
                                indexed_until: None,
                            },
                            r.get::<_, Option<i64>>(7)?,
                        ))
                    })
                    .optional()?;
                if let Some((mut s, until)) = s {
                    s.indexed_until = opt_ts(until)?;
                    out.push(s);
                }
            }
            Ok(out)
        })?)
    }

    fn range(&self, q: &RangeQuery) -> Result<RangePage, StoreError> {
        Ok(self.shared.read(|c| range(c, q))?)
    }

    fn changes(&self, since: Seq, limit: u32) -> Result<ChangesPage, StoreError> {
        Ok(self.shared.read(|c| changes(c, since, limit))?)
    }

    fn unseen_count(&self, col: CollectionId, read: ReadId) -> Result<u64, StoreError> {
        Ok(self.shared.read(|c| {
            let n: i64 = c
                .prepare_cached(&format!(
                    "SELECT count(*) FROM items WHERE collection = ?1 AND tombstone = 0 AND {UNSEEN}"
                ))?
                .query_row(params![col.to_bytes(), read.0 as i64], |r| r.get(0))?;
            Ok(n as u64)
        })?)
    }

    fn unseen(
        &self,
        col: CollectionId,
        read: ReadId,
        after: Option<&RemoteKey>,
        limit: u32,
    ) -> Result<Vec<RemoteKey>, StoreError> {
        Ok(self.shared.read(|c| {
            let mut st = c.prepare_cached(&format!(
                "SELECT key FROM items WHERE collection = ?1 AND tombstone = 0 AND {UNSEEN}
                   AND key > ?3 ORDER BY key LIMIT ?4"
            ))?;
            st.query_map(
                params![
                    col.to_bytes(),
                    read.0 as i64,
                    after.map_or("", |k| k.0.as_str()),
                    limit
                ],
                |r| Ok(RemoteKey(r.get(0)?)),
            )?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
        })?)
    }

    fn series_to_extend(&self, until: Timestamp, limit: u32) -> Result<Vec<SeriesRef>, StoreError> {
        Ok(self.shared.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT id, indexed_until FROM entities
                 WHERE indexed_until IS NOT NULL AND tombstone = 0 AND indexed_until < ?1
                   AND (series_last IS NULL OR series_last > indexed_until)
                 ORDER BY indexed_until, id LIMIT ?2",
            )?;
            let mut rows = st.query(params![us(until), limit])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                out.push(SeriesRef {
                    entity: EntityId::from_bytes(r.get(0)?),
                    indexed_until: ts(r.get(1)?)?,
                });
            }
            Ok(out)
        })?)
    }

    fn list_entities(
        &self,
        kind: EntityKind,
        collections: &[CollectionId],
        after: Option<EntityId>,
        limit: u32,
        inc: Include,
    ) -> Result<Vec<EntityView>, StoreError> {
        if collections.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.shared.read(|c| {
            // Driven by `entities_kind`; an empty blob sorts before every id.
            let ids = ids_page(
                c,
                "SELECT e.id FROM entities e
                 WHERE e.kind = ?1 AND e.tombstone = 0 AND e.id > ?2
                   AND EXISTS (SELECT 1 FROM placements p WHERE p.entity = e.id
                     AND p.tombstone = 0 AND p.collection IN ({cols}))
                 ORDER BY e.id LIMIT ?3",
                vec![
                    kind.as_str().to_owned().into(),
                    after.map_or(Vec::new(), |a| a.to_bytes().to_vec()).into(),
                    i64::from(limit).into(),
                ],
                collections,
            )?;
            let inc = Include { raw: false, ..inc };
            ids.into_iter()
                .map(|id| {
                    entity(c, EntityId::from_bytes(id), inc)?
                        .ok_or_else(|| corrupt("listed entity vanished"))
                })
                .collect()
        })?)
    }

    fn stale_items(
        &self,
        kind: EntityKind,
        collections: &[CollectionId],
        schema_version: u32,
        mapping_version: u32,
        after: Option<ItemId>,
        limit: u32,
    ) -> Result<Vec<ItemView>, StoreError> {
        if collections.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.shared.read(|c| {
            let ids = ids_page(
                c,
                "SELECT i.id FROM items i
                 WHERE i.kind = ?1 AND i.tombstone = 0 AND i.id > ?2
                   AND (i.schema_version <> ?4 OR i.mapping_version <> ?5)
                   AND i.collection IN ({cols})
                 ORDER BY i.id LIMIT ?3",
                vec![
                    kind.as_str().to_owned().into(),
                    after.map_or(Vec::new(), |a| a.to_bytes().to_vec()).into(),
                    i64::from(limit).into(),
                    i64::from(schema_version).into(),
                    i64::from(mapping_version).into(),
                ],
                collections,
            )?;
            let inc = Include {
                placements: false,
                raw: true,
            };
            ids.into_iter()
                .map(|id| {
                    item(c, ItemId::from_bytes(id), inc)?
                        .ok_or_else(|| corrupt("listed item vanished"))
                })
                .collect()
        })?)
    }

    fn redecoded(
        &self,
        provider: &ProviderId,
        kind: EntityKind,
    ) -> Result<Option<(u32, u32)>, StoreError> {
        let (schema, mapping) = redecode_keys(provider, kind);
        Ok(self.shared.read(|c| {
            let get = |k: &str| -> Result<Option<u32>> {
                let v: Option<i64> = c
                    .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
                    .optional()?;
                v.map(|v| u32::try_from(v).map_err(corrupt)).transpose()
            };
            Ok(get(&schema)?.zip(get(&mapping)?))
        })?)
    }

    fn set_redecoded(
        &mut self,
        provider: &ProviderId,
        kind: EntityKind,
        schema: u32,
        mapping: u32,
    ) -> Result<(), StoreError> {
        let keys = redecode_keys(provider, kind);
        Ok(self.shared.write(|c| {
            for (k, v) in [(keys.0, schema), (keys.1, mapping)] {
                c.prepare_cached(
                    "INSERT INTO meta VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2",
                )?
                .execute(params![k, i64::from(v)])?;
            }
            Ok(())
        })?)
    }

    fn stats(&self) -> Result<StoreStats, StoreError> {
        let s = &self.shared;
        let (head, items, entities) = s.read(|c| {
            let count = |sql: &str| -> Result<u64> {
                Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
            };
            Ok((
                head(c)?,
                count("SELECT count(*) FROM items WHERE tombstone = 0")?,
                count("SELECT count(*) FROM entities WHERE tombstone = 0")?,
            ))
        })?;
        let wal_bytes = s.wal_size();
        Ok(StoreStats {
            head,
            main_bytes: s.main_size(),
            wal_bytes,
            wal_max_bytes: s.stats.wal_max.load(Ordering::Relaxed),
            truncates: s.stats.truncates.load(Ordering::Relaxed),
            truncate_max: Duration::from_micros(s.stats.truncate_max_us.load(Ordering::Relaxed)),
            reset_failures: s.stats.reset_failures.load(Ordering::Relaxed),
            items,
            entities,
        })
    }

    fn apply_discovery(&mut self, d: &DiscoveryWrite) -> Result<(), StoreError> {
        Ok(self.shared.write(|c| {
            let account = d.account.to_bytes();
            let mut stored: HashMap<String, ([u8; 16], String, bool)> = c
                .prepare_cached("SELECT remote, id, meta, gone FROM collections WHERE account = ?1")?
                .query_map([account], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?
                .collect::<rusqlite::Result<_>>()?;
            let mut j = Journal::new(c)?;
            let mut listed = Vec::new();
            for rc in &d.collections {
                let meta = serde_json::to_string(rc)?;
                let remote = rc.remote.0.clone();
                match stored.get(&remote) {
                    Some((id, old, gone)) => {
                        if *old != meta || *gone {
                            c.prepare_cached("UPDATE collections SET meta = ?2, gone = 0 WHERE id = ?1")?
                                .execute(params![id, meta])?;
                            j.record(
                                ChangeTarget::Collection(CollectionId::from_bytes(*id)),
                                ChangeOp::Updated,
                            );
                        }
                    }
                    None => {
                        let id = random_id(c)?;
                        c.prepare_cached(
                            "INSERT INTO collections(id, account, remote, meta) VALUES (?1, ?2, ?3, ?4)",
                        )?
                        .execute(params![id, account, remote, meta])?;
                        j.record(
                            ChangeTarget::Collection(CollectionId::from_bytes(id)),
                            ChangeOp::Created,
                        );
                    }
                }
                let id = c.query_row(
                    "SELECT id FROM collections WHERE account = ?1 AND remote = ?2",
                    params![account, remote],
                    |r| r.get(0),
                )?;
                stored.insert(remote.clone(), (id, meta, false));
                listed.push(remote);
            }
            if d.complete {
                for (remote, (id, _, gone)) in &stored {
                    if !gone && !listed.contains(remote) {
                        c.prepare_cached("UPDATE collections SET gone = 1 WHERE id = ?1")?
                            .execute([id])?;
                        j.record(
                            ChangeTarget::Collection(CollectionId::from_bytes(*id)),
                            ChangeOp::Updated,
                        );
                    }
                }
            }
            j.flush(c, Timestamp::now()).map(drop)
        })?)
    }

    fn apply_page(&mut self, p: &PageWrite) -> Result<Seq, StoreError> {
        Ok(self.shared.write(|c| apply_page(c, p))?)
    }

    fn redecode(&mut self, col: CollectionId, items: &[ItemWrite]) -> Result<Seq, StoreError> {
        Ok(self.shared.write(|c| {
            let id = col.to_bytes();
            read_state(c, id)?;
            let mut j = Journal::new(c)?;
            for w in items {
                redecode(c, &mut j, id, w)?;
            }
            j.flush(c, Timestamp::now())
        })?)
    }

    fn finish_full_read(&mut self, col: CollectionId, read: ReadId) -> Result<Seq, StoreError> {
        Ok(self.shared.write(|c| {
            let id = col.to_bytes();
            let st = read_state(c, id)?;
            let r = read.0 as i64;
            if st.active != Some(r) || !st.complete {
                return Err(StoreError::StaleRead.into());
            }
            let now = Timestamp::now();
            let mut j = Journal::new(c)?;
            for (item, entity) in live_items(c, id, UNSEEN, Some(r))? {
                tombstone(c, &mut j, item, entity, now)?;
            }
            c.prepare_cached(
                "UPDATE collections SET active_read = NULL, read_complete = 0, last_read = ?2
                 WHERE id = ?1",
            )?
            .execute(params![id, r])?;
            j.flush(c, now)
        })?)
    }

    fn purge_collection(&mut self, col: CollectionId) -> Result<Seq, StoreError> {
        Ok(self.shared.write(|c| {
            let id = col.to_bytes();
            read_state(c, id)?;
            let now = Timestamp::now();
            let mut j = Journal::new(c)?;
            for (item, entity) in live_items(c, id, "1", None)? {
                tombstone(c, &mut j, item, entity, now)?;
            }
            c.prepare_cached("DELETE FROM collections WHERE id = ?1")?
                .execute([id])?;
            j.record(ChangeTarget::Collection(col), ChangeOp::Deleted);
            j.flush(c, now)
        })?)
    }

    fn set_push_resource(&mut self, col: CollectionId, resource: &str) -> Result<(), StoreError> {
        Ok(self.shared.write(|c| {
            let n = c
                .prepare_cached("UPDATE collections SET push_resource = ?2 WHERE id = ?1")?
                .execute(params![col.to_bytes(), resource])?;
            if n == 0 {
                return Err(StoreError::NotFound.into());
            }
            Ok(())
        })?)
    }

    fn set_collection_state(
        &mut self,
        col: CollectionId,
        s: &CollectionStatus,
    ) -> Result<(), StoreError> {
        Ok(self.shared.write(|c| {
            let (coverage, cov) = match s.coverage {
                Coverage::None => (0, None),
                Coverage::Window(w) => (1, Some(w)),
                Coverage::Full => (2, None),
            };
            let n = c
                .prepare_cached(
                    "UPDATE collections SET coverage = ?2, cov_start = ?3, cov_end = ?4,
                       err_message = ?5, err_at = ?6, err_permanent = ?7,
                       synced_at = ?8, full_read_at = ?9
                     WHERE id = ?1",
                )?
                .execute(params![
                    col.to_bytes(),
                    coverage,
                    cov.map(|w| us(w.start())),
                    cov.map(|w| us(w.end())),
                    s.error.as_ref().map(|e| &e.message),
                    s.error.as_ref().map(|e| us(e.at)),
                    s.error.as_ref().map(|e| e.permanent),
                    s.synced_at.map(us),
                    s.full_read_at.map(us)
                ])?;
            if n == 0 {
                return Err(StoreError::NotFound.into());
            }
            Ok(())
        })?)
    }

    fn extend_occurrences(&mut self, w: &OccurrenceWrite) -> Result<(), StoreError> {
        Ok(self.shared.write(|c| {
            for ext in &w.extensions {
                let entity = ext.entity.to_bytes();
                let found: Option<(i64, [u8; 16], [u8; 16])> = c
                    .prepare_cached(
                        "SELECT e.indexed_until, i.id, i.collection
                         FROM entities e JOIN items i ON i.entity = e.id
                         WHERE e.id = ?1 AND e.tombstone = 0 AND e.indexed_until IS NOT NULL",
                    )?
                    .query_row([entity], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .optional()?;
                // Re-indexed meanwhile (or gone): the rows were computed from a stale horizon.
                let Some((until, item, col)) = found.filter(|f| f.0 == us(ext.from)) else {
                    continue;
                };
                let _ = until;
                insert_occurrences(c, entity, item, col, &ext.rows)?;
                c.prepare_cached("UPDATE entities SET indexed_until = ?2 WHERE id = ?1")?
                    .execute(params![entity, us(ext.indexed_until)])?;
            }
            Ok(())
        })?)
    }

    fn prune(&mut self, older_than: Timestamp) -> Result<PruneInfo, StoreError> {
        Ok(self.shared.write(|c| {
            let cut = us(older_than);
            // Walks seq order up to the first young entry: cost follows what is
            // pruned, not the journal size. Entries after a young one stay.
            let first_kept: Option<i64> = c
                .query_row(
                    "SELECT seq FROM changes WHERE at >= ?1 ORDER BY seq LIMIT 1",
                    [cut],
                    |r| r.get(0),
                )
                .optional()?;
            let last: Option<i64> = match first_kept {
                Some(k) => Some(k - 1).filter(|l| *l > 0),
                None => c.query_row("SELECT max(seq) FROM changes", [], |r| r.get(0))?,
            };
            let mut info = PruneInfo::default();
            if let Some(last) = last {
                info.changes = c.execute("DELETE FROM changes WHERE seq <= ?1", [last])? as u64;
                c.execute("UPDATE meta SET v = max(v, ?1) WHERE k = 'oldest'", [last])?;
            }
            for table in ["placements", "items", "entities"] {
                info.tombstones += c.execute(
                    &format!("DELETE FROM {table} WHERE tombstone = 1 AND deleted_at < ?1"),
                    [cut],
                )? as u64;
            }
            // Bodies no item refers to any more (replaced content, pruned items).
            info.raw = c.execute(
                "DELETE FROM raw WHERE NOT EXISTS (SELECT 1 FROM items WHERE raw_hash = raw.hash)",
                [],
            )? as u64;
            info.oldest = Seq(meta(c, "oldest")? as u64);
            Ok(info)
        })?)
    }

    fn checkpoint(&mut self, mode: CheckpointMode) -> Result<CheckpointInfo, StoreError> {
        Ok(self.shared.checkpoint(mode)?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use jiff::tz::TimeZone;
    use musubi_model::TimeWindow;

    use super::*;
    use crate::schema::{MAIN_MIGRATIONS, migrate};

    const H: i64 = 3_600_000_000;

    /// VM steps and occurrences of one first-page `range` over `[from, to)` hours.
    fn range_steps(c: &mut Connection, from: i64, to: i64) -> (u64, usize) {
        let n = Arc::new(AtomicU64::new(0));
        let counter = n.clone();
        c.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let q = RangeQuery {
            window: TimeWindow::new(
                Timestamp::from_microsecond(from * H).unwrap(),
                Timestamp::from_microsecond(to * H).unwrap(),
            )
            .unwrap(),
            collections: None,
            zone: TimeZone::UTC,
            after: None,
            limit: 10_000,
            ids_only: true,
        };
        let p = range(c, &q).unwrap();
        c.progress_handler(0, None::<fn() -> bool>).unwrap();
        (n.load(Ordering::Relaxed), p.occurrences.len())
    }

    /// A multi-year occurrence is found without widening the scan of short ones.
    #[test]
    fn long_occurrence_does_not_widen_the_range_scan() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, MAIN_MIGRATIONS).unwrap();
        c.execute_batch(
            "INSERT INTO items(id, collection, key, entity, kind, validator, raw_hash,
               schema_version, mapping_version) VALUES (x'00000000000000000000000000000001', x'00000000000000000000000000000002', 'k', x'00000000000000000000000000000003', 'event', 'v', x'00000000000000000000000000000004', 1, 1)",
        )
        .unwrap();
        let add = |c: &Connection, start: i64, end: i64| {
            c.execute(
                "INSERT INTO occurrences VALUES (?1, ?2, x'00000000000000000000000000000003', ?3, 0, x'00000000000000000000000000000002', x'00000000000000000000000000000001')",
                params![start * H, end * H, NO_RID],
            )
            .unwrap();
        };
        // Two years of one-hour events every three hours before a one-day window.
        let day = 24 * 730;
        let rows = (0..day + 24)
            .step_by(3)
            .inspect(|h| add(&c, *h, h + 1))
            .count() as u64;
        let (base, found) = range_steps(&mut c, day, day + 24);
        assert_eq!(found, 8);

        add(&c, -24 * 365, day + 24 * 365);
        let (steps, found) = range_steps(&mut c, day, day + 24);
        assert_eq!(found, 9, "the long occurrence is found");
        // A scan visiting every row costs several steps per row.
        assert!(
            steps < 2 * base && steps < rows,
            "{steps} VM steps, {base} without it, {rows} rows"
        );
    }
}
