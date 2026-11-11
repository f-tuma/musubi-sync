//! `musubi-syncd`: the phase 1 server (docs/plan/phase1.md, K6).
//!
//! HTTP API (`openapi.yaml` next to this crate's `Cargo.toml`), caller
//! authentication, the OAuth flow, the Google webhook, the commit bus behind
//! SSE, and the `backup` / `restore` operations. The binary in `main.rs` only
//! parses the command line.
//!
//! **Engine.** [`App::engine`] runs the sync engine over the same stores. Its
//! commits reach SSE through [`CommitBus::notify`]; the OAuth callback starts
//! a new account's discovery, the webhook enqueues the engine's push job, and
//! deleting a tenant removes its accounts through the engine first, so their
//! watch channels are stopped while their tokens still exist.
//!
//! **Callers.** Every `/v1` route except the OAuth callback and the webhook
//! needs `Authorization: Bearer <credential>`:
//!
//! - the instance admin key from the configuration: `/v1/admin/*` only;
//! - a tenant API key (`mk_…`, stored only as SHA-256): the tenant's routes up
//!   to its scope, and `POST /v1/tokens`;
//! - a short-lived token (`mt1.…`, [`crypto`]): like the key it was issued
//!   from, verified without a database lookup.
//!
//! Missing, unknown, revoked, expired or forged credentials get `401`; a
//! valid credential outside its scope gets `403`. A tenant credential only
//! ever opens its own tenant's store, and ids of another tenant's objects
//! answer `404`.
//!
//! Stores are synchronous: handlers call them through [`App::run`]
//! (`spawn_blocking`). Every route except `/v1/stream`, `/v1/admin/backup`
//! and `DELETE /v1/admin/tenants/{id}` runs under one shared concurrency limit and a request timeout, so a flood
//! of unknown credentials or webhook calls queues instead of filling the
//! blocking pool; request bodies are capped. Errors, including rejected
//! extractors and unknown routes, are `{"error", "message"}` JSON.

mod admin;
pub mod config;
pub mod crypto;
mod oauth;
mod push;
mod read;

// Linked so that the dependency graph matches docs/plan/phase1.md chapter 2.
use musubi_recurrence as _;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use jiff::Timestamp;
use musubi_engine::Engine;
use musubi_google::{GoogleCodec, GoogleConfig, GoogleProvider};
use musubi_model::control::{ApiKeyHash, Scope};
use musubi_model::provider::Clock;
use musubi_model::{ControlStore, Seq, StoreError, TenantId};
use musubi_store_sqlite::{SqliteControl, SqliteStorage};
use reqwest::Url;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;

pub use admin::{backup, restore};
use config::{Config, key32};
use crypto::{Claims, Sealer, Signer, Vault, sha256};

/// Tenants kept open by the storage (LRU).
const OPEN_TENANTS: usize = 64;
/// Default and longest lifetime of a short-lived token.
const TOKEN_TTL: u64 = 900;
const TOKEN_TTL_MAX: u64 = 900;
/// Requests in flight at once (excluding SSE and backup), and their timeout.
const CONCURRENCY: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest request body; the API takes only small JSON objects.
const BODY_LIMIT: usize = 64 * 1024;

/// The sync engine as the node runs it.
pub type SyncEngine = Engine<SqliteStorage, GoogleCodec>;

/// The running node: stores, adapter, engine, keys and the commit bus.
pub struct App {
    control: Arc<SqliteControl>,
    storage: Arc<SqliteStorage>,
    google: Arc<GoogleProvider>,
    engine: SyncEngine,
    oauth: oauth::Client,
    http: reqwest::Client,
    signer: Signer,
    sealer: Arc<Sealer>,
    admin_hash: [u8; 32],
    bus: Arc<CommitBus>,
    /// Tenants being deleted: an OAuth callback does not add accounts to them.
    deleting: Mutex<HashSet<TenantId>>,
    data_dir: PathBuf,
    _lock: File,
}

