//! Connecting a DAV account: `POST /v1/accounts/dav` (docs/plan/phase2.md 5.5).
//!
//! Discovery runs inside the request (20 s). Only when it succeeds are the
//! credentials sealed and the endpoint stored; a failure answers `400` with
//! the reason and stores nothing. A reconnect (with `account`, or a new
//! connect that finds the same subject) must find every principal the stored
//! account has, else `400 subject_mismatch` and the account stays as it was.
//! The three writes are separate commits: an account left without its
//! endpoint by a failure between them is finished by the next connect to the
//! same principal.
//! The password is never logged or returned.
//!
//! Profile `icloud` takes no addresses from the request: it connects only to
//! iCloud's fixed entries (`DavSetup::icloud()`, plan 5.4) through the
//! `icloud` instance.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use musubi_dav::{ConnectError, DavProvider, DavSetup, Principals, Profile};
use musubi_model::Provider;
use musubi_model::control::{AccountRecord, AccountState, Scope};
use musubi_model::provider::{Credential, Limiter};
use musubi_model::{AccountCx, AccountId, ControlStore, Storage, StoreError};
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::Value;

use crate::config::DavSettings;
use crate::crypto::random;
use crate::{ApiError, App, Caller, SystemClock};

/// Plan 5.5: discovery on connect gives up after this long (`400 timeout`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConnectRequest {
    profile: String,
    server_url: Option<String>,
    caldav_url: Option<String>,
    carddav_url: Option<String>,
    #[serde(default)]
    allow_origins: Vec<String>,
    username: String,
    password: String,
    /// Reconnect this account (after `reauth_required`).
    account: Option<AccountId>,
}

/// Connect-time discovery is one request; the engine's limiter is per lane.
struct NoLimit;

#[async_trait]
impl Limiter for NoLimit {
    async fn acquire(&self) {}
}

fn reason(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, code, message)
}

fn connect_error(profile: Profile, e: ConnectError) -> ApiError {
    let message = e.to_string();
    match e {
        ConnectError::Unauthorized => ApiError {
            hint: profile.reauth_hint(),
            ..reason("unauthorized", "the server rejected the credentials")
        },
        ConnectError::NotDav(_) => reason("not_dav", message),
        ConnectError::BlockedAddress(_) => reason("blocked_address", message),
        // The message names the target, for the user to confirm through `allow_origins`.
        ConnectError::CrossOriginRedirect(_) => reason("cross_origin_redirect", message),
        ConnectError::SubjectMismatch => reason("subject_mismatch", message),
        ConnectError::InvalidConfig(_) => ApiError::bad_request(message),
        _ => ApiError::new(StatusCode::BAD_GATEWAY, "provider_error", message),
    }
}

