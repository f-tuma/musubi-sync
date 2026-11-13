//! Deltas and bodies (plan, chapters 5.2 and 5.3).

use std::collections::{BTreeMap, HashSet, VecDeque};

use musubi_model::provider::{BoxStream, FetchHint, Pass, Start};
use musubi_model::remote::{MediaType, RemoteRef};
use musubi_model::{
    AccountCx, ChangePage, CollectionRef, Cursor, EntityKind, Raw, RemoteKey, RemoteObject, Stub,
    SyncError, TimeWindow, Validator,
};
use reqwest::Url;

use crate::net::{Mode, Reply, Session, origin};
use crate::state::{Col, CursorState, Level, State};
use crate::xml::{self, CALDAV, CARDDAV, CS, DAV, Multistatus, pct_decode};
use crate::{DavProvider, MAX_BATCH, MAX_PAGE};

/// One request's worth of work; each yields one or more pages.
enum Step {
    Begin(Option<Cursor>, Start),
    Window(TimeWindow),
    /// `sync-collection` from `token` (empty: initial read).
    Sync {
        token: String,
        full: bool,
    },
    /// Read the ctag (level `CtagEtag`): unchanged from `old` → an empty
    /// delta; otherwise (and at level `None`) an ETag listing.
    Ctag {
        old: Option<String>,
    },
}

/// The collection a `changes` call reads.
struct Target {
    key: String,
    col: Col,
    url: Url,
    origins: Vec<String>,
}

enum Entry {
    Stub(Stub),
    Object(RemoteObject),
}

pub(crate) fn changes<'a>(
    p: &'a DavProvider,
    cx: &'a AccountCx,
    col: &'a CollectionRef,
    cursor: Option<Cursor>,
    hint: FetchHint,
) -> BoxStream<'a, Result<ChangePage, SyncError>> {
    let size = hint.page_size.map_or(MAX_PAGE, |n| n.get().min(MAX_PAGE)) as usize;
    let init = (
        VecDeque::<ChangePage>::new(),
        Some(Step::Begin(cursor, hint.start)),
        None::<Target>,
    );
    Box::pin(futures_util::stream::unfold(
        init,
        move |(mut queue, mut step, mut target)| async move {
            loop {
                if let Some(page) = queue.pop_front() {
                    return Some((Ok(page), (queue, step, target)));
                }
                let next = match step.take()? {
                    Step::Begin(cursor, start) => match begin(cx, col, cursor, start) {
                        Ok((t, first)) => {
                            target = Some(t);
                            Ok((Vec::new(), Some(first)))
                        }
                        Err(e) => Err(e),
                    },
                    s => run(p, cx, target.as_ref().expect("begun"), s, size).await,
                };
                match next {
                    Ok((pages, then)) => {
                        queue.extend(pages);
                        step = then;
                    }
                    Err(e) => return Some((Err(e), (queue, None, target))),
                }
            }
        },
    ))
}

/// Picks the first request by start, cursor and the collection's level.
fn begin(
    cx: &AccountCx,
    col: &CollectionRef,
    cursor: Option<Cursor>,
    start: Start,
) -> Result<(Target, Step), SyncError> {
    let st = State::of(cx)?;
    let c = st.col(&col.0)?.clone();
    let url = c.url(&col.0)?;
    let first = match (cursor.as_ref().map(CursorState::from_cursor), c.delta) {
        (None, _) if c.kind == EntityKind::Event && matches!(start, Start::Window(_)) => {
            let Start::Window(w) = start else {
                unreachable!()
            };
            Step::Window(w)
        }
        (Some(Some(CursorState::SyncToken { token })), Level::SyncToken) => {
            Step::Sync { token, full: false }
        }
        (_, Level::SyncToken) => Step::Sync {
            token: String::new(),
            full: true,
        },
        (Some(Some(CursorState::Ctag { ctag })), Level::CtagEtag) => Step::Ctag { old: Some(ctag) },
        // A cursor of another level, a full read in progress or none: list again.
        _ => Step::Ctag { old: None },
    };
    let target = Target {
        key: col.0.clone(),
        col: c,
        url,
        origins: st.origins,
    };
    Ok((target, first))
}