impl App {
    /// Opens the data directory (taking its lock: one node per directory) and
    /// builds everything the routes need. The engine's scheduler is not
    /// started: call [`SyncEngine::start`] and keep its handle.
    pub fn open(c: &Config) -> Result<Arc<Self>, String> {
        c.check()?;
        let data = |e: std::io::Error| format!("{}: {e}", c.data_dir.display());
        admin::private_dir_all(&c.data_dir).map_err(data)?;
        // An existing directory keeps its mode otherwise; it holds every secret.
        admin::set_mode(&c.data_dir, 0o700).map_err(data)?;
        let lock = lock_data_dir(&c.data_dir)?;
        let control = Arc::new(SqliteControl::open(&c.data_dir.join("control.db")).map_err(err)?);
        let storage = Arc::new(SqliteStorage::new(&c.data_dir, OPEN_TENANTS).map_err(err)?);
        let url = |name: &str, v: &str| Url::parse(v).map_err(|e| format!("{name}: {e}"));
        let g = &c.google;
        let clock = Arc::new(SystemClock);
        let google = Arc::new(
            GoogleProvider::new(
                GoogleConfig {
                    client_id: g.client_id.clone(),
                    client_secret: SecretString::from(g.client_secret.clone()),
                    api_base: url("google.api_base", &g.api_base)?,
                    token_url: url("google.token_url", &g.token_url)?,
                },
                clock.clone(),
            )
            .map_err(err)?,
        );
        // GoogleProvider::new installed the ring provider reqwest needs.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(err)?;
        let public = base_url(url("public_url", &c.public_url)?);
        let sealer = Arc::new(Sealer::new(&key32("token_key", &c.token_key)?));
        let bus = Arc::new(CommitBus::default());
        let sink = bus.clone();
        let callback = public.join("v1/webhooks/google").map_err(err)?;
        let engine = Engine::new(
            storage.clone(),
            google.clone(),
            control.clone(),
            Arc::new(Vault {
                control: control.clone(),
                sealer: sealer.clone(),
            }),
            clock,
            Arc::new(move |tenant, head| sink.notify(tenant, head)),
            musubi_engine::Config {
                callback: Some(callback.as_str().parse().map_err(err)?),
                ..Default::default()
            },
        );
        Ok(Arc::new(Self {
            control,
            storage,
            google,
            engine,
            oauth: oauth::Client {
                client_id: g.client_id.clone(),
                client_secret: SecretString::from(g.client_secret.clone()),
                auth_url: url("google.auth_url", &g.auth_url)?,
                token_url: url("google.token_url", &g.token_url)?,
                redirect_uri: public
                    .join("v1/oauth/google/callback")
                    .map_err(err)?
                    .to_string(),
            },
            http,
            signer: Signer::new(&key32("signing_key", &c.signing_key)?)?,
            sealer,
            admin_hash: sha256(c.admin_key.as_bytes()),
            bus,
            deleting: Mutex::default(),
            data_dir: c.data_dir.clone(),
            _lock: lock,
        }))
    }

    /// The HTTP API.
    pub fn router(self: Arc<Self>) -> Router {
        let bounded = Router::new()
            .route("/v1/accounts", post(oauth::start))
            .route("/v1/accounts/{id}", get(read::account))
            .route("/v1/collections", get(read::collections))
            .route("/v1/entities", get(read::entities))
            .route("/v1/entities/{id}", get(read::entity))
            .route("/v1/items/{id}", get(read::item))
            .route("/v1/changes", get(read::changes))
            .route("/v1/tokens", post(tokens))
            .route("/v1/oauth/google/callback", get(oauth::callback))
            .route("/v1/webhooks/google", post(push::google))
            .route("/v1/admin/tenants", post(admin::create_tenant))
            .route("/v1/admin/tenants/{id}/keys", post(admin::create_key))
            .route("/v1/admin/tenants/{id}/stats", get(admin::stats))
            .route("/v1/admin/keys/{id}", delete(admin::revoke_key))
            .layer(GlobalConcurrencyLimitLayer::new(CONCURRENCY))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::SERVICE_UNAVAILABLE,
                REQUEST_TIMEOUT,
            ));
        // Long-lived: the stream has its own per-tenant cap (`read::stream`),
        // a backup and a tenant deletion take as long as the data needs.
        Router::new()
            .route("/v1/stream", get(read::stream))
            .route("/v1/admin/backup", post(admin::backup_route))
            .route("/v1/admin/tenants/{id}", delete(admin::delete_tenant))
            .merge(bounded)
            .fallback(|| async { ApiError::not_found() })
            .layer(axum::middleware::map_response(json_errors))
            .layer(DefaultBodyLimit::max(BODY_LIMIT))
            .with_state(self)
    }

    /// The control store (shared with the engine).
    pub fn control(&self) -> &Arc<SqliteControl> {
        &self.control
    }

    /// Tenant storage (shared with the engine).
    pub fn storage(&self) -> &SqliteStorage {
        &self.storage
    }

    /// The Google adapter (shared with the engine).
    pub fn google(&self) -> &GoogleProvider {
        &self.google
    }

    /// The sync engine over this node's stores.
    pub fn engine(&self) -> &SyncEngine {
        &self.engine
    }

    /// Commit notifications for SSE.
    pub fn bus(&self) -> &CommitBus {
        &self.bus
    }

    /// The engine's view of account tokens.
    pub fn vault(&self) -> Vault {
        Vault {
            control: self.control.clone(),
            sealer: self.sealer.clone(),
        }
    }

    /// Signs a short-lived token (what `POST /v1/tokens` returns).
    pub fn issue_token(&self, c: &Claims) -> String {
        self.signer.issue(c)
    }

    /// Tenants being deleted; held while an OAuth callback adds an account.
    pub(crate) fn deleting(&self) -> std::sync::MutexGuard<'_, HashSet<TenantId>> {
        self.deleting.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs blocking store work off the async workers.
    pub(crate) async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&App) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let app = self.clone();
        tokio::task::spawn_blocking(move || f(&app))
            .await
            .map_err(|e| ApiError::internal(&e))?
    }
}