/// `POST /v1/accounts/dav`.
pub(crate) async fn connect(
    State(app): State<Arc<App>>,
    caller: Caller,
    Json(req): Json<ConnectRequest>,
) -> Result<Json<Value>, ApiError> {
    let tenant = caller.tenant(Scope::Admin)?;
    let password = SecretString::from(req.password);
    let user_setup = DavSetup {
        server_url: req.server_url,
        caldav_url: req.caldav_url,
        carddav_url: req.carddav_url,
        allow_origins: req.allow_origins,
    };
    let (profile, dav, setup): (_, &DavProvider, _) = match req.profile.as_str() {
        "generic" => (Profile::Generic, &app.dav, user_setup),
        "icloud" => {
            let s = &user_setup;
            if s.server_url.is_some()
                || s.caldav_url.is_some()
                || s.carddav_url.is_some()
                || !s.allow_origins.is_empty()
            {
                return Err(ApiError::bad_request(
                    "profile icloud takes no server_url, caldav_url, carddav_url or allow_origins",
                ));
            }
            (Profile::Icloud, &app.icloud, app.icloud_entry.clone())
        }
        _ => return Err(ApiError::bad_request("profile must be generic or icloud")),
    };
    let fail = move |e| connect_error(profile, e);
    let provider = dav.descriptor().id;
    // RFC 7617: the user-id of Basic cannot contain a colon.
    if req.username.is_empty() || req.username.contains(':') {
        return Err(ApiError::bad_request(
            "username must be non-empty and without ':'",
        ));
    }
    let start = setup.endpoint_for(profile).map_err(fail)?;
    if let Some(id) = req.account {
        let p = provider.clone();
        let same = app
            .run(move |a| Ok(a.control.account(id)?))
            .await?
            .is_some_and(|x| x.tenant == tenant && x.provider == p);
        if !same {
            return Err(ApiError::not_found());
        }
    }

    let id = req
        .account
        .unwrap_or_else(|| AccountId::from_bytes(random()));
    let mut cx = AccountCx::new(
        id,
        Credential::Basic {
            username: req.username.clone(),
            password: password.clone(),
        },
        Arc::new(NoLimit),
        Arc::new(SystemClock),
    );
    cx.endpoint = Some(start);
    let found = tokio::time::timeout(CONNECT_TIMEOUT, dav.connect(&cx))
        .await
        .map_err(|_| reason("timeout", "discovery did not finish in 20 s"))?
        .map_err(fail)?
        .endpoint
        .ok_or_else(|| reason("not_dav", "discovery found no principal"))?;
    let subject = Principals::of(&found)
        .subject()
        .ok_or_else(|| reason("not_dav", "discovery found no principal"))?
        .to_owned();

    let username = req.username;
    let account = req.account;
    let rec = app
        .run(move |a| {
            // Held until the account exists, so a deletion lists it. It also
            // serialises connects: two to one principal cannot both create it.
            let deleting = a.deleting();
            if deleting.contains(&tenant) {
                return Err(StoreError::NotFound.into());
            }
            a.storage.open(tenant)?;
            let existing = match account {
                Some(id) => a.control.account(id)?,
                None => a.control.account_by_subject(tenant, &provider, &subject)?,
            }
            .filter(|x| x.tenant == tenant && x.provider == provider);
            let rec = match existing {
                Some(mut acc) => {
                    match a.control.endpoint(acc.id)? {
                        Some(stored) => {
                            Principals::check_reconnect(&stored, &found).map_err(fail)?
                        }
                        // A connect cut short before its endpoint was stored:
                        // the same principal finishes it.
                        None if acc.subject == subject => {}
                        None => {
                            return Err(reason(
                                "subject_mismatch",
                                "the account has no stored principal",
                            ));
                        }
                    }
                    if acc.state == AccountState::ReauthRequired {
                        acc.state = AccountState::Active;
                    }
                    acc
                }
                None if account.is_some() => return Err(ApiError::not_found()),
                None => AccountRecord {
                    id,
                    tenant,
                    provider,
                    subject,
                    state: AccountState::Connecting,
                },
            };
            // A running job of the account stores no account field over these.
            let mut writes = a.engine.account_writes();
            writes.reconnected(rec.id);
            a.control.upsert_account(&rec)?;
            a.control.write_tokens(
                rec.id,
                &a.sealer
                    .seal_basic(rec.tenant, rec.id, &username, &password),
            )?;
            a.control.set_endpoint(rec.id, &found)?;
            Ok(rec)
        })
        .await?;
    // Discovers a new account (`connecting` → `active`) or rediscovers a reconnected one.
    app.engine
        .connect(rec.id)
        .await
        .map_err(|e| ApiError::internal(&e))?;
    let view = app.run(move |a| crate::read::account_view(a, &rec)).await?;
    Ok(Json(view))
}

/// The entries profile `icloud` connects to: Apple's, unless the test-only
/// `dav.icloud_test_entries` points them at a fake server.
pub(crate) fn icloud_entry(c: &DavSettings) -> Result<DavSetup, String> {
    let Some(t) = &c.icloud_test_entries else {
        return Ok(DavSetup::icloud());
    };
    tracing::warn!(
        caldav = %t.caldav, carddav = %t.carddav,
        "dav.icloud_test_entries is set: profile icloud connects to a test server, not iCloud"
    );
    let setup = DavSetup {
        caldav_url: Some(t.caldav.clone()),
        carddav_url: Some(t.carddav.clone()),
        allow_origins: t.allow_origins.clone(),
        ..DavSetup::default()
    };
    setup
        .endpoint_for(Profile::Icloud)
        .map_err(|e| format!("dav.icloud_test_entries: {e}"))?;
    Ok(setup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IcloudTestEntries;

    #[test]
    fn icloud_uses_apple_entries_unless_test_entries_are_set() {
        let apple = DavSetup::icloud().endpoint_for(Profile::Icloud).unwrap();
        let entry = |c: &DavSettings| {
            icloud_entry(c)
                .unwrap()
                .endpoint_for(Profile::Icloud)
                .unwrap()
        };
        let unset = entry(&DavSettings::default());
        assert_eq!(unset, apple);
        assert_eq!(
            unset.state["origins"],
            serde_json::json!(["https://caldav.icloud.com", "https://contacts.icloud.com"])
        );

        let mut c = DavSettings {
            allow_private: true,
            icloud_test_entries: Some(IcloudTestEntries {
                caldav: "http://127.0.0.1:1/".into(),
                carddav: "http://127.0.0.1:2/".into(),
                allow_origins: vec!["http://127.0.0.1:3".into()],
            }),
        };
        let fake = entry(&c);
        assert_eq!(fake.state["profile"], "icloud");
        assert_eq!(
            fake.state["origins"],
            serde_json::json!([
                "http://127.0.0.1:1",
                "http://127.0.0.1:2",
                "http://127.0.0.1:3"
            ])
        );
        c.icloud_test_entries.as_mut().unwrap().caldav = "not a url".into();
        assert!(
            icloud_entry(&c)
                .unwrap_err()
                .contains("dav.icloud_test_entries")
        );
    }
}