type Pages = (Vec<ChangePage>, Option<Step>);

async fn run(
    p: &DavProvider,
    cx: &AccountCx,
    t: &Target,
    step: Step,
    size: usize,
) -> Result<Pages, SyncError> {
    let s = Session::new(&p.net, cx, &t.origins, Mode::Run)?;
    match step {
        Step::Begin(..) => unreachable!("handled by changes"),
        Step::Window(w) => window(&s, t, w, size).await,
        Step::Sync { token, full } => sync(p, &s, t, token, full, size).await,
        Step::Ctag { old } => ctag(&s, t, old, size).await,
    }
}

async fn send(
    s: &Session<'_>,
    method: &str,
    url: &Url,
    depth: Option<&str>,
    body: String,
) -> Result<Reply, SyncError> {
    s.send(method, url, depth, body)
        .await
        .map_err(|e| e.into_sync())
}

/// A `207` body, or the error for the status (plan, chapter 5.3).
fn multistatus(s: &Session<'_>, r: &Reply) -> Result<Multistatus, SyncError> {
    if r.status != 207 {
        return Err(r.error(s.cx.clock.now()));
    }
    xml::multistatus(&r.body)
        .map_err(|e| SyncError::Permanent(format!("{}: unreadable XML: {e}", r.url).into()))
}

/// Decoded key of an href: the collection itself or a member under its
/// path, on its origin; anything else is refused (plan, chapter 5.4).
fn object_key(t: &Target, base: &Url, href: &str) -> Option<String> {
    let url = base.join(href).ok()?;
    let key = pct_decode(url.path());
    let dir = format!("{}/", t.key.trim_end_matches('/'));
    if origin(&url) != t.col.origin || !(same_path(&key, &t.key) || key.starts_with(&dir)) {
        let url = url.path();
        tracing::warn!(collection = %t.key, %url, "DAV href outside the collection; skipped");
        return None;
    }
    Some(key)
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn stub(key: String, etag: Option<&str>) -> Entry {
    Entry::Stub(Stub::new(
        RemoteKey(key),
        etag.map(|e| Validator(e.to_owned())),
    ))
}

/// Cuts entries into pages: `mid` on every page but the final one, which
/// carries `end`, the removals and `last`.
#[allow(clippy::too_many_arguments)]
fn pages(
    entries: Vec<Entry>,
    removals: Vec<RemoteKey>,
    size: usize,
    pass: Pass,
    mid: Option<CursorState>,
    end: Option<CursorState>,
    last: bool,
) -> Vec<ChangePage> {
    let mut out = Vec::new();
    let mut it = entries.into_iter().peekable();
    loop {
        let mut page = ChangePage::new(pass, None, false);
        for e in it.by_ref().take(size) {
            match e {
                Entry::Stub(s) => page.stubs.push(s),
                Entry::Object(o) => page.upserts.push(o),
            }
        }
        let fin = it.peek().is_none();
        page.cursor = if fin { &end } else { &mid }
            .as_ref()
            .map(CursorState::to_cursor);
        if fin {
            page.removals = removals;
            page.last = last;
            out.push(page);
            return out;
        }
        out.push(page);
    }
}

/// First impression: events around today with their bodies.
async fn window(
    s: &Session<'_>,
    t: &Target,
    w: TimeWindow,
    size: usize,
) -> Result<Pages, SyncError> {
    let fmt = |ts: jiff::Timestamp| ts.strftime("%Y%m%dT%H%M%SZ").to_string();
    let r = send(
        s,
        "REPORT",
        &t.url,
        Some("1"),
        xml::calendar_query(&fmt(w.start()), &fmt(w.end())),
    )
    .await?;
    let ms = multistatus(s, &r)?;
    let mut entries = Vec::new();
    for resp in &ms.responses {
        let Some(key) = object_key(t, &r.url, &resp.href) else {
            continue;
        };
        if same_path(&key, &t.key) {
            continue;
        }
        if let Some(o) = resp
            .prop(CALDAV, "calendar-data")
            .and_then(|d| object(key, resp.text(DAV, "getetag"), &d.text, MediaType::Calendar))
        {
            entries.push(Entry::Object(o));
        }
    }
    Ok((
        pages(entries, Vec::new(), size, Pass::Window, None, None, true),
        None,
    ))
}

async fn sync(
    p: &DavProvider,
    s: &Session<'_>,
    t: &Target,
    token: String,
    full: bool,
    size: usize,
) -> Result<Pages, SyncError> {
    let r = send(s, "REPORT", &t.url, Some("0"), xml::sync_collection(&token)).await?;
    match r.status {
        403 | 409 if xml::has_condition(&r.body, DAV, "valid-sync-token") => {
            return Err(SyncError::CursorExpired);
        }
        // The report is gone: descend to ctag (or none) at the next discovery.
        405 | 501 => {
            tracing::info!(collection = %t.key, status = r.status, "sync-collection unsupported; delta descends");
            p.descents()
                .entry(s.cx.account)
                .or_default()
                .insert(t.key.clone());
            return Err(SyncError::Rediscover);
        }
        _ => {}
    }
    let ms = multistatus(s, &r)?;
    let new = ms
        .sync_token
        .clone()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            SyncError::Permanent("sync-collection answer without a sync-token".into())
        })?;
    let mut truncated = false;
    let mut changes: Vec<(String, Option<Option<&str>>)> = Vec::new();
    for resp in &ms.responses {
        let Some(key) = object_key(t, &r.url, &resp.href) else {
            continue;
        };
        if same_path(&key, &t.key) {
            truncated |= resp.status == Some(507);
            continue;
        }
        match resp.status {
            Some(404) => changes.push((key, None)),
            Some(c) if !(200..300).contains(&c) => {}
            _ => changes.push((key, Some(resp.text(DAV, "getetag")))),
        }
    }
    if truncated && changes.is_empty() && new == token {
        return Err(SyncError::Transient(
            "sync-collection truncated without progress".into(),
        ));
    }
    // One entry per key: the last one wins.
    let mut seen = HashSet::new();
    changes.reverse();
    changes.retain(|(k, _)| seen.insert(k.clone()));
    changes.reverse();
    let mut entries = Vec::new();
    let mut removals = Vec::new();
    for (key, change) in changes {
        match change {
            Some(etag) => entries.push(stub(key, etag)),
            // A full read deletes by absence.
            None if !full => removals.push(RemoteKey(key)),
            None => {}
        }
    }
    let (pass, mid) = if full {
        (Pass::Full, CursorState::Listing { since: None })
    } else {
        (Pass::Delta, CursorState::SyncToken { token })
    };
    let end = if truncated && full {
        CursorState::Listing { since: None }
    } else {
        CursorState::SyncToken { token: new.clone() }
    };
    let out = pages(
        entries,
        removals,
        size,
        pass,
        Some(mid),
        Some(end),
        !truncated,
    );
    Ok((out, truncated.then_some(Step::Sync { token: new, full })))
}

