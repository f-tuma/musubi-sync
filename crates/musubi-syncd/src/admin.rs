//! Instance administration: tenants, API keys, storage statistics, and the
//! `backup` (running node) / `restore` (stopped node) operations.
//!
//! A backup directory holds `control.db` (`VACUUM INTO`, a consistent online
//! copy) and `tenants/<id>/main.db` (each through [`Storage::backup`]).
//! `control.db` is copied first, so a tenant created during the backup is at
//! worst an orphan directory without keys, never a key without data.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use jiff::Timestamp;
use musubi_model::control::{ApiKeyHash, ApiKeyRecord, Scope};
use musubi_model::{ControlStore, Storage, StoreError, TenantId, TenantStore};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::crypto::{random, random_token, sha256};
use crate::read::hex;
use crate::{ApiError, App, Caller, lock_data_dir};

type Created = Result<(StatusCode, Json<Value>), ApiError>;

/// `POST /v1/admin/tenants`.
pub(crate) async fn create_tenant(State(app): State<Arc<App>>, caller: Caller) -> Created {
    caller.admin()?;
    let id = TenantId::from_bytes(random());
    app.run(move |a| {
        a.control.insert_tenant(id)?;
        if let Err(e) = a.storage.create_tenant(id) {
            let _ = a.control.delete_tenant(id);
            return Err(e.into());
        }
        Ok(())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

/// `DELETE /v1/admin/tenants/{id}`: data, accounts, keys and jobs.
// Watch channels stay open at Google until they expire: stopping them needs the
// account's tokens and the adapter (K6 step 2). Their notifications then find
// no channel and answer 404.
pub(crate) async fn delete_tenant(
    State(app): State<Arc<App>>,
    caller: Caller,
    UrlPath(id): UrlPath<TenantId>,
) -> Result<StatusCode, ApiError> {
    caller.admin()?;
    app.run(move |a| {
        a.control.delete_tenant(id)?;
        match a.storage.delete_tenant(id) {
            Ok(()) | Err(StoreError::NotFound) => Ok(()),
            Err(e) => Err(e.into()),
        }
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyRequest {
    scope: Scope,
}

/// `POST /v1/admin/tenants/{id}/keys`: the key is shown once; only its
/// SHA-256 is stored, and its hex is the key's id.
pub(crate) async fn create_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    UrlPath(tenant): UrlPath<TenantId>,
    Json(req): Json<KeyRequest>,
) -> Created {
    caller.admin()?;
    let key = format!("mk_{}", random_token());
    let hash = sha256(key.as_bytes());
    app.run(move |a| {
        // The control schema has no foreign keys: check the tenant here.
        a.storage.open(tenant)?;
        Ok(a.control.insert_api_key(&ApiKeyRecord {
            hash: ApiKeyHash(hash),
            tenant,
            scope: req.scope,
            created: Timestamp::now(),
            revoked: None,
        })?)
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "key": key, "id": hex(&hash), "scope": req.scope })),
    ))
}

/// `DELETE /v1/admin/keys/{id}`.
pub(crate) async fn revoke_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    caller.admin()?;
    let hash = unhex32(&id).ok_or_else(ApiError::not_found)?;
    app.run(move |a| {
        Ok(a.control
            .revoke_api_key(&ApiKeyHash(hash), Timestamp::now())?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// `GET /v1/admin/tenants/{id}/stats`.
pub(crate) async fn stats(
    State(app): State<Arc<App>>,
    caller: Caller,
    UrlPath(tenant): UrlPath<TenantId>,
) -> Result<Json<Value>, ApiError> {
    caller.admin()?;
    let s = app
        .run(move |a| Ok(a.storage.open(tenant)?.stats()?))
        .await?;
    Ok(Json(json!({
        "head": s.head,
        "main_bytes": s.main_bytes,
        "wal_bytes": s.wal_bytes,
        "wal_max_bytes": s.wal_max_bytes,
        "truncates": s.truncates,
        "truncate_max_ms": s.truncate_max.as_millis() as u64,
        "reset_failures": s.reset_failures,
        "items": s.items,
        "entities": s.entities,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackupRequest {
    /// Absolute path on the node; must not exist yet.
    to: PathBuf,
}

/// `POST /v1/admin/backup`.
pub(crate) async fn backup_route(
    State(app): State<Arc<App>>,
    caller: Caller,
    Json(req): Json<BackupRequest>,
) -> Created {
    caller.admin()?;
    if !req.to.is_absolute() {
        return Err(ApiError::bad_request("to must be an absolute path"));
    }
    let tenants = app.run(move |a| backup_into(a, &req.to)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "tenants": tenants }))))
}

fn backup_into(app: &App, to: &Path) -> Result<usize, ApiError> {
    let io = |e: std::io::Error| ApiError::bad_request(format!("{}: {e}", to.display()));
    // `create_dir` (not `_all`) refuses an existing destination.
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    fs::create_dir(to).map_err(io)?;
    fs::create_dir(to.join("tenants")).map_err(io)?;

    let conn = rusqlite::Connection::open(app.data_dir.join("control.db"))
        .map_err(|e| ApiError::internal(&e))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| ApiError::internal(&e))?;
    let dest = to.join("control.db");
    let dest = dest
        .to_str()
        .ok_or_else(|| ApiError::bad_request("to must be valid UTF-8"))?;
    conn.execute("VACUUM INTO ?1", [dest])
        .map_err(|e| ApiError::internal(&e))?;

    let mut n = 0;
    for t in tenant_dirs(&app.data_dir.join("tenants")).map_err(|e| ApiError::internal(&e))? {
        let out = to.join("tenants").join(t.to_string());
        fs::create_dir(&out).map_err(io)?;
        match app.storage.backup(t, &out) {
            Ok(_) => n += 1,
            // Deleted meanwhile.
            Err(StoreError::NotFound) => fs::remove_dir(&out).map_err(io)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}

/// Tenant directories (named by their id; temporary ones are skipped).
fn tenant_dirs(dir: &Path) -> std::io::Result<Vec<TenantId>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_dir()
            && let Some(id) = e.file_name().to_str().and_then(|n| n.parse().ok())
        {
            out.push(id);
        }
    }
    Ok(out)
}

/// `musubi-syncd backup`: asks the running node for a backup into `to`.
pub async fn backup(c: &Config, to: &Path) -> Result<(), String> {
    let to = std::path::absolute(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let mut addr = c.listen;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    // reqwest is built without a default TLS provider (as in musubi-google).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/admin/backup"))
        .bearer_auth(&c.admin_key)
        .json(&json!({ "to": to }))
        .send()
        .await
        .map_err(|e| format!("node at {addr}: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("backup failed ({status}): {body}"));
    }
    println!("{body}");
    Ok(())
}

/// `musubi-syncd restore`: fills an empty data directory from a backup. Takes
/// the data directory lock, so it fails while a node runs there.
pub fn restore(c: &Config, from: &Path) -> Result<(), String> {
    let data = &c.data_dir;
    fs::create_dir_all(data).map_err(|e| format!("{}: {e}", data.display()))?;
    let _lock = lock_data_dir(data)?;
    let io = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    for e in fs::read_dir(data).map_err(|e| io(data, e))? {
        let e = e.map_err(|e| io(data, e))?;
        if e.file_name() != "musubi.lock" {
            return Err(format!(
                "{} is not empty; restore only into an empty data directory",
                data.display()
            ));
        }
    }
    let src = from.join("control.db");
    fs::copy(&src, data.join("control.db")).map_err(|e| io(&src, e))?;
    let tenants = data.join("tenants");
    fs::create_dir(&tenants).map_err(|e| io(&tenants, e))?;
    let from_tenants = from.join("tenants");
    let ids = tenant_dirs(&from_tenants).map_err(|e| io(&from_tenants, e))?;
    for t in &ids {
        // Copied aside and renamed, like `create_tenant`; storage start-up
        // removes a `.new` left by a crash.
        let tmp = tenants.join(format!(".{t}.restore.new"));
        fs::create_dir(&tmp).map_err(|e| io(&tmp, e))?;
        let src = from_tenants.join(t.to_string());
        for f in fs::read_dir(&src).map_err(|e| io(&src, e))? {
            let f = f.map_err(|e| io(&src, e))?;
            fs::copy(f.path(), tmp.join(f.file_name())).map_err(|e| io(&f.path(), e))?;
        }
        let dst = tenants.join(t.to_string());
        fs::rename(&tmp, &dst).map_err(|e| io(&dst, e))?;
    }
    println!(
        "restored control.db and {} tenants into {}",
        ids.len(),
        data.display()
    );
    Ok(())
}
