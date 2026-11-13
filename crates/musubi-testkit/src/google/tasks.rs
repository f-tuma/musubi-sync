//! Google Tasks API v1: `tasklists.list`, `tasks.list` and `tasks.get`
//! (plan 6.2, 6.4). Shapes follow Google's documentation; T10 has not
//! recorded live responses yet (FIDELITY.md).

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State as AxState};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Endpoint, FakeGoogle, Inner, Params, St, State, b32hex, bad_request, decode, encode, etag,
    invalid_page_token, logical, max_results, merge_defaults, not_found, ok, params, req_line,
};

/// Google's cap on `maxResults` for `tasks.list` and `tasklists.list`.
pub const MAX_TASKS_PAGE: u32 = 100;
/// Google's default and cap for `maxResults` of `tasklists.list`.
pub const MAX_TASK_LISTS_PAGE: u32 = 1000;

pub(super) struct TaskList {
    title: String,
    /// Version of the last change of the list itself.
    changed: u64,
    tasks: BTreeMap<String, TaskEntry>,
}

struct TaskEntry {
    body: Value,
    version: u64,
}

/// Where a `tasks.list` pass continues: after the last id served, with the
/// filters of the first request (Google rejects a token used with others).
#[derive(Serialize, Deserialize, PartialEq)]
struct PageToken {
    list: String,
    after: String,
    filters: Vec<(String, String)>,
}

impl FakeGoogle {
    /// Replacement for `https://tasks.googleapis.com/tasks/v1`.
    pub fn tasks_api(&self) -> String {
        format!("{}/tasks/v1", self.base_url())
    }