async fn ctag(
    s: &Session<'_>,
    t: &Target,
    old: Option<String>,
    size: usize,
) -> Result<Pages, SyncError> {
    let mut ctag = None;
    if t.col.delta == Level::CtagEtag {
        let r = send(
            s,
            "PROPFIND",
            &t.url,
            Some("0"),
            xml::propfind(&["CS:getctag"]),
        )
        .await?;
        let ms = multistatus(s, &r)?;
        ctag = ms
            .responses
            .first()
            .and_then(|x| x.text(CS, "getctag"))
            .map(str::to_owned);
        if let (Some(new), Some(old)) = (&ctag, &old)
            && new == old
        {
            let cursor = CursorState::Ctag { ctag: new.clone() }.to_cursor();
            return Ok((vec![ChangePage::new(Pass::Delta, Some(cursor), true)], None));
        }
    }
    // The ctag read before the listing: a change during it shows next time.
    let r = send(
        s,
        "PROPFIND",
        &t.url,
        Some("1"),
        xml::propfind(&["D:getetag", "D:resourcetype"]),
    )
    .await?;
    let ms = multistatus(s, &r)?;
    let mut entries = Vec::new();
    for resp in &ms.responses {
        let Some(key) = object_key(t, &r.url, &resp.href) else {
            continue;
        };
        let sub = resp
            .prop(DAV, "resourcetype")
            .is_some_and(|rt| rt.kid(DAV, "collection").is_some());
        if same_path(&key, &t.key) || sub {
            continue;
        }
        entries.push(stub(key, resp.text(DAV, "getetag")));
    }
    let mid = CursorState::Listing {
        since: ctag.clone(),
    };
    let end = match ctag {
        Some(ctag) => CursorState::Ctag { ctag },
        None => CursorState::Listing { since: None },
    };
    Ok((
        pages(
            entries,
            Vec::new(),
            size,
            Pass::Full,
            Some(mid),
            Some(end),
            true,
        ),
        None,
    ))
}

