//! Google People API v1: `people.connections.list` and `people.get`
//! (plan 6.3, 6.4). Shapes follow Google's documentation; T10 has not
//! recorded live responses yet (FIDELITY.md).

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::{Path, Query, State as AxState};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Endpoint, FakeGoogle, Inner, Params, St, b64, bad_request, decode, encode, invalid_page_token,
    logical, not_found, ok, params, req_line, rpc_error,
};

/// Google's cap on `pageSize` for `people.connections.list`.
pub const MAX_CONNECTIONS_PAGE: u32 = 1000;

#[derive(Default)]
pub(super) struct People {
    /// By `resourceName`; deleted contacts stay as tombstones for deltas.
    contacts: BTreeMap<String, Contact>,
    /// Sync tokens below this version are expired.
    min_sync: u64,
}

struct Contact {
    body: Value,
    version: u64,
    deleted: bool,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct SyncToken {
    v: u64,
    fields: String,
}

/// Where a `connections.list` pass continues: after the last resource name
/// served, with the parameters of the first request and the version its
/// sync token will carry.
#[derive(Serialize, Deserialize, PartialEq)]
struct PageToken {
    after: String,
    params: Vec<(String, String)>,
    snapshot: u64,
}

/// `etag` of a person: opaque, changes with every write.
fn person_etag(v: u64) -> String {
    format!("%Eg{}", b64(&v.to_be_bytes()))
}

impl FakeGoogle {
    /// Replacement for `https://people.googleapis.com/v1`.
    pub fn people_api(&self) -> String {
        format!("{}/people/v1", self.base_url())
    }

    /// Inserts or replaces a contact. The server sets `etag` and
    /// `metadata` (one `CONTACT` source with its `updateTime`);
    /// `resourceName` is generated when missing. Returns the resource name.
    ///
    /// # Panics
    /// If `person` is not an object.
    pub fn put_contact(&self, mut person: Value) -> String {
        assert!(person.is_object(), "person must be a JSON object");
        let epoch = self.inner.config.epoch;
        let mut s = self.inner.lock();
        s.version += 1;
        let v = s.version;
        let name = match person["resourceName"].as_str() {
            Some(n) => n.to_owned(),
            None => format!("people/c{}", 1_000_000_000_000_000_000u64 + v),
        };
        person["resourceName"] = json!(name);
        person["etag"] = json!(person_etag(v));
        person["metadata"] = json!({
            "sources": [{
                "type": "CONTACT",
                "id": format!("{:x}", v),
                "etag": format!("#{}", b64(&v.to_le_bytes())),
                "updateTime": logical(epoch, v).to_string(),
            }],
            "objectType": "PERSON",
        });
        s.people.contacts.insert(
            name.clone(),
            Contact {
                body: person,
                version: v,
                deleted: false,
            },
        );
        name
    }

    /// `n` contacts with a name, an e-mail and a phone each; returns their
    /// resource names.
    pub fn seed_contacts(&self, n: u32) -> Vec<String> {
        (0..n)
            .map(|i| {
                self.put_contact(json!({
                    "names": [{ "displayName": format!("Contact {i}"), "givenName": "Contact", "familyName": format!("{i}") }],
                    "emailAddresses": [{ "value": format!("contact{i}@example.com"), "type": "home" }],
                    "phoneNumbers": [{ "value": format!("+1 202-555-01{:02}", i % 100), "type": "mobile" }],
                }))
            })
            .collect()
    }

    /// Deletes a contact: deltas list it with `metadata.deleted: true`.
    ///
    /// # Panics
    /// If the contact does not exist.
    pub fn delete_contact(&self, resource_name: &str) {
        let mut s = self.inner.lock();
        s.version += 1;
        let v = s.version;
        let c = s
            .people
            .contacts
            .get_mut(resource_name)
            .expect("unknown contact");
        c.body = json!({
            "resourceName": resource_name,
            "etag": person_etag(v),
            "metadata": { "deleted": true, "objectType": "PERSON" },
        });
        c.version = v;
        c.deleted = true;
    }

