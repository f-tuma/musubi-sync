//! Read routes over the tenant store, the journal and the SSE stream.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use musubi_model::control::{AccountRecord, ApiKeyHash, Scope};
use musubi_model::store::{
    CollectionState, Coverage, EntityView, Include, ItemView, RangeCursor, RangeQuery,
};
use musubi_model::{
    AccountId, CollectionId, ControlStore, EntityId, EntityKind, ItemId, Seq, Storage, TenantId,
    TenantStore, TimeWindow,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::{Duration, Instant, Interval};

use crate::crypto::{b64, unb64};
use crate::{ApiError, App, Caller};

const RANGE_LIMIT: u32 = 100;
const RANGE_LIMIT_MAX: u32 = 1000;
const CHANGES_LIMIT: u32 = 1000;
const CHANGES_LIMIT_MAX: u32 = 10_000;
/// Open streams per tenant; one more gets `429`.
pub(crate) const STREAMS_PER_TENANT: usize = 16;
/// How often a stream opened with an API key checks that the key still works.
const KEY_RECHECK: Duration = Duration::from_secs(60);

type Out = Result<Json<Value>, ApiError>;

/// `GET /v1/accounts/{id}`.
pub(crate) async fn account(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<AccountId>,
) -> Out {
    let tenant = caller.tenant(Scope::Read)?;
    let view = app
        .run(move |a| {
            let rec = a
                .control
                .account(id)?
                .filter(|r| r.tenant == tenant)
                .ok_or_else(ApiError::not_found)?;
            account_view(a, &rec)
        })
        .await?;
    Ok(Json(view))
}

/// The account as the API shows it: `missing` kinds with the `reconsent`
/// request that grants them, and for DAV the profile, entries, allowed
/// origins and user name. Never the password.
pub(crate) fn account_view(a: &App, rec: &AccountRecord) -> Result<Value, ApiError> {
    let mut v = json!({
        "id": rec.id, "provider": rec.provider, "subject": rec.subject, "state": rec.state,
        "missing": a.control.missing(rec.id)?,
    });
    if v["missing"] != json!([]) {
        // Plan 6.1: the way to grant the missing kinds.
        v["reconsent"] = json!({
            "method": "POST", "path": "/v1/accounts",
            "body": { "provider": rec.provider, "account": rec.id },
        });
    }
    if let Some(e) = a.control.endpoint(rec.id)? {
        for k in ["profile", "caldav_url", "carddav_url", "origins"] {
            v[k] = e.state.get(k).cloned().unwrap_or(Value::Null);
        }
    }
    if let Some(sealed) = a.control.read_tokens(rec.id)?
        && matches!(rec.provider.0.as_str(), "dav" | "icloud")
    {
        let (username, _password) = a.sealer.open_basic(rec.tenant, rec.id, &sealed)?;
        v["username"] = json!(username);
    }
    Ok(v)
}

/// `GET /v1/collections`.
pub(crate) async fn collections(State(app): State<Arc<App>>, caller: Caller) -> Out {
    let tenant = caller.tenant(Scope::Read)?;
    let cols = app
        .run(move |a| Ok(a.storage.open(tenant)?.collections()?))
        .await?;
    let cols: Vec<Value> = cols.iter().map(collection_json).collect();
    Ok(Json(json!({ "collections": cols })))
}

fn collection_json(c: &CollectionState) -> Value {
    let s = &c.status;
    let coverage = match s.coverage {
        Coverage::None => json!({ "type": "none" }),
        Coverage::Window(w) => json!({ "type": "window", "start": w.start(), "end": w.end() }),
        Coverage::Full => json!({ "type": "full" }),
    };
    json!({
        "id": c.id,
        "account": c.account,
        "remote": c.remote,
        "gone": c.gone,
        "coverage": coverage,
        "synced_at": s.synced_at,
        "full_read_at": s.full_read_at,
        "error": s.error.as_ref().map(|e| json!({
            "message": e.message, "at": e.at, "permanent": e.permanent,
        })),
    })
}

#[derive(Deserialize)]
pub(crate) struct RangeParams {
    /// `task`, `contact` or `contact_group`: a listing of that kind instead
    /// of the event range (`from`, `to`, `zone`, `ids_only`, `expand` unused).
    kind: Option<EntityKind>,
    from: Option<Timestamp>,
    to: Option<Timestamp>,
    /// IANA zone floating times are read in; default UTC.
    zone: Option<String>,
    /// Comma-separated collection ids; default all.
    collection: Option<String>,
    limit: Option<u32>,
    /// `next` of the previous page.
    after: Option<String>,
    #[serde(default)]
    ids_only: bool,
    /// Only `occurrences` (the default and only expansion in phase 1).
    expand: Option<String>,
}

/// The opaque `after` cursor of `GET /v1/entities`.
#[derive(Serialize, Deserialize)]
struct After {
    s: Timestamp,
    e: EntityId,
    r: Option<Timestamp>,
}

/// `GET /v1/entities?from&to`: occurrences in a window, paged; with `kind`,
/// the entities of a kind without occurrences.
pub(crate) async fn entities(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(p): Query<RangeParams>,
) -> Out {
    let tenant = caller.tenant(Scope::Read)?;
    let collections = p
        .collection
        .as_deref()
        .map(|c| {
            c.split(',')
                .map(|id| id.parse::<CollectionId>())
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map_err(|_| ApiError::bad_request("collection must be comma-separated collection ids"))?;
    let limit = p.limit.unwrap_or(RANGE_LIMIT);
    if !(1..=RANGE_LIMIT_MAX).contains(&limit) {
        return Err(ApiError::bad_request(format!(
            "limit must be 1..={RANGE_LIMIT_MAX}"
        )));
    }
    if let Some(kind) = p.kind {
        return list(&app, tenant, kind, collections, p.after, limit).await;
    }
    let (Some(from), Some(to)) = (p.from, p.to) else {
        return Err(ApiError::bad_request(
            "from and to are required without kind",
        ));
    };
    let window =
        TimeWindow::new(from, to).ok_or_else(|| ApiError::bad_request("from must be before to"))?;
    if p.expand.as_deref().is_some_and(|e| e != "occurrences") {
        return Err(ApiError::bad_request("expand supports only occurrences"));
    }
    let zone = match p.zone.as_deref() {
        None => TimeZone::UTC,
        Some(z) => TimeZone::get(z).map_err(|_| ApiError::bad_request("unknown zone"))?,
    };
    let after = p
        .after
        .as_deref()
        .map(|a| {
            unb64(a)
                .and_then(|b| serde_json::from_slice::<After>(&b).ok())
                .map(|a| RangeCursor {
                    start: a.s,
                    entity: a.e,
                    recurrence_id: a.r,
                })
                .ok_or_else(|| ApiError::bad_request("bad after cursor"))
        })
        .transpose()?;
    let q = RangeQuery {
        window,
        collections,
        zone,
        after,
        limit,
        ids_only: p.ids_only,
    };
    let page = app
        .run(move |a| Ok(a.storage.open(tenant)?.range(&q)?))
        .await?;
    let occurrences: Vec<Value> = page
        .occurrences
        .into_iter()
        .map(|o| {
            json!({
                "entity": o.entity,
                "recurrence_id": o.recurrence_id,
                "start": o.start,
                "end": o.end,
                "floating": o.floating,
                "canonical": o.canonical,
            })
        })
        .collect();
    let next = page.next.map(|n| {
        let a = After {
            s: n.start,
            e: n.entity,
            r: n.recurrence_id,
        };
        b64(&serde_json::to_vec(&a).expect("cursor serialises"))
    });
    // shortcut: series whose index ends inside the window are listed, not
    // expanded; the engine's ExtendIndex job (K5) extends them.
    let unindexed: Vec<Value> = page
        .unindexed
        .iter()
        .map(|s| json!({ "entity": s.entity, "indexed_until": s.indexed_until }))
        .collect();
    Ok(Json(
        json!({ "occurrences": occurrences, "next": next, "unindexed": unindexed }),
    ))
}

/// `GET /v1/entities?kind`: live entities of `kind` in `collections` (default
/// all), in id order; `next` is the last id of a full page.
async fn list(
    app: &Arc<App>,
    tenant: TenantId,
    kind: EntityKind,
    collections: Option<Vec<CollectionId>>,
    after: Option<String>,
    limit: u32,
) -> Out {
    if kind == EntityKind::Event {
        return Err(ApiError::bad_request("events are read by range (from, to)"));
    }
    let after = after
        .map(|a| a.parse::<EntityId>())
        .transpose()
        .map_err(|_| ApiError::bad_request("bad after cursor"))?;
    let page = app
        .run(move |a| {
            let t = a.storage.open(tenant)?;
            let cols = match collections {
                Some(c) => c,
                None => t.collections()?.into_iter().map(|c| c.id).collect(),
            };
            Ok(t.list_entities(kind, &cols, after, limit, Include::default())?)
        })
        .await?;
    let next = (page.len() == limit as usize)
        .then(|| page.last().map(|e| e.id.to_string()))
        .flatten();
    let entities: Vec<Value> = page.iter().map(|e| entity_json(e, false)).collect();
    Ok(Json(json!({ "entities": entities, "next": next })))
}

#[derive(Deserialize)]
pub(crate) struct IncludeParam {
    include: Option<String>,
}

impl IncludeParam {
    fn has(&self, what: &str) -> bool {
        self.include
            .as_deref()
            .is_some_and(|i| i.split(',').any(|x| x == what))
    }
}

/// `GET /v1/entities/{id}?include=placements`.
pub(crate) async fn entity(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<EntityId>,
    Query(inc): Query<IncludeParam>,
) -> Out {
    let tenant = caller.tenant(Scope::Read)?;
    let inc = Include {
        placements: inc.has("placements"),
        raw: false,
    };
    let e = app
        .run(move |a| Ok(a.storage.open(tenant)?.entity(id, inc)?))
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(entity_json(&e, inc.placements)))
}

fn entity_json(e: &EntityView, placements: bool) -> Value {
    let mut v = json!({
        "id": e.id, "kind": e.kind, "rev": e.rev, "seq": e.seq,
        "tombstone": e.tombstone, "canonical": e.canonical,
    });
    if placements {
        v["placements"] = e
            .placements
            .iter()
            .map(|p| {
                json!({
                    "id": p.id, "item": p.item, "collection": p.collection, "role": p.role,
                    "direction": p.direction, "projection": p.projection,
                    "origin": p.origin, "delivery": p.delivery,
                })
            })
            .collect();
    }
    v
}

/// `GET /v1/items/{id}?include=raw`.
pub(crate) async fn item(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<ItemId>,
    Query(inc): Query<IncludeParam>,
) -> Out {
    let tenant = caller.tenant(Scope::Read)?;
    let inc = Include {
        placements: false,
        raw: inc.has("raw"),
    };
    let i = app
        .run(move |a| Ok(a.storage.open(tenant)?.item(id, inc)?))
        .await?
        .ok_or_else(ApiError::not_found)?;
    item_json(&i)
}

fn item_json(i: &ItemView) -> Out {
    let mut v = json!({
        "id": i.id, "entity": i.entity, "collection": i.collection, "kind": i.kind,
        "key": i.key, "validator": i.validator, "raw_hash": hex(&i.raw_hash.0),
        "schema_version": i.schema_version, "mapping_version": i.mapping_version,
        "observed": i.observed, "tombstone": i.tombstone,
    });
    if let Some(r) = &i.raw {
        // Raw JSON is embedded as is; it was valid JSON when the adapter stored it.
        let body: Value = serde_json::from_slice(&r.body).map_err(|e| ApiError::internal(&e))?;
        v["raw"] = json!({ "media_type": r.media_type, "body": body });
    }
    Ok(Json(v))
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Deserialize)]
pub(crate) struct ChangesParams {
    since: Seq,
    limit: Option<u32>,
}

/// `GET /v1/changes?since`: the journal after `since`; `410` if pruned past it.
pub(crate) async fn changes(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(p): Query<ChangesParams>,
) -> Out {
    let tenant = caller.tenant(Scope::Read)?;
    let limit = p.limit.unwrap_or(CHANGES_LIMIT);
    if !(1..=CHANGES_LIMIT_MAX).contains(&limit) {
        return Err(ApiError::bad_request(format!(
            "limit must be 1..={CHANGES_LIMIT_MAX}"
        )));
    }
    let page = app
        .run(move |a| {
            let t = a.storage.open(tenant)?;
            ahead(p.since, t.head()?)?;
            Ok(t.changes(p.since, limit)?)
        })
        .await?;
    Ok(Json(
        serde_json::to_value(page).map_err(|e| ApiError::internal(&e))?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct StreamParams {
    since: Option<Seq>,
}

/// A cursor past the head comes from another journal (a node restored from
/// an older backup): like a pruned one, it needs a new initial sync.
fn ahead(since: Seq, head: Seq) -> Result<(), ApiError> {
    if since > head {
        return Err(ApiError {
            oldest: Some(head),
            ..ApiError::new(
                axum::http::StatusCode::GONE,
                "cursor_ahead",
                "new initial sync required: the cursor is ahead of this journal",
            )
        });
    }
    Ok(())
}

/// `GET /v1/stream[?since]`: SSE of journal heads. Each event is
/// `id: <seq>`, `data: {"cursor": <seq>}`, sent after the commit. With `since`
/// (or `Last-Event-ID`) a pruned cursor or one past the head gets `410` before
/// the stream starts; otherwise the current head is sent first. A tenant has
/// at most [`STREAMS_PER_TENANT`] streams (`429` beyond). A stream ends when
/// the token it was opened with expires, or within [`KEY_RECHECK`] after its
/// API key is revoked; the client reconnects with a fresh credential.
pub(crate) async fn stream(
    State(app): State<Arc<App>>,
    caller: Caller,
    headers: HeaderMap,
    Query(p): Query<StreamParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let tenant = caller.tenant(Scope::Read)?;
    let (until, key) = match caller {
        Caller::Token { exp, .. } => {
            let left = exp.saturating_sub(Timestamp::now().as_second()).max(0);
            (Instant::now() + Duration::from_secs(left as u64), None)
        }
        Caller::Key { hash, .. } => (
            Instant::now() + Duration::from_secs(10 * 365 * 86_400),
            Some(hash),
        ),
        Caller::Admin => unreachable!("rejected by tenant()"),
    };
    let since = match p.since {
        Some(s) => Some(s),
        None => headers
            .get("last-event-id")
            .map(|v| {
                v.to_str()
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .map(Seq)
                    .ok_or_else(|| ApiError::bad_request("bad Last-Event-ID"))
            })
            .transpose()?,
    };
    // Subscribe before reading the head, so no commit falls in between. The
    // receiver is the stream's place under the cap until the stream drops.
    let rx = app
        .bus
        .subscribe(tenant, STREAMS_PER_TENANT)
        .ok_or_else(|| ApiError::too_many("too many open streams for this tenant"))?;
    let head = app
        .run(move |a| {
            let t = a.storage.open(tenant)?;
            let head = t.head()?;
            if let Some(s) = since {
                ahead(s, head)?;
                t.changes(s, 1)?;
            }
            Ok(head)
        })
        .await?;
    let st = Feed {
        rx,
        app,
        until,
        key,
        recheck: tokio::time::interval_at(Instant::now() + KEY_RECHECK, KEY_RECHECK),
        sent: since,
        pending: since.is_none_or(|s| head > s).then_some(head),
    };
    let events = futures_util::stream::unfold(st, |mut st| async move {
        let seq = st.next().await?;
        let ev = Event::default()
            .id(seq.0.to_string())
            .data(json!({ "cursor": seq }).to_string());
        Some((Ok(ev), st))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

struct Feed {
    /// The tenant's heads; also its slot under the stream cap.
    rx: watch::Receiver<Seq>,
    app: Arc<App>,
    /// The credential's expiry.
    until: Instant,
    /// The API key the stream was opened with, checked every [`KEY_RECHECK`].
    key: Option<ApiKeyHash>,
    recheck: Interval,
    sent: Option<Seq>,
    pending: Option<Seq>,
}

impl Feed {
    /// The next head to send; `None` ends the stream.
    async fn next(&mut self) -> Option<Seq> {
        loop {
            if let Some(h) = self.pending.take() {
                self.sent = Some(h);
                return Some(h);
            }
            tokio::select! {
                changed = self.rx.changed() => {
                    changed.ok()?;
                    let head = *self.rx.borrow_and_update();
                    if self.sent.is_none_or(|s| head > s) {
                        self.pending = Some(head);
                    }
                }
                () = tokio::time::sleep_until(self.until) => return None,
                _ = self.recheck.tick(), if self.key.is_some() => {
                    let hash = self.key?;
                    let found = self.app.run(move |a| Ok(a.control.find_api_key(&hash)?)).await;
                    // Revoked, deleted with its tenant, or unknown: stop.
                    if !matches!(found, Ok(Some(_))) {
                        return None;
                    }
                }
            }
        }
    }
}
