//! Connecting a Google account: authorization code flow with PKCE (RFC 7636).
//!
//! `POST /v1/accounts` stores a pending sign-in under the SHA-256 of a random
//! `state` (one use, 10 minutes, bound to the tenant) and returns the consent
//! URL. Google redirects the browser to the callback, which takes the pending
//! sign-in, redeems the code with the PKCE verifier, and stores the account and
//! its sealed tokens. The account subject comes from the `id_token` received
//! directly from the token endpoint over TLS, which OpenID Connect Core 3.1.3.7
//! accepts without checking its signature; `aud` is still checked.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use jiff::{SignedDuration, Timestamp};
use musubi_model::capability::ProviderId;
use musubi_model::control::{
    AccountRecord, AccountState, AccountTokens, OAuthPending, OAuthStateHash, Scope,
};
use musubi_model::{AccountId, ControlStore, Storage, StoreError};
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::crypto::{b64, random, random_token, sha256, unb64};
use crate::{ApiError, App, Caller};

const SCOPES: &str = "openid https://www.googleapis.com/auth/calendar.readonly";
const STATE_TTL: SignedDuration = SignedDuration::from_mins(10);

/// The OAuth client of this node.
pub(crate) struct Client {
    pub(crate) client_id: String,
    pub(crate) client_secret: SecretString,
    pub(crate) auth_url: Url,
    pub(crate) token_url: Url,
    pub(crate) redirect_uri: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartRequest {
    provider: String,
    /// Reconnect this account (after `reauth_required`) instead of adding one.
    account: Option<AccountId>,
}

/// `POST /v1/accounts`: starts connecting an account; returns the consent URL.
pub(crate) async fn start(
    State(app): State<Arc<App>>,
    caller: Caller,
    Json(req): Json<StartRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let tenant = caller.tenant(Scope::Admin)?;
    if req.provider != "google" {
        return Err(ApiError::bad_request("provider must be google"));
    }
    let state = random_token();
    let verifier = random_token();
    let challenge = b64(&sha256(verifier.as_bytes()));
    let expires = Timestamp::now() + STATE_TTL;
    let pending = OAuthPending {
        state: OAuthStateHash(sha256(state.as_bytes())),
        tenant,
        provider: ProviderId("google".into()),
        account: req.account,
        verifier: SecretString::from(verifier),
        expires,
    };
    app.run(move |a| {
        // A token can outlive its tenant by its lifetime: check the tenant.
        a.storage.open(tenant)?;
        if let Some(id) = pending.account
            && a.control.account(id)?.is_none_or(|x| x.tenant != tenant)
        {
            return Err(ApiError::not_found());
        }
        Ok(a.control.put_oauth_state(&pending)?)
    })
    .await?;
    let c = &app.oauth;
    let mut url = c.auth_url.clone();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &c.client_id)
        .append_pair("redirect_uri", &c.redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok((
        StatusCode::CREATED,
        Json(json!({ "authorization_url": url.as_str(), "expires": expires })),
    ))
}

#[derive(Deserialize)]
pub(crate) struct CallbackParams {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct Grant {
    access_token: String,
    expires_in: i64,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    sub: String,
    aud: String,
}

fn bad_gateway(why: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, "provider_error", why)
}

/// `GET /v1/oauth/google/callback`: finishes connecting (no caller credential;
/// the one-time `state` is the authorization).
pub(crate) async fn callback(
    State(app): State<Arc<App>>,
    Query(p): Query<CallbackParams>,
) -> Result<Json<Value>, ApiError> {
    let state = p
        .state
        .ok_or_else(|| ApiError::bad_request("missing state"))?;
    let hash = OAuthStateHash(sha256(state.as_bytes()));
    let pending = app
        .run(move |a| Ok(a.control.take_oauth_state(&hash, Timestamp::now())?))
        .await?
        .ok_or_else(|| ApiError::bad_request("unknown, used or expired state"))?;
    // The provider's `error` is not echoed: it is attacker-controlled text.
    if p.error.is_some() {
        return Err(ApiError::bad_request(
            "authorization was declined or failed at the provider",
        ));
    }
    let code = p
        .code
        .ok_or_else(|| ApiError::bad_request("missing code"))?;

    let c = &app.oauth;
    let resp = app
        .http
        .post(c.token_url.clone())
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &c.redirect_uri),
            ("client_id", &c.client_id),
            ("client_secret", c.client_secret.expose_secret()),
            ("code_verifier", pending.verifier.expose_secret()),
        ])
        .send()
        .await
        .map_err(|e| bad_gateway(format!("token endpoint: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::BAD_REQUEST {
        return Err(ApiError::bad_request(
            "the provider rejected the authorization code",
        ));
    }
    if !status.is_success() {
        return Err(bad_gateway(format!("token endpoint answered {status}")));
    }
    let g: Grant = resp
        .json()
        .await
        .map_err(|e| bad_gateway(format!("token response: {e}")))?;
    let refresh = g
        .refresh_token
        .ok_or_else(|| bad_gateway("no refresh token granted"))?;
    let subject = g
        .id_token
        .as_deref()
        .and_then(|t| t.split('.').nth(1))
        .and_then(unb64)
        .and_then(|b| serde_json::from_slice::<IdClaims>(&b).ok())
        .filter(|c| c.aud == app.oauth.client_id)
        .ok_or_else(|| bad_gateway("missing or foreign id_token"))?
        .sub;
    let tokens = AccountTokens {
        refresh: SecretString::from(refresh),
        access: SecretString::from(g.access_token),
        access_until: Timestamp::now() + SignedDuration::from_secs(g.expires_in.clamp(0, 86_400)),
    };

    let rec = app
        .run(move |a| {
            a.storage.open(pending.tenant)?;
            let found = match pending.account {
                Some(id) => {
                    let acc = a
                        .control
                        .account(id)?
                        .filter(|x| x.tenant == pending.tenant)
                        .ok_or(StoreError::NotFound)?;
                    if acc.subject != subject {
                        return Err(ApiError::new(
                            StatusCode::CONFLICT,
                            "conflict",
                            "signed in as a different Google account than the one to reconnect",
                        ));
                    }
                    Some(acc)
                }
                None => {
                    a.control
                        .account_by_subject(pending.tenant, &pending.provider, &subject)?
                }
            };
            let rec = match found {
                Some(mut acc) => {
                    if acc.state == AccountState::ReauthRequired {
                        acc.state = AccountState::Active;
                    }
                    acc
                }
                None => AccountRecord {
                    id: AccountId::from_bytes(random()),
                    tenant: pending.tenant,
                    provider: pending.provider,
                    subject,
                    state: AccountState::Connecting,
                },
            };
            a.control.upsert_account(&rec)?;
            a.control
                .write_tokens(rec.id, &a.sealer.seal(rec.tenant, rec.id, &tokens))?;
            Ok(rec)
        })
        .await?;
    // Discovers a new account (`connecting` → `active`) or rediscovers a reconnected one.
    app.engine
        .connect(rec.id)
        .await
        .map_err(|e| ApiError::internal(&e))?;
    Ok(Json(json!({ "account": rec.id, "state": rec.state })))
}
