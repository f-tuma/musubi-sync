//! Google Tasks (plan 6.2): task lists as [`EntityKind::Task`] collections,
//! `tasks.list` reads by `updatedMin`, and [`TasksCodec`].
//!
//! Tasks have no sync token and no push. A full read lists every task
//! (`showDeleted=false`, absence is deletion); a delta lists what changed
//! since the largest `updated` seen, minus [`OVERLAP`], with `showDeleted=true`
//! so `deleted: true` is a removal. Tasks seen again in the overlap carry the
//! same `etag`, and the engine skips them by validator.

use jiff::civil::DateTime;
use jiff::{SignedDuration, Timestamp};
use musubi_model::provider::{
    Access, BoxStream, Codec, Decoded, FetchHint, Pass, RemoteCollection,
};
use musubi_model::remote::MediaType;
use musubi_model::task::TaskProgress;
use musubi_model::{
    AccountCx, CalendarTask, ChangePage, CollectionRef, Cursor, EntityKind, Raw, RemoteKey,
    RemoteObject, SyncError, Task,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::provider::{
    GoogleProvider, body, error_for, excerpt, join, pages, permanent, raw_object,
};

/// Prefix of a task list's [`CollectionRef`]: `tasks/<tasklist id>`.
pub(crate) const PREFIX: &str = "tasks/";
/// Largest `tasks.list` page Google serves.
const MAX_PAGE: u32 = 100;
/// How far before the largest `updated` seen a delta starts again: tasks
/// changed in the same instant as the last one seen are not lost.
const OVERLAP: SignedDuration = SignedDuration::from_secs(60);

/// What the engine stores as [`Cursor`] for a task list (plan 3.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum TasksCursor {
    /// Mid full read: the next page and the largest `updated` so far.
    Full {
        page: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<Timestamp>,
        /// `maxResults` of the pass while `page` is set: a page token belongs
        /// to the parameters that issued it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u32>,
    },
    /// Delta from `ts`, the largest `updated` seen (`None`: the list had no
    /// task yet, the delta lists everything). `page` and `max` while a delta
    /// spans several pages.
    UpdatedMin {
        ts: Option<Timestamp>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<Timestamp>,
        /// As in `Full`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u32>,
    },
}

/// One `tasks.list` request of a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Query {
    pass: Pass,
    /// Delta: the watermark the pass started from.
    ts: Option<Timestamp>,
    page: Option<String>,
    /// Largest `updated` seen in this pass so far.
    max: Option<Timestamp>,
    /// `maxResults`, the same on every page of a pass.
    size: u32,
}

