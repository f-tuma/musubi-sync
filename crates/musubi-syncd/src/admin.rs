//! Instance administration: tenants, API keys, storage statistics, and the
//! `backup` (running node) / `restore` (stopped node) operations.
//!
//! A backup directory holds `control.db` (`VACUUM INTO`, a consistent online
//! copy) and `tenants/<id>/main.db` (each through [`Storage::backup`]).
//! `control.db` is copied first, so a tenant created during the backup is at
//! worst an orphan directory without keys, never a key without data.
//!
//! Backups, restored data and the data directory hold secrets: directories
//! are created `0700`, copied database files set to `0600`.

use std::fs;
use std::io;
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

/// `DELETE /v1/admin/tenants/{id}`: accounts, data, keys and jobs.
///
/// Each account is removed through the engine first: it stops the account's
/// watch channels at Google while the account's tokens still exist, then
/// deletes them. Only then go the tenant's rows, store and commit channel.
///
/// The tenant is marked first, so an OAuth callback finishing meanwhile does
/// not add an account after the listing. The work runs in its own task: a
/// client that gives up does not cut it short.
pub(crate) async fn delete_tenant(
    State(app): State<Arc<App>>,
    caller: Caller,
    UrlPath(id): UrlPath<TenantId>,
) -> Result<StatusCode, ApiError> {
    caller.admin()?;
    tokio::spawn(async move {
        app.deleting().insert(id);
        let res = remove_tenant(&app, id).await;
        app.deleting().remove(&id);
        res
    })
    .await
    .map_err(|e| ApiError::internal(&e))??;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_tenant(app: &Arc<App>, id: TenantId) -> Result<(), ApiError> {
    let accounts = app.run(move |a| Ok(a.control.accounts(id)?)).await?;
    for acc in accounts {
        app.engine
            .remove_account(acc.id)
            .await
            .map_err(|e| ApiError::internal(&e))?;
    }
    app.run(move |a| {
        a.control.delete_tenant(id)?;
        a.engine.forget_tenant(id);
        a.bus.remove(id);
        match a.storage.delete_tenant(id) {
            Ok(()) | Err(StoreError::NotFound) => Ok(()),
            Err(e) => Err(e.into()),
        }
    })
    .await
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
        private_dir_all(parent).map_err(io)?;
    }
    private_dir(to).map_err(io)?;
    private_dir(&to.join("tenants")).map_err(io)?;

    let conn = rusqlite::Connection::open(app.data_dir.join("control.db"))
        .map_err(|e| ApiError::internal(&e))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| ApiError::internal(&e))?;
    let dest = to.join("control.db");
    let dest = dest
        .to_str()
        .ok_or_else(|| ApiError::bad_request("to must be valid UTF-8"))?;
    // Created private and empty first: `VACUUM INTO` fills an empty file.
    private_new_file(Path::new(dest)).map_err(io)?;
    conn.execute("VACUUM INTO ?1", [dest])
        .map_err(|e| ApiError::internal(&e))?;

    let mut n = 0;
    for t in tenant_dirs(&app.data_dir.join("tenants")).map_err(|e| ApiError::internal(&e))? {
        let out = to.join("tenants").join(t.to_string());
        private_dir(&out).map_err(io)?;
        match app.storage.backup(t, &out) {
            Ok(_) => {
                // The store names its files; the `0700` directory covers them until then.
                for f in fs::read_dir(&out).map_err(io)? {
                    set_mode(&f.map_err(io)?.path(), 0o600).map_err(io)?;
                }
                n += 1;
            }
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
/// the data directory lock, so it fails while a node runs there. Each tenant
/// and `control.db` (last) are copied aside, synced and renamed into place;
/// after a failure the directory holds a partial restore, which has to be
/// emptied by hand before running `restore` again.
pub fn restore(c: &Config, from: &Path) -> Result<(), String> {
    let data = &c.data_dir;
    private_dir_all(data)
        .and_then(|()| set_mode(data, 0o700))
        .map_err(|e| format!("{}: {e}", data.display()))?;
    let _lock = lock_data_dir(data)?;
    for e in fs::read_dir(data).map_err(|e| at(data, e))? {
        let e = e.map_err(|e| at(data, e))?;
        if e.file_name() != "musubi.lock" {
            return Err(format!(
                "{} is not empty; restore only into an empty data directory",
                data.display()
            ));
        }
    }
    let n = fill(data, from).map_err(|e| {
        format!(
            "{e}; {} now holds a partial restore: empty it (keep nothing) before retrying",
            data.display()
        )
    })?;
    println!(
        "restored control.db and {n} tenants into {}",
        data.display()
    );
    Ok(())
}

fn at(p: &Path, e: io::Error) -> String {
    format!("{}: {e}", p.display())
}

fn fill(data: &Path, from: &Path) -> Result<usize, String> {
    let tenants = data.join("tenants");
    private_dir(&tenants).map_err(|e| at(&tenants, e))?;
    let from_tenants = from.join("tenants");
    let ids = tenant_dirs(&from_tenants).map_err(|e| at(&from_tenants, e))?;
    for t in &ids {
        // Storage start-up removes a `.new` left by a crash.
        let tmp = tenants.join(format!(".{t}.restore.new"));
        private_dir(&tmp).map_err(|e| at(&tmp, e))?;
        let src = from_tenants.join(t.to_string());
        for f in fs::read_dir(&src).map_err(|e| at(&src, e))? {
            let f = f.map_err(|e| at(&src, e))?;
            copy_synced(&f.path(), &tmp.join(f.file_name()))?;
        }
        sync_dir(&tmp)?;
        let dst = tenants.join(t.to_string());
        fs::rename(&tmp, &dst).map_err(|e| at(&dst, e))?;
    }
    sync_dir(&tenants)?;
    // Last, so a node never starts on keys whose tenant data is missing.
    let tmp = data.join(".control.db.restore.new");
    copy_synced(&from.join("control.db"), &tmp)?;
    let dst = data.join("control.db");
    fs::rename(&tmp, &dst).map_err(|e| at(&dst, e))?;
    sync_dir(data)?;
    Ok(ids.len())
}

/// Copies `src` to the new file `dst` (mode `0600`) and flushes it to disk.
fn copy_synced(src: &Path, dst: &Path) -> Result<(), String> {
    private_new_file(dst).map_err(|e| at(dst, e))?;
    fs::copy(src, dst).map_err(|e| at(src, e))?;
    // `copy` gives `dst` the mode of `src`.
    set_mode(dst, 0o600).map_err(|e| at(dst, e))?;
    fs::File::open(dst)
        .and_then(|f| f.sync_all())
        .map_err(|e| at(dst, e))
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| at(dir, e))
}

fn dir_builder() -> fs::DirBuilder {
    #[allow(unused_mut)]
    let mut b = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b
}

/// Creates the new directory `dir`, accessible by its owner only.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    dir_builder().create(dir)
}

/// Like [`private_dir`], with missing parents; an existing `dir` is kept as is.
pub(crate) fn private_dir_all(dir: &Path) -> io::Result<()> {
    dir_builder().recursive(true).create(dir)
}

/// Creates the new empty file `path`, readable and writable by its owner only.
fn private_new_file(path: &Path) -> io::Result<()> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path).map(drop)
}

/// Sets the Unix permission bits of `path` (no-op elsewhere).
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}