/// `public_url` as a base to join relative paths to: `Url::join` replaces
/// the last path segment of a base without a trailing `/`.
fn base_url(mut u: Url) -> Url {
    if !u.path().ends_with('/') {
        u.set_path(&format!("{}/", u.path()));
    }
    u
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Takes `<data>/musubi.lock` exclusively; held for the life of the returned file.
pub(crate) fn lock_data_dir(dir: &Path) -> Result<File, String> {
    let path = dir.join("musubi.lock");
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.try_lock().map_err(|_| {
        format!(
            "{} is in use by a running node (lock {} held)",
            dir.display(),
            path.display()
        )
    })?;
    Ok(f)
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// In-memory notification of committed journal positions: the seam between
/// the tenant writer (engine) and SSE. One `watch` per tenant holds the latest
/// head: a slow subscriber skips to it, and a commit wakes only that tenant's
/// streams. Its receivers are the open streams, so their count is the cap.
#[derive(Default)]
pub struct CommitBus(Mutex<HashMap<TenantId, watch::Sender<Seq>>>);

impl CommitBus {
    /// Announces that `tenant`'s journal head is now `head`. Call after the
    /// commit returned.
    pub fn notify(&self, tenant: TenantId, head: Seq) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let tx = map.entry(tenant).or_insert_with(|| watch::channel(head).0);
        tx.send_if_modified(|h| (head > *h).then(|| *h = head).is_some());
    }

    /// Drops a deleted tenant's channel; its open streams end.
    pub(crate) fn remove(&self, tenant: TenantId) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&tenant);
    }

    /// A receiver of `tenant`'s heads, unless it already has `max` of them.
    pub(crate) fn subscribe(&self, tenant: TenantId, max: usize) -> Option<watch::Receiver<Seq>> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let tx = map
            .entry(tenant)
            .or_insert_with(|| watch::channel(Seq(0)).0);
        (tx.receiver_count() < max).then(|| tx.subscribe())
    }
}

/// Gives every error response without a JSON body (rejected extractors,
/// unknown routes, body limit, timeout) the `{"error", "message"}` shape.
async fn json_errors(r: Response) -> Response {
    let s = r.status();
    let is_json = r
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|c| c.as_bytes().starts_with(b"application/json"));
    if !(s.is_client_error() || s.is_server_error()) || is_json {
        return r;
    }
    let (parts, body) = r.into_parts();
    let text = axum::body::to_bytes(body, BODY_LIMIT)
        .await
        .unwrap_or_default();
    let reason = s.canonical_reason().unwrap_or("error");
    let message = match String::from_utf8_lossy(&text).trim() {
        "" => reason.to_lowercase(),
        m => m.to_owned(),
    };
    let code = reason.to_lowercase().replace([' ', '-'], "_");
    let mut out = (s, Json(json!({ "error": code, "message": message }))).into_response();
    for (k, v) in &parts.headers {
        if k != header::CONTENT_TYPE && k != header::CONTENT_LENGTH {
            out.headers_mut().insert(k, v.clone());
        }
    }
    out
}

