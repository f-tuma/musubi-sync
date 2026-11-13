//! What the adapter keeps between calls: the account's [`Endpoint`] state
//! and the collection cursors. Both are JSON with a version, opaque to the
//! engine.

use std::collections::BTreeMap;

use musubi_model::capability::DeltaSupport;
use musubi_model::{Cursor, Endpoint, EntityKind, SyncError};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::ConnectError;
use crate::net::origin;
use crate::xml::{pct_decode, pct_encode};

/// How an account is connected: the input of chapter 5.5 without the
/// credentials, which travel in [`musubi_model::Credential`].
#[derive(Debug, Clone, Default)]
pub struct DavSetup {
    /// CalDAV and CardDAV entry.
    pub server_url: Option<String>,
    /// CalDAV entry when the server has CalDAV and CardDAV on separate hosts.
    pub caldav_url: Option<String>,
    /// CardDAV entry, see `caldav_url`.
    pub carddav_url: Option<String>,
    /// More origins the user confirmed the password may go to (after a
    /// `cross_origin_redirect`).
    pub allow_origins: Vec<String>,
}

impl DavSetup {
    /// The endpoint a first [`crate::DavProvider::connect`] starts from:
    /// the entries and the allowed origins, fixed from now on. A new connect
    /// also drops every delta descent (plan, chapter 5.3).
    pub fn endpoint(&self) -> Result<Endpoint, ConnectError> {
        let parse = |u: &Option<String>| -> Result<Option<Url>, ConnectError> {
            u.as_deref()
                .map(|u| {
                    Url::parse(u.trim())
                        .ok()
                        .filter(|u| {
                            matches!(u.scheme(), "https" | "http") && u.has_host() && no_userinfo(u)
                        })
                        .ok_or_else(|| {
                            ConnectError::InvalidConfig(
                                "not an http(s) URL without userinfo".into(),
                            )
                        })
                })
                .transpose()
        };
        let server = parse(&self.server_url)?;
        let caldav = parse(&self.caldav_url)?.or_else(|| server.clone());
        let carddav = parse(&self.carddav_url)?.or_else(|| server.clone());
        let base = caldav.as_ref().or(carddav.as_ref()).ok_or_else(|| {
            ConnectError::InvalidConfig("server_url, caldav_url or carddav_url is required".into())
        })?;
        let mut origins: Vec<String> = caldav.iter().chain(&carddav).map(origin).collect();
        for o in &self.allow_origins {
            let u = Url::parse(o.trim())
                .ok()
                .filter(|u| {
                    matches!(u.scheme(), "https" | "http") && u.has_host() && no_userinfo(u)
                })
                .ok_or_else(|| {
                    ConnectError::InvalidConfig("not an http(s) origin without userinfo".into())
                })?;
            origins.push(origin(&u));
        }
        origins.sort();
        origins.dedup();
        let state = State {
            v: 1,
            profile: "generic".into(),
            caldav_url: caldav.as_ref().map(Url::to_string),
            carddav_url: carddav.as_ref().map(Url::to_string),
            origins,
            ..State::default()
        };
        Ok(Endpoint::new(base.as_str(), state.to_json()))
    }
}

/// Credentials travel only in [`musubi_model::Credential`]: a URL with
/// userinfo would put a password into the stored endpoint and the logs.
fn no_userinfo(u: &Url) -> bool {
    u.username().is_empty() && u.password().is_none()
}

/// Normalised principal URLs of a connected account (plan, chapter 5.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principals {
    /// CalDAV principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caldav: Option<String>,
    /// CardDAV principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carddav: Option<String>,
}

impl Principals {
    /// The account subject: the CalDAV principal, else the CardDAV one.
    pub fn subject(&self) -> Option<&str> {
        self.caldav.as_deref().or(self.carddav.as_deref())
    }

    /// Of an endpoint [`crate::DavProvider::connect`] returned.
    pub fn of(endpoint: &Endpoint) -> Principals {
        State::from_endpoint(endpoint)
            .map(|s| s.principals)
            .unwrap_or_default()
    }

    /// A reconnect must find every principal the stored account has, unchanged
    /// (plan, chapter 5.5).
    pub fn check_reconnect(stored: &Endpoint, found: &Endpoint) -> Result<(), ConnectError> {
        let (old, new) = (Principals::of(stored), Principals::of(found));
        let keeps = |o: &Option<String>, n: &Option<String>| o.is_none() || o == n;
        if old.subject().is_some()
            && keeps(&old.caldav, &new.caldav)
            && keeps(&old.carddav, &new.carddav)
        {
            Ok(())
        } else {
            Err(ConnectError::SubjectMismatch)
        }
    }
}

/// Normalised URL: origin plus the canonically percent-encoded path.
pub(crate) fn normalise(url: &Url) -> String {
    format!("{}{}", origin(url), pct_encode(&pct_decode(url.path())))
}

/// Delta level of a collection; the names are the plan's (`delta_override`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Level {
    SyncToken,
    CtagEtag,
    None,
}

impl From<Level> for DeltaSupport {
    fn from(l: Level) -> Self {
        match l {
            Level::SyncToken => DeltaSupport::SyncToken,
            Level::CtagEtag => DeltaSupport::CtagEtag,
            Level::None => DeltaSupport::None,
        }
    }
}

