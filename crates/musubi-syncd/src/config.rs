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
//!   "google": { "client_id": "...", "client_secret": "..." },
//!   "dav": { "allow_private": false }
//! }
//! ```
//!
//! `head -c 32 /dev/urandom | base64` makes a key (also a good `admin_key`,
//! which needs at least 32 characters). The file holds secrets: readable by the
//! service user only; `load` warns otherwise. `Debug` omits the secrets.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

/// The whole file.
#[derive(Clone, Deserialize)]
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
    /// DAV accounts; optional.
    #[serde(default)]
    pub dav: DavSettings,
}

/// DAV accounts (`POST /v1/accounts/dav`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DavSettings {
    /// Lets DAV accounts reach private addresses and `http` on loopback (a
    /// server on the LAN, tests with Radicale). Off: only public addresses
    /// over `https`.
    #[serde(default)]
    pub allow_private: bool,
}

/// Google OAuth client. Endpoints default to Google's production ones.
///
/// Deployment: enable Google Calendar API, Tasks API and People API in the
/// client's Cloud project. A disabled Tasks or People API leaves that kind
/// unauthorised, not an error.
#[derive(Clone, Deserialize)]
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
    /// Tasks API root.
    #[serde(default = "default_tasks_api_base")]
    pub tasks_api_base: String,
    /// People API root.
    #[serde(default = "default_people_api_base")]
    pub people_api_base: String,
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
fn default_tasks_api_base() -> String {
    "https://tasks.googleapis.com/tasks/v1".into()
}
fn default_people_api_base() -> String {
    "https://people.googleapis.com/v1".into()
}

const REDACTED: &str = "[redacted]";

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("data_dir", &self.data_dir)
            .field("public_url", &self.public_url)
            .field("admin_key", &REDACTED)
            .field("signing_key", &REDACTED)
            .field("token_key", &REDACTED)
            .field("google", &self.google)
            .field("dav", &self.dav)
            .finish()
    }
}

impl fmt::Debug for GoogleSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleSettings")
            .field("client_id", &self.client_id)
            .field("client_secret", &REDACTED)
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("api_base", &self.api_base)
            .field("tasks_api_base", &self.tasks_api_base)
            .field("people_api_base", &self.people_api_base)
            .finish()
    }
}

impl Config {
    /// Reads and parses `path`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        #[cfg(unix)]
        if let Ok(m) = std::fs::metadata(path) {
            use std::os::unix::fs::PermissionsExt as _;
            if m.permissions().mode() & 0o077 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    "the configuration holds secrets but is readable by group or others; chmod 600 it"
                );
            }
        }
        serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Rejects values that would weaken the node.
    pub(crate) fn check(&self) -> Result<(), String> {
        let k = &self.admin_key;
        if k.chars().count() < 32 {
            return Err("admin_key must have at least 32 characters".into());
        }
        // Such a key would be taken for a tenant key or a token.
        if k.starts_with("mk_") || k.starts_with("mt1.") {
            return Err("admin_key must not start with mk_ or mt1.".into());
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_admin_keys_and_secrets_in_debug_are_refused() {
        let c = |admin: &str| Config {
            listen: "127.0.0.1:1".parse().unwrap(),
            data_dir: "/d".into(),
            public_url: "http://x/".into(),
            admin_key: admin.into(),
            signing_key: "signing-secret".into(),
            token_key: "token-secret".into(),
            google: GoogleSettings {
                client_id: "id".into(),
                client_secret: "client-secret".into(),
                auth_url: default_auth_url(),
                token_url: default_token_url(),
                api_base: default_api_base(),
                tasks_api_base: default_tasks_api_base(),
                people_api_base: default_people_api_base(),
            },
            dav: DavSettings::default(),
        };
        let long = "a".repeat(32);
        assert!(c(&long).check().is_ok());
        assert!(c(&long[1..]).check().is_err(), "31 characters");
        assert!(c(&format!("mk_{long}")).check().is_err());
        assert!(c(&format!("mt1.{long}")).check().is_err());
        let debug = format!("{:?}", c(&long));
        for secret in [
            long.as_str(),
            "signing-secret",
            "token-secret",
            "client-secret",
        ] {
            assert!(!debug.contains(secret), "{debug}");
        }
    }
}