/// A body as received: validator `getetag`, else a hash of the body.
fn object(
    key: String,
    etag: Option<&str>,
    data: &str,
    media_type: MediaType,
) -> Option<RemoteObject> {
    let raw = Raw {
        media_type,
        body: data.as_bytes().to_vec(),
    };
    let Some(kind) = musubi_ical::kind_of(&raw) else {
        tracing::debug!(%key, "DAV object of no known kind; skipped");
        return None;
    };
    let validator = match etag {
        Some(e) => e.to_owned(),
        None => {
            let hex: String = raw.hash().0.iter().map(|b| format!("{b:02x}")).collect();
            format!("musubi:sha256:{hex}")
        }
    };
    Some(RemoteObject::new(
        RemoteKey(key),
        kind,
        Validator(validator),
        raw,
    ))
}

/// One `multiget` per collection and `MAX_BATCH` references.
pub(crate) async fn fetch(
    p: &DavProvider,
    cx: &AccountCx,
    refs: &[RemoteRef],
) -> Result<Vec<RemoteObject>, SyncError> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let st = State::of(cx)?;
    let s = Session::new(&p.net, cx, &st.origins, Mode::Run)?;
    let mut by_col: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for r in refs {
        by_col.entry(&r.collection.0).or_default().push(&r.key.0);
    }
    let mut out = Vec::new();
    for (key, objects) in by_col {
        let col = st.col(key)?;
        let t = Target {
            key: key.to_owned(),
            col: col.clone(),
            url: col.url(key)?,
            origins: Vec::new(),
        };
        let card = matches!(col.kind, EntityKind::Contact | EntityKind::ContactGroup);
        let (ns, data, media) = if card {
            (CARDDAV, "address-data", MediaType::VCard)
        } else {
            (CALDAV, "calendar-data", MediaType::Calendar)
        };
        for chunk in objects.chunks(MAX_BATCH as usize) {
            let wanted: HashSet<&str> = chunk.iter().copied().collect();
            let r = send(&s, "REPORT", &t.url, None, xml::multiget(card, chunk)).await?;
            let ms = multistatus(&s, &r)?;
            for resp in &ms.responses {
                // A per-href 404: deleted since the listing.
                if resp.status.is_some_and(|c| !(200..300).contains(&c)) {
                    continue;
                }
                let Some(okey) =
                    object_key(&t, &r.url, &resp.href).filter(|k| wanted.contains(k.as_str()))
                else {
                    continue;
                };
                if let Some(o) = resp
                    .prop(ns, data)
                    .and_then(|d| object(okey, resp.text(DAV, "getetag"), &d.text, media))
                {
                    out.push(o);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_stay_inside_the_collection() {
        let col = Col {
            origin: "https://h".into(),
            kind: EntityKind::Event,
            delta: Level::None,
        };
        let t = Target {
            key: "/cal/".into(),
            url: col.url("/cal/").unwrap(),
            col,
            origins: Vec::new(),
        };
        let key = |href: &str| object_key(&t, &t.url, href);
        assert_eq!(key("a%20b.ics").as_deref(), Some("/cal/a b.ics"));
        assert_eq!(key("https://h/cal/x.ics").as_deref(), Some("/cal/x.ics"));
        assert_eq!(key("/cal").as_deref(), Some("/cal"));
        for out in [
            "/other/x.ics",
            "/calx/y.ics",
            "../x.ics",
            "/",
            "https://evil/cal/x.ics",
            "http://h/cal/x.ics",
        ] {
            assert_eq!(key(out), None, "{out}");
        }
    }
}