/// A discovered collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Col {
    pub origin: String,
    pub kind: EntityKind,
    pub delta: Level,
}

impl Col {
    /// The collection's URL from its key (a decoded absolute path).
    pub fn url(&self, key: &str) -> Result<Url, SyncError> {
        Url::parse(&format!("{}{}", self.origin, pct_encode(key)))
            .map_err(|e| SyncError::Permanent(format!("bad collection URL: {e}").into()))
    }
}

/// `Endpoint.state` of a DAV account.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    pub v: u32,
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caldav_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carddav_url: Option<String>,
    /// Where the password may go, normalised ([`origin`]).
    pub origins: Vec<String>,
    #[serde(default)]
    pub principals: Principals,
    /// Home set URLs (iCloud's partition hosts are here).
    #[serde(default)]
    pub homes: Vec<String>,
    /// Collections by key.
    #[serde(default)]
    pub collections: BTreeMap<String, Col>,
    /// Delta descents (plan, chapter 5.3); only a new connect drops them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub delta_override: BTreeMap<String, Level>,
}

impl State {
    pub fn from_endpoint(e: &Endpoint) -> Result<State, String> {
        let s: State = serde_json::from_value(e.state.clone()).map_err(|e| e.to_string())?;
        if s.v != 1 || s.profile != "generic" {
            return Err(format!("unsupported endpoint state v{} {}", s.v, s.profile));
        }
        Ok(s)
    }

    pub fn of(cx: &musubi_model::AccountCx) -> Result<State, SyncError> {
        let e = cx
            .endpoint
            .as_ref()
            .ok_or_else(|| SyncError::Permanent("DAV account without endpoint".into()))?;
        State::from_endpoint(e).map_err(|e| SyncError::Permanent(e.into()))
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("state serialises")
    }

    /// The collection behind a key; unknown means discovery is stale.
    pub fn col(&self, key: &str) -> Result<&Col, SyncError> {
        self.collections.get(key).ok_or(SyncError::Rediscover)
    }
}

/// Collection cursor (plan, chapter 3.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CursorState {
    SyncToken {
        token: String,
    },
    Ctag {
        ctag: String,
    },
    /// A full read in progress; it starts over when resumed.
    Listing {
        since: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
struct Versioned {
    v: u32,
    #[serde(flatten)]
    c: CursorState,
}

impl CursorState {
    pub fn to_cursor(&self) -> Cursor {
        Cursor(
            serde_json::to_vec(&Versioned {
                v: 1,
                c: self.clone(),
            })
            .expect("cursor serialises"),
        )
    }

    /// `None` for a cursor this version cannot read: a full read then.
    pub fn from_cursor(c: &Cursor) -> Option<CursorState> {
        serde_json::from_slice::<Versioned>(&c.0)
            .ok()
            .filter(|v| v.v == 1)
            .map(|v| v.c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips() {
        for c in [
            CursorState::SyncToken { token: "t".into() },
            CursorState::Ctag { ctag: "c".into() },
            CursorState::Listing { since: None },
        ] {
            assert_eq!(CursorState::from_cursor(&c.to_cursor()), Some(c));
        }
        assert_eq!(CursorState::from_cursor(&Cursor(b"x".to_vec())), None);
    }

    #[test]
    fn setup_needs_an_entry_and_normalises_origins() {
        assert!(DavSetup::default().endpoint().is_err());
        for bad in [
            "https://alice:pw@cal.example/",
            "ftp://cal.example/",
            "cal.example",
        ] {
            let s = DavSetup {
                server_url: Some(bad.into()),
                ..DavSetup::default()
            };
            assert!(
                matches!(s.endpoint(), Err(ConnectError::InvalidConfig(_))),
                "{bad}"
            );
        }
        let e = DavSetup {
            caldav_url: Some("https://Cal.Example:443/dav/".into()),
            carddav_url: Some("https://card.example/".into()),
            allow_origins: vec!["https://p1.example:8443".into()],
            ..DavSetup::default()
        }
        .endpoint()
        .unwrap();
        let s = State::from_endpoint(&e).unwrap();
        assert_eq!(
            s.origins,
            [
                "https://cal.example",
                "https://card.example",
                "https://p1.example:8443"
            ]
        );
        assert_eq!(e.base, "https://cal.example/dav/");
    }

    #[test]
    fn reconnect_needs_the_same_principals() {
        let with = |cal: Option<&str>, card: Option<&str>| {
            let s = State {
                v: 1,
                profile: "generic".into(),
                principals: Principals {
                    caldav: cal.map(str::to_owned),
                    carddav: card.map(str::to_owned),
                },
                ..State::default()
            };
            Endpoint::new("x", s.to_json())
        };
        let stored = with(Some("https://a/p/"), Some("https://b/p/"));
        assert!(Principals::check_reconnect(&stored, &stored).is_ok());
        assert!(Principals::check_reconnect(&stored, &with(Some("https://a/p/"), None)).is_err());
        assert!(
            Principals::check_reconnect(&stored, &with(Some("https://a/q/"), Some("https://b/p/")))
                .is_err()
        );
    }
}