    /// Creates an empty task list; returns its id.
    pub fn put_task_list(&self, title: &str) -> String {
        let mut s = self.inner.lock();
        s.version += 1;
        let id = format!(
            "MDk{}",
            b32hex(s.version.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        );
        let list = TaskList {
            title: title.to_owned(),
            changed: s.version,
            tasks: BTreeMap::new(),
        };
        s.task_lists.insert(id.clone(), list);
        id
    }

    /// Inserts or replaces a task. The server sets `kind`, `etag`, `updated`,
    /// `selfLink` and `position`, and defaults `status` to `needsAction`; `id`
    /// is generated when missing. Returns the task id.
    ///
    /// # Panics
    /// If the list does not exist or `task` is not an object.
    pub fn put_task(&self, list: &str, mut task: Value) -> String {
        assert!(task.is_object(), "task must be a JSON object");
        let epoch = self.inner.config.epoch;
        let mut s = self.inner.lock();
        s.version += 1;
        let v = s.version;
        let id = match task["id"].as_str() {
            Some(id) => id.to_owned(),
            None => format!("T{}", b32hex(v.wrapping_mul(0x9E37_79B9_7F4A_7C15))),
        };
        task["kind"] = json!("tasks#task");
        task["id"] = json!(id);
        task["etag"] = json!(etag(v));
        task["updated"] = json!(logical(epoch, v).to_string());
        task["selfLink"] = json!(format!(
            "https://www.googleapis.com/tasks/v1/lists/{list}/tasks/{id}"
        ));
        merge_defaults(
            &mut task,
            json!({ "title": "", "status": "needsAction", "position": format!("{v:020}"), "links": [] }),
        );
        let l = s.task_lists.get_mut(list).expect("unknown task list");
        l.tasks.insert(
            id.clone(),
            TaskEntry {
                body: task,
                version: v,
            },
        );
        id
    }

    /// Deletes a task: it stays with `deleted: true` for `showDeleted`.
    ///
    /// # Panics
    /// If the list or the task does not exist.
    pub fn delete_task(&self, list: &str, id: &str) {
        let epoch = self.inner.config.epoch;
        let mut s = self.inner.lock();
        s.version += 1;
        let v = s.version;
        let l = s.task_lists.get_mut(list).expect("unknown task list");
        let t = l.tasks.get_mut(id).expect("unknown task");
        t.body["deleted"] = json!(true);
        t.body["etag"] = json!(etag(v));
        t.body["updated"] = json!(logical(epoch, v).to_string());
        t.version = v;
    }
}

impl State {
    fn task_list_entry(&self, id: &str, l: &TaskList, epoch: Timestamp) -> Value {
        json!({
            "kind": "tasks#taskList",
            "id": id,
            "etag": etag(l.changed),
            "title": l.title,
            "updated": logical(epoch, l.changed).to_string(),
            "selfLink": format!("https://www.googleapis.com/tasks/v1/users/@me/lists/{id}"),
        })
    }
}

pub(super) async fn task_lists(
    AxState(g): St,
    uri: Uri,
    h: HeaderMap,
    Query(q): Params,
) -> Response {
    if let Err(r) = g
        .enter(Endpoint::TaskLists, req_line("GET", &uri), &h)
        .await
    {
        return r;
    }
    let p = match params(q, &["maxResults", "pageToken"]) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let size = match max_results(&p, MAX_TASK_LISTS_PAGE, MAX_TASK_LISTS_PAGE) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let start = match p.get("pageToken").map(|t| decode::<usize>(t)) {
        None => 0,
        Some(Some(n)) => n,
        Some(None) => return invalid_page_token(),
    };
    let s = g.lock();
    let items: Vec<Value> = s
        .task_lists
        .iter()
        .skip(start)
        .take(size)
        .map(|(id, l)| s.task_list_entry(id, l, g.config.epoch))
        .collect();
    let mut body = json!({ "kind": "tasks#taskLists", "etag": etag(s.version), "items": items });
    if start + size < s.task_lists.len() {
        body["nextPageToken"] = json!(encode(&(start + size)));
    }
    ok(body)
}

pub(super) async fn tasks_list(
    AxState(g): St,
    Path(list): Path<String>,
    uri: Uri,
    h: HeaderMap,
    Query(q): Params,
) -> Response {
    if let Err(r) = g
        .enter(Endpoint::TasksList, req_line("GET", &uri), &h)
        .await
    {
        return r;
    }
    tasks_list_inner(&g, &list, q).unwrap_or_else(|r| r)
}

fn tasks_list_inner(g: &Inner, list: &str, q: Vec<(String, String)>) -> Result<Response, Response> {
    let p = params(
        q,
        &[
            "maxResults",
            "pageToken",
            "showCompleted",
            "showDeleted",
            "showHidden",
            "updatedMin",
        ],
    )?;
    // Google's defaults: completed tasks shown, deleted and hidden ones not.
    let flag = |k: &str, default: bool| match p.get(k).map(String::as_str) {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(_) => Err(bad_request(&format!("Invalid {k}"))),
    };
    let (completed, deleted, hidden) = (
        flag("showCompleted", true)?,
        flag("showDeleted", false)?,
        flag("showHidden", false)?,
    );
    let updated_min: Option<Timestamp> = p
        .get("updatedMin")
        .map(|v| v.parse().map_err(|_| bad_request("Invalid updatedMin")))
        .transpose()?;
    let size = max_results(&p, 20, MAX_TASKS_PAGE)?.min(g.config.max_page_size.max(1) as usize);
    let filters: Vec<(String, String)> = p
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "pageToken" | "maxResults"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let after = match p.get("pageToken") {
        None => None,
        Some(t) => match decode::<PageToken>(t) {
            Some(t) if t.list == list && t.filters == filters => Some(t.after),
            _ => return Err(invalid_page_token()),
        },
    };
    let s = g.lock();
    let l = s.task_lists.get(list).ok_or_else(not_found)?;
    let epoch = g.config.epoch;
    let mut matching = l
        .tasks
        .iter()
        .filter(|(id, _)| after.as_ref().is_none_or(|a| *id > a))
        .filter(|(_, t)| {
            let b = &t.body;
            (deleted || b["deleted"] != json!(true))
                && (hidden || b["hidden"] != json!(true))
                && (completed || b["status"] != json!("completed"))
                && updated_min.is_none_or(|m| logical(epoch, t.version) >= m)
        });
    let items: Vec<(&String, &TaskEntry)> = matching.by_ref().take(size).collect();
    let more = matching.next().is_some();
    let mut body = json!({
        "kind": "tasks#tasks",
        "etag": etag(s.version),
        "items": items.iter().map(|(_, t)| t.body.clone()).collect::<Vec<_>>(),
    });
    if let (true, Some((last, _))) = (more, items.last()) {
        body["nextPageToken"] = json!(encode(&PageToken {
            list: list.to_owned(),
            after: (*last).clone(),
            filters,
        }));
    }
    Ok(ok(body))
}

pub(super) async fn tasks_get(
    AxState(g): St,
    Path((list, id)): Path<(String, String)>,
    uri: Uri,
    h: HeaderMap,
    Query(q): Params,
) -> Response {
    if let Err(r) = g.enter(Endpoint::TasksGet, req_line("GET", &uri), &h).await {
        return r;
    }
    if let Err(r) = params(q, &[]) {
        return r;
    }
    let s = g.lock();
    match s.task_lists.get(&list).and_then(|l| l.tasks.get(&id)) {
        Some(t) => ok(t.body.clone()),
        None => not_found(),
    }
}