impl Query {
    /// Tasks have no window (plan, open question 5): every start without a
    /// cursor is a full read. `size` is the hint's page size; a pass resumed
    /// mid-way keeps its own.
    fn start(cursor: Option<Cursor>, size: u32) -> Result<Self, SyncError> {
        let Some(c) = cursor else {
            return Ok(Query {
                pass: Pass::Full,
                ts: None,
                page: None,
                max: None,
                size,
            });
        };
        match serde_json::from_slice(&c.0) {
            Ok(TasksCursor::Full {
                page,
                max,
                size: stored,
            }) => Ok(Query {
                pass: Pass::Full,
                ts: None,
                page: Some(page),
                max,
                size: stored.unwrap_or(size),
            }),
            Ok(TasksCursor::UpdatedMin {
                ts,
                page,
                max,
                size: stored,
            }) => Ok(Query {
                pass: Pass::Delta,
                ts,
                page,
                max,
                size: stored.unwrap_or(size),
            }),
            Err(e) => {
                tracing::warn!(error = %e, "unreadable task list cursor; full read");
                Err(SyncError::CursorExpired)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TasksPage {
    #[serde(default)]
    items: Vec<Box<RawValue>>,
    next_page_token: Option<String>,
}

/// The fields of a task the transport itself needs.
#[derive(Deserialize)]
struct TaskMeta {
    id: String,
    etag: Option<String>,
    updated: Option<Timestamp>,
    #[serde(default)]
    deleted: bool,
}

/// Turns one `tasks.list` response into a page and the next request.
fn build_page(q: &Query, body: &[u8]) -> Result<(ChangePage, Option<Query>), SyncError> {
    let resp: TasksPage =
        serde_json::from_slice(body).map_err(|e| permanent(format!("tasks.list: {e}")))?;
    let mut upserts = Vec::new();
    let mut removals = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut max = q.max;
    // Each key at most once per page; the last occurrence is the newest.
    for raw in resp.items.iter().rev() {
        let meta: TaskMeta =
            serde_json::from_str(raw.get()).map_err(|e| permanent(format!("task: {e}")))?;
        max = max.max(meta.updated);
        if !seen.insert(meta.id.clone()) {
            continue;
        }
        match (meta.deleted, q.pass) {
            // A full read does not ask for deleted tasks; absence deletes.
            (true, Pass::Full) => {}
            (true, _) => removals.push(RemoteKey(meta.id)),
            (false, _) => upserts.push(raw_object(
                raw,
                &meta.id,
                EntityKind::Task,
                meta.etag.as_deref(),
            )),
        }
    }
    upserts.reverse();
    removals.reverse();
    let (cursor, next) = match resp.next_page_token {
        Some(page) => {
            let cursor = match q.pass {
                Pass::Delta => TasksCursor::UpdatedMin {
                    ts: q.ts,
                    page: Some(page.clone()),
                    max,
                    size: Some(q.size),
                },
                _ => TasksCursor::Full {
                    page: page.clone(),
                    max,
                    size: Some(q.size),
                },
            };
            let next = Query {
                page: Some(page),
                max,
                ..q.clone()
            };
            (cursor, Some(next))
        }
        // Without results the watermark stays where it was.
        None => (
            TasksCursor::UpdatedMin {
                ts: max.max(q.ts),
                page: None,
                max: None,
                size: None,
            },
            None,
        ),
    };
    let cursor = Cursor(serde_json::to_vec(&cursor).unwrap_or_default());
    let mut page = ChangePage::new(q.pass, Some(cursor), next.is_none());
    page.upserts = upserts;
    page.removals = removals;
    Ok((page, next))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskLists {
    #[serde(default)]
    items: Vec<TaskList>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct TaskList {
    id: String,
    title: Option<String>,
}

impl GoogleProvider {
    fn tasks_url(&self, segments: &[&str]) -> reqwest::Url {
        join(&self.config.tasks_base, segments)
    }

    /// `tasklists.list`, every page, each list appended to `found` as it
    /// arrives. A missing scope is [`SyncError::MissingScope`].
    pub(crate) async fn task_lists(
        &self,
        cx: &AccountCx,
        found: &mut Vec<RemoteCollection>,
    ) -> Result<(), SyncError> {
        let mut page: Option<String> = None;
        loop {
            let mut url = self.tasks_url(&["users", "@me", "lists"]);
            url.query_pairs_mut().append_pair("maxResults", "1000");
            if let Some(p) = &page {
                url.query_pairs_mut().append_pair("pageToken", p);
            }
            let body = self
                .call(cx, self.http.get(url), Some(EntityKind::Task))
                .await?;
            let lists: TaskLists = serde_json::from_slice(&body)
                .map_err(|e| permanent(format!("tasklists.list: {e}")))?;
            found.extend(lists.items.into_iter().map(|l| {
                RemoteCollection::new(
                    CollectionRef(format!("{PREFIX}{}", l.id)),
                    EntityKind::Task,
                    l.title.unwrap_or_default(),
                    Access::Owner,
                )
            }));
            match lists.next_page_token {
                Some(next) => page = Some(next),
                None => return Ok(()),
            }
        }
    }

    pub(crate) fn task_changes<'a>(
        &'a self,
        cx: &'a AccountCx,
        list: &'a str,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> BoxStream<'a, Result<ChangePage, SyncError>> {
        let size = hint.page_size.map_or(MAX_PAGE, |n| n.get().min(MAX_PAGE));
        pages(Query::start(cursor, size), move |q| {
            self.tasks_page(cx, list, q)
        })
    }

    /// One `tasks.list` request.
    async fn tasks_page(
        &self,
        cx: &AccountCx,
        list: &str,
        q: Query,
    ) -> Result<(ChangePage, Option<Query>), SyncError> {
        let mut url = self.tasks_url(&["lists", list, "tasks"]);
        {
            // A page request repeats every parameter of the first one.
            let mut p = url.query_pairs_mut();
            p.append_pair("maxResults", &q.size.to_string());
            p.append_pair("showCompleted", "true");
            p.append_pair("showHidden", "true");
            let delta = q.pass == Pass::Delta;
            p.append_pair("showDeleted", if delta { "true" } else { "false" });
            if let Some(ts) = q.ts.filter(|_| delta) {
                p.append_pair("updatedMin", &(ts - OVERLAP).to_string());
            }
            if let Some(page) = &q.page {
                p.append_pair("pageToken", page);
            }
        }
        let resp = self.send(cx, self.http.get(url)).await?;
        let status = resp.status();
        // Assumed shape (T10): a rejected page token is a 400. The pass starts
        // over as a full read; a 400 without a page token is a real error.
        if status == StatusCode::BAD_REQUEST && q.page.is_some() {
            let body = resp.bytes().await.unwrap_or_default();
            tracing::warn!(body = %excerpt(&body), pass = ?q.pass, "tasks.list: page token rejected; full read");
            return Err(SyncError::CursorExpired);
        }
        if !status.is_success() {
            return Err(error_for(resp, cx.clock.now(), Some(EntityKind::Task)).await);
        }
        build_page(&q, &body(resp).await?)
    }

    /// `tasks.get`; `None` for a task that is gone or deleted.
    pub(crate) async fn fetch_task(
        &self,
        cx: &AccountCx,
        list: &str,
        key: &RemoteKey,
    ) -> Result<Option<RemoteObject>, SyncError> {
        let url = self.tasks_url(&["lists", list, "tasks", &key.0]);
        let resp = self.send(cx, self.http.get(url)).await?;
        match resp.status() {
            StatusCode::NOT_FOUND | StatusCode::GONE => return Ok(None),
            s if !s.is_success() => {
                return Err(error_for(resp, cx.clock.now(), Some(EntityKind::Task)).await);
            }
            _ => {}
        }
        let body = body(resp).await?;
        let raw: Box<RawValue> =
            serde_json::from_slice(&body).map_err(|e| permanent(format!("tasks.get: {e}")))?;
        let meta: TaskMeta =
            serde_json::from_str(raw.get()).map_err(|e| permanent(format!("task: {e}")))?;
        Ok((!meta.deleted)
            .then(|| raw_object(&raw, &meta.id, EntityKind::Task, meta.etag.as_deref())))
    }
}

/// `Codec<Task>` for Google Tasks `tasks` resources.
///
/// `id` → `uid`, `title`, `notes` → `description`, `due` (a date; Google
/// drops the time) → `due` at midnight with `showWithoutTime`, `status` →
/// `progress`, `completed` → `progressUpdated`, `hidden`, `parent` →
/// `relatedTo`, `updated`. `position`, `links`, `webViewLink` and
/// `assignmentInfo` stay in the raw layer only.
#[derive(Debug, Clone, Copy, Default)]
pub struct TasksCodec;

impl Codec<Task> for TasksCodec {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<CalendarTask> {
        match decode(raw) {
            Ok(task) => Decoded::Canonical(task),
            Err(reason) => Decoded::Unsupported { reason },
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GTask {
    id: Option<String>,
    title: Option<String>,
    notes: Option<String>,
    status: Option<String>,
    due: Option<String>,
    completed: Option<String>,
    updated: Option<String>,
    parent: Option<String>,
    hidden: Option<bool>,
}

fn decode(raw: &Raw) -> Result<CalendarTask, String> {
    if raw.media_type != MediaType::Json {
        return Err(format!("unexpected media type {:?}", raw.media_type));
    }
    let g: GTask =
        serde_json::from_slice(&raw.body).map_err(|e| format!("not a Google task: {e}"))?;
    let id = g.id.filter(|id| !id.is_empty()).ok_or("task without id")?;
    let instant = |name: &str, v: Option<String>| -> Result<Option<Timestamp>, String> {
        v.map(|v| v.parse().map_err(|e| format!("{name}: {e}")))
            .transpose()
    };
    let mut t = CalendarTask::new(id);
    t.title = g.title.unwrap_or_default();
    t.description = g.notes.unwrap_or_default();
    // Google keeps only the date of `due` (`2026-11-20T00:00:00.000Z`).
    t.due = instant("due", g.due)?
        .map(|due| DateTime::from(due.to_zoned(jiff::tz::TimeZone::UTC).date()));
    t.show_without_time = true;
    t.progress = match g.status.as_deref() {
        Some("completed") => TaskProgress::Completed,
        _ => TaskProgress::NeedsAction,
    };
    t.progress_updated = instant("completed", g.completed)?;
    t.hidden = g.hidden.unwrap_or(false);
    t.related_to = g.parent;
    t.updated = instant("updated", g.updated)?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pass: Pass, ts: Option<&str>) -> Query {
        Query {
            pass,
            ts: ts.map(|t| t.parse().unwrap()),
            page: None,
            max: None,
            size: 2,
        }
    }

    fn cursor(page: &ChangePage) -> TasksCursor {
        serde_json::from_slice(&page.cursor.as_ref().unwrap().0).unwrap()
    }

    #[test]
    fn watermark_is_the_largest_updated_across_pages_and_stays_without_results() {
        let p1 = br#"{"items":[
            {"id":"a","etag":"\"1\"","updated":"2026-11-11T10:00:00.000Z"},
            {"id":"b","etag":"\"2\"","updated":"2026-11-11T12:00:00.000Z"}
        ],"nextPageToken":"p2"}"#;
        let (page, next) = build_page(&query(Pass::Full, None), p1).unwrap();
        assert!(!page.last && page.upserts.len() == 2);
        // A resumed pass keeps its page size whatever the new hint says.
        let resumed = Query::start(page.cursor.clone(), 50).unwrap();
        assert_eq!(Some(resumed.clone()), next);
        let p2 = br#"{"items":[{"id":"c","updated":"2026-11-11T11:00:00.000Z"}]}"#;
        let (page, next) = build_page(&resumed, p2).unwrap();
        assert!(page.last && next.is_none());
        assert!(page.upserts[0].validator.0.starts_with("sha256:"));
        let ts: Timestamp = "2026-11-11T12:00:00Z".parse().unwrap();
        assert_eq!(
            cursor(&page),
            TasksCursor::UpdatedMin {
                ts: Some(ts),
                page: None,
                max: None,
                size: None,
            }
        );
        let delta = Query::start(page.cursor, 50).unwrap();
        assert_eq!(delta.pass, Pass::Delta);
        let (page, _) = build_page(&delta, br#"{"kind":"tasks#tasks"}"#).unwrap();
        assert_eq!(Query::start(page.cursor, 50).unwrap().ts, Some(ts));
    }

    #[test]
    fn deleted_tasks_are_removals_only_in_a_delta() {
        let body = br#"{"items":[
            {"id":"a","etag":"\"1\"","updated":"2026-11-11T10:00:00.000Z"},
            {"id":"gone","deleted":true,"updated":"2026-11-11T10:01:00.000Z"},
            {"id":"a","etag":"\"3\"","updated":"2026-11-11T10:02:00.000Z"}
        ]}"#;
        let (page, _) =
            build_page(&query(Pass::Delta, Some("2026-11-11T09:00:00Z")), body).unwrap();
        assert_eq!(page.upserts.len(), 1);
        assert_eq!(page.upserts[0].validator.0, r#""3""#);
        assert_eq!(page.removals, [RemoteKey("gone".into())]);
        let (page, _) = build_page(&query(Pass::Full, None), body).unwrap();
        assert!(page.removals.is_empty());
    }

    #[test]
    fn unreadable_cursor_is_expired() {
        assert!(matches!(
            Query::start(Some(Cursor(b"{\"sync\":1}".to_vec())), 100),
            Err(SyncError::CursorExpired)
        ));
    }
}