/// An API error: status and `{"error": code, "message": …}`.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    oldest: Option<Seq>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            oldest: None,
        }
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid credential",
        )
    }

    fn forbidden(why: &str) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", why)
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "not found")
    }

    fn too_many(why: &str) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "too_many_requests", why)
    }

    fn bad_request(why: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", why)
    }

    fn internal(e: &dyn std::fmt::Display) -> Self {
        tracing::error!(error = %e, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => Self::not_found(),
            StoreError::AlreadyExists => {
                Self::new(StatusCode::CONFLICT, "conflict", "already exists")
            }
            StoreError::CursorTooOld { oldest } => Self {
                oldest: Some(oldest),
                ..Self::new(
                    StatusCode::GONE,
                    "cursor_too_old",
                    "new initial sync required: the journal no longer reaches this cursor",
                )
            },
            StoreError::Busy => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "busy", "store busy, retry")
            }
            e => Self::internal(&e),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.code, "message": self.message });
        if let Some(o) = self.oldest {
            body["oldest"] = json!(o);
        }
        let mut r = (self.status, Json(body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            r.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        r
    }
}

/// The authenticated caller.
#[derive(Clone, Copy)]
pub(crate) enum Caller {
    /// The instance admin key.
    Admin,
    /// A tenant API key.
    Key {
        tenant: TenantId,
        scope: Scope,
        hash: ApiKeyHash,
    },
    /// A short-lived token, valid until `exp` (Unix seconds).
    Token {
        tenant: TenantId,
        scope: Scope,
        exp: i64,
    },
}

impl Caller {
    /// The caller's tenant, if its scope covers `need`.
    fn tenant(self, need: Scope) -> Result<TenantId, ApiError> {
        match self {
            Caller::Admin => Err(ApiError::forbidden("the admin key has no tenant")),
            Caller::Key { tenant, scope, .. } | Caller::Token { tenant, scope, .. } => {
                if scope == Scope::Admin || need == Scope::Read {
                    Ok(tenant)
                } else {
                    Err(ApiError::forbidden("needs admin scope"))
                }
            }
        }
    }

    fn admin(self) -> Result<(), ApiError> {
        match self {
            Caller::Admin => Ok(()),
            _ => Err(ApiError::forbidden("needs the instance admin key")),
        }
    }
}

impl FromRequestParts<Arc<App>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, ApiError> {
        let cred = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, cred)| cred.trim())
            .ok_or_else(ApiError::unauthorized)?;
        if cred.starts_with("mt1.") {
            let c = app
                .signer
                .verify(cred, Timestamp::now())
                .ok_or_else(ApiError::unauthorized)?;
            return Ok(Caller::Token {
                tenant: c.tenant,
                scope: c.scope,
                exp: c.exp,
            });
        }
        let hash = sha256(cred.as_bytes());
        if crypto::same(&hash, &app.admin_hash) {
            return Ok(Caller::Admin);
        }
        match app
            .run(move |a| Ok(a.control.find_api_key(&ApiKeyHash(hash))?))
            .await?
        {
            Some(k) if k.revoked.is_none() => Ok(Caller::Key {
                tenant: k.tenant,
                scope: k.scope,
                hash: k.hash,
            }),
            _ => Err(ApiError::unauthorized()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRequest {
    scope: Option<Scope>,
    ttl: Option<u64>,
}

/// `POST /v1/tokens`: a short-lived token for the presenting API key.
async fn tokens(
    State(app): State<Arc<App>>,
    caller: Caller,
    Json(req): Json<TokenRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let Caller::Key { tenant, scope, .. } = caller else {
        return Err(ApiError::forbidden(
            "tokens are issued for tenant API keys only",
        ));
    };
    let want = req.scope.unwrap_or(scope);
    if want == Scope::Admin && scope == Scope::Read {
        return Err(ApiError::forbidden(
            "a read key cannot issue an admin token",
        ));
    }
    let ttl = req.ttl.unwrap_or(TOKEN_TTL);
    if !(1..=TOKEN_TTL_MAX).contains(&ttl) {
        return Err(ApiError::bad_request(format!(
            "ttl must be 1..={TOKEN_TTL_MAX} seconds"
        )));
    }
    let iat = Timestamp::now().as_second();
    let claims = Claims {
        tenant,
        scope: want,
        iat,
        exp: iat + ttl as i64,
    };
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "token": app.issue_token(&claims),
            "scope": want,
            "expires": Timestamp::from_second(claims.exp).map_err(|e| ApiError::internal(&e))?,
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_url_keeps_its_path() {
        let join = |public: &str| {
            base_url(Url::parse(public).unwrap())
                .join("v1/webhooks/google")
                .unwrap()
                .to_string()
        };
        assert_eq!(join("https://h/sync"), "https://h/sync/v1/webhooks/google");
        assert_eq!(join("https://h/sync/"), "https://h/sync/v1/webhooks/google");
        assert_eq!(join("https://h"), "https://h/v1/webhooks/google");
    }
}
