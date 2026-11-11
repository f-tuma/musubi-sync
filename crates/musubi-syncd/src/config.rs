//! The configuration file (JSON), read by every command.
//!
//! ```json
//! {
//!   "listen": "127.0.0.1:8080",
//!   "data_dir": "/var/lib/musubi",
//!   "public_url": "https://sync.example.com",
//!   "admin_key": "<long random string>",
//!   "signing_key": "<base64 of 32 random bytes>",
//!   "token_key": "<base64 of 32 random bytes>",
//!   "google": { "client_id": "...", "client_secret": "..." }
//! }
//! ```
//!
//! `head -c 32 /dev/urandom | base64` makes a key. The file holds secrets:
//! readable by the service user only.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

/// The whole file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the HTTP server binds.
    pub listen: SocketAddr,
    /// Data directory: `control.db`, `tenants/`, `musubi.lock`.
    pub data_dir: PathBuf,
    /// External base URL of this node; the OAuth redirect is
    /// `<public_url>/v1/oauth/google/callback`.
    pub public_url: String,
    /// Instance administrator credential (admin routes only).
    pub admin_key: String,
    /// Ed25519 seed for short-lived tokens, standard base64 of 32 bytes.
    pub signing_key: String,
    /// AES-256-GCM key sealing account tokens, standard base64 of 32 bytes.
    pub token_key: String,
    /// Google OAuth client and endpoints.
    pub google: GoogleSettings,
}

/// Google OAuth client. Endpoints default to Google's production ones.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleSettings {
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: String,
    /// Consent page.
    #[serde(default = "default_auth_url")]
    pub auth_url: String,
    /// Token endpoint.
    #[serde(default = "default_token_url")]
    pub token_url: String,
    /// Calendar API root.
    #[serde(default = "default_api_base")]
    pub api_base: String,
}

fn default_auth_url() -> String {
    "https://accounts.google.com/o/oauth2/v2/auth".into()
}
fn default_token_url() -> String {
    "https://oauth2.googleapis.com/token".into()
}
fn default_api_base() -> String {
    "https://www.googleapis.com/calendar/v3".into()
}

impl Config {
    /// Reads and parses `path`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Decodes a 32-byte key given as standard base64.
pub(crate) fn key32(name: &str, v: &str) -> Result<[u8; 32], String> {
    STANDARD
        .decode(v.trim())
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| format!("{name} must be standard base64 of exactly 32 bytes"))
}