    /// Expires every People sync token issued so far (Google: 7 days after
    /// the full read that issued it).
    pub fn expire_people_sync_tokens(&self) {
        let mut s = self.inner.lock();
        // Tokens issued from now on carry the bumped version and stay valid.
        s.version += 1;
        s.people.min_sync = s.version;
    }
}

/// Live Google's answer to an expired sync token, as documented: `400
/// FAILED_PRECONDITION` with `ErrorInfo.reason = EXPIRED_SYNC_TOKEN` (the
/// status, 400 or 410, is unverified; T10).
fn expired_sync_token() -> Response {
    rpc_error(
        StatusCode::BAD_REQUEST,
        "FAILED_PRECONDITION",
        "people.googleapis.com",
        "EXPIRED_SYNC_TOKEN",
        "Sync token is expired. Clear local cache and retry call without the sync token.",
    )
}

/// The person with only the requested fields (Google's `personFields` mask).
fn masked(body: &Value, fields: &BTreeSet<&str>) -> Value {
    let mut out = serde_json::Map::new();
    for (k, v) in body.as_object().into_iter().flatten() {
        if matches!(k.as_str(), "resourceName" | "etag") || fields.contains(k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    // A tombstone keeps `metadata.deleted` whatever the mask.
    if body["metadata"]["deleted"] == json!(true) {
        out.insert("metadata".into(), body["metadata"].clone());
    }
    Value::Object(out)
}

fn person_fields(p: &BTreeMap<String, String>) -> Result<BTreeSet<&str>, Response> {
    match p.get("personFields").filter(|f| !f.is_empty()) {
        Some(f) => Ok(f.split(',').collect()),
        None => Err(bad_request("personFields mask is required")),
    }
}

pub(super) async fn connections(
    AxState(g): St,
    uri: Uri,
    h: HeaderMap,
    Query(q): Params,
) -> Response {
    if let Err(r) = g
        .enter(Endpoint::PeopleConnections, req_line("GET", &uri), &h)
        .await
    {
        return r;
    }
    connections_inner(&g, q).unwrap_or_else(|r| r)
}

fn connections_inner(g: &Inner, q: Vec<(String, String)>) -> Result<Response, Response> {
    let p = params(
        q,
        &[
            "personFields",
            "pageSize",
            "pageToken",
            "requestSyncToken",
            "syncToken",
        ],
    )?;
    let fields = person_fields(&p)?;
    let size = match p.get("pageSize") {
        None => 100,
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=MAX_CONNECTIONS_PAGE).contains(n))
            .ok_or_else(|| bad_request("Invalid pageSize"))?,
    }
    .min(g.config.max_page_size.max(1)) as usize;
    let request_token = match p.get("requestSyncToken").map(String::as_str) {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(bad_request("Invalid requestSyncToken")),
    };
    let first: Vec<(String, String)> = p
        .iter()
        .filter(|(k, _)| k.as_str() != "pageToken")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let s = g.lock();
    let since = match p.get("syncToken") {
        None => None,
        Some(t) => match decode::<SyncToken>(t) {
            Some(t) if t.v < s.people.min_sync => return Err(expired_sync_token()),
            Some(t) if t.fields == p["personFields"] => Some(t.v),
            _ => return Err(bad_request("Invalid sync token")),
        },
    };
    let (after, snapshot) = match p.get("pageToken") {
        None => (None, s.version),
        Some(t) => match decode::<PageToken>(t) {
            Some(t) if t.params == first => (Some(t.after), t.snapshot),
            _ => return Err(invalid_page_token()),
        },
    };
    let mut matching = s
        .people
        .contacts
        .iter()
        .filter(|(name, _)| after.as_ref().is_none_or(|a| *name > a))
        .filter(|(_, c)| match since {
            // A delta lists every change since the token, deletions included.
            Some(v) => c.version > v,
            None => !c.deleted,
        });
    let page: Vec<(&String, &Contact)> = matching.by_ref().take(size).collect();
    let more = matching.next().is_some();
    let total = s.people.contacts.values().filter(|c| !c.deleted).count();
    let mut body = json!({
        "connections": page.iter().map(|(_, c)| masked(&c.body, &fields)).collect::<Vec<_>>(),
        "totalPeople": total,
        "totalItems": total,
    });
    match (more, page.last()) {
        (true, Some((last, _))) => {
            body["nextPageToken"] = json!(encode(&PageToken {
                after: (*last).clone(),
                params: first,
                snapshot,
            }));
        }
        _ if request_token => {
            body["nextSyncToken"] = json!(encode(&SyncToken {
                v: snapshot,
                fields: p["personFields"].clone(),
            }));
        }
        _ => {}
    }
    Ok(ok(body))
}

pub(super) async fn person(
    AxState(g): St,
    Path(id): Path<String>,
    uri: Uri,
    h: HeaderMap,
    Query(q): Params,
) -> Response {
    if let Err(r) = g
        .enter(Endpoint::PeopleGet, req_line("GET", &uri), &h)
        .await
    {
        return r;
    }
    let p = match params(q, &["personFields"]) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let fields = match person_fields(&p) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let s = g.lock();
    match s.people.contacts.get(&format!("people/{id}")) {
        Some(c) if !c.deleted => ok(masked(&c.body, &fields)),
        _ => not_found(),
    }
}
