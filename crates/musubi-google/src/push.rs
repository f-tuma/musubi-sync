//! `PushSource`: Google watch channels (`events.watch`, `channels.stop`).

use async_trait::async_trait;
use http::{HeaderMap, StatusCode, Uri};
use jiff::Timestamp;
use musubi_model::provider::{ChannelId, PushHint, PushSource, Subscription};
use musubi_model::{AccountCx, CollectionRef, PushRejected, SyncError};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::provider::{GoogleProvider, error_for, permanent};

const CHANNEL_ID: &str = "x-goog-channel-id";
const CHANNEL_TOKEN: &str = "x-goog-channel-token";
const RESOURCE_ID: &str = "x-goog-resource-id";
const RESOURCE_STATE: &str = "x-goog-resource-state";

/// The parts of the `events.watch` reply we need. Lenient, so a reply missing
/// one still yields the other (see [`GoogleProvider::subscribe`]).
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Channel {
    resource_id: Option<String>,
    expiration: Option<serde_json::Value>,
}

/// Google sends `expiration` as a decimal string of epoch milliseconds.
fn millis(v: &serde_json::Value) -> Option<Timestamp> {
    let ms = match v {
        serde_json::Value::String(s) => s.parse().ok()?,
        v => v.as_i64()?,
    };
    Timestamp::from_millisecond(ms).ok()
}

/// `n` random bytes as lowercase hex, from rustls' CSPRNG.
fn random_hex(n: usize) -> Result<String, SyncError> {
    let mut buf = vec![0u8; n];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut buf)
        .map_err(|e| SyncError::Permanent(format!("no randomness: {e:?}").into()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn header<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Equality that takes the same time wherever the first difference is.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl GoogleProvider {
    /// `channels.stop`. Closing an unknown or expired channel is `Ok`.
    async fn stop(&self, cx: &AccountCx, id: &str, resource: &str) -> Result<(), SyncError> {
        let body = serde_json::json!({ "id": id, "resourceId": resource });
        let url = self.url(&["channels", "stop"]);
        let resp = self.send(cx, self.http.post(url).json(&body)).await?;
        match resp.status() {
            s if s.is_success() => Ok(()),
            // Already stopped, expired or never known.
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(()),
            _ => Err(error_for(resp, cx.clock.now()).await),
        }
    }
}

#[async_trait]
impl PushSource for GoogleProvider {
    async fn subscribe(
        &self,
        cx: &AccountCx,
        col: &CollectionRef,
        callback: &Uri,
    ) -> Result<Subscription, SyncError> {
        let id = random_hex(16)?;
        let token = random_hex(32)?;
        let body = serde_json::json!({
            "id": id,
            "type": "web_hook",
            "address": callback.to_string(),
            "token": token,
        });
        let url = self.url(&["calendars", &col.0, "events", "watch"]);
        let bytes = self.call(cx, self.http.post(url).json(&body)).await?;
        let ch: Channel = serde_json::from_slice(&bytes).unwrap_or_default();
        match (ch.resource_id, ch.expiration.as_ref().and_then(millis)) {
            (Some(resource), Some(expiration)) => Ok(Subscription {
                channel: ChannelId(id),
                collection: col.clone(),
                resource,
                token: SecretString::from(token),
                expiration,
            }),
            (resource, _) => {
                // The channel is open at Google but unusable here: close it
                // rather than leak it until it expires. Best effort.
                if let Some(resource) = resource {
                    let _ = self.stop(cx, &id, &resource).await;
                }
                Err(permanent(
                    "events.watch: reply without resourceId or valid expiration",
                ))
            }
        }
    }

    async fn unsubscribe(&self, cx: &AccountCx, s: &Subscription) -> Result<(), SyncError> {
        self.stop(cx, &s.channel.0, &s.resource).await
    }

    fn channel_of(&self, headers: &HeaderMap) -> Option<ChannelId> {
        header(headers, CHANNEL_ID).map(|c| ChannelId(c.to_owned()))
    }

    fn verify(&self, headers: &HeaderMap, known: &Subscription) -> Result<PushHint, PushRejected> {
        let missing = |h: &str| PushRejected::Malformed(format!("missing {h}"));
        let channel = header(headers, CHANNEL_ID).ok_or_else(|| missing(CHANNEL_ID))?;
        let resource = header(headers, RESOURCE_ID).ok_or_else(|| missing(RESOURCE_ID))?;
        let state = header(headers, RESOURCE_STATE).ok_or_else(|| missing(RESOURCE_STATE))?;
        if channel != known.channel.0 || resource != known.resource {
            return Err(PushRejected::UnknownChannel);
        }
        let token = header(headers, CHANNEL_TOKEN).ok_or(PushRejected::BadToken)?;
        if !constant_time_eq(token.as_bytes(), known.token.expose_secret().as_bytes()) {
            return Err(PushRejected::BadToken);
        }
        if self.clock.now() > known.expiration {
            return Err(PushRejected::Expired);
        }
        Ok(match state {
            "sync" => PushHint::Handshake,
            _ => PushHint::Changed,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use musubi_model::provider::Clock;

    use super::*;
    use crate::{GoogleConfig, GoogleProvider};

    struct Fixed(Timestamp);
    impl Clock for Fixed {
        fn now(&self) -> Timestamp {
            self.0
        }
    }

    fn provider(now: &str) -> GoogleProvider {
        let config = GoogleConfig::new("client", SecretString::from("secret"));
        GoogleProvider::new(config, Arc::new(Fixed(now.parse().unwrap()))).unwrap()
    }

    fn sub(channel: &str, token: &str) -> Subscription {
        Subscription {
            channel: ChannelId(channel.into()),
            collection: CollectionRef("primary".into()),
            resource: "res-1".into(),
            token: SecretString::from(token),
            expiration: "2026-11-18T00:00:00Z".parse().unwrap(),
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|&(k, v)| (http::HeaderName::from_static(k), v.parse().unwrap()))
            .collect()
    }

    fn notification(channel: &str, token: &str, state: &str) -> HeaderMap {
        headers(&[
            (CHANNEL_ID, channel),
            (CHANNEL_TOKEN, token),
            (RESOURCE_ID, "res-1"),
            (RESOURCE_STATE, state),
            ("x-goog-message-number", "7"),
        ])
    }

    #[test]
    fn verify_accepts_its_own_channel_only() {
        let p = provider("2026-11-11T12:00:00Z");
        let a = sub("chan-a", "token-a");
        let b = sub("chan-b", "token-b");

        assert_eq!(
            p.verify(&notification("chan-a", "token-a", "sync"), &a),
            Ok(PushHint::Handshake)
        );
        assert_eq!(
            p.verify(&notification("chan-a", "token-a", "exists"), &a),
            Ok(PushHint::Changed)
        );
        assert_eq!(
            p.channel_of(&notification("chan-a", "token-a", "exists")),
            Some(a.channel.clone())
        );

        // Channel A's id with channel B's token: forged or crossed wires.
        assert_eq!(
            p.verify(&notification("chan-a", "token-b", "exists"), &a),
            Err(PushRejected::BadToken)
        );
        // A notification of channel B checked against A.
        assert_eq!(
            p.verify(&notification("chan-b", "token-b", "exists"), &a),
            Err(PushRejected::UnknownChannel)
        );
        assert_eq!(
            p.verify(&notification("chan-b", "token-b", "exists"), &b),
            Ok(PushHint::Changed)
        );
        let mut no_token = notification("chan-a", "token-a", "exists");
        no_token.remove(CHANNEL_TOKEN);
        assert_eq!(p.verify(&no_token, &a), Err(PushRejected::BadToken));
        let mut no_state = notification("chan-a", "token-a", "exists");
        no_state.remove(RESOURCE_STATE);
        assert!(matches!(
            p.verify(&no_state, &a),
            Err(PushRejected::Malformed(_))
        ));
        assert_eq!(p.channel_of(&HeaderMap::new()), None);
    }

    #[test]
    fn verify_rejects_after_expiration() {
        let p = provider("2026-11-18T00:00:01Z");
        assert_eq!(
            p.verify(
                &notification("chan-a", "token-a", "exists"),
                &sub("chan-a", "token-a")
            ),
            Err(PushRejected::Expired)
        );
    }

    #[test]
    fn expiration_is_epoch_millis_as_text_or_number() {
        let at: Timestamp = "2026-11-18T00:00:00Z".parse().unwrap();
        let ms = at.as_millisecond();
        assert_eq!(millis(&serde_json::json!(ms.to_string())), Some(at));
        assert_eq!(millis(&serde_json::json!(ms)), Some(at));
        assert_eq!(millis(&serde_json::json!("tomorrow")), None);
        let (a, b) = (random_hex(16).unwrap(), random_hex(16).unwrap());
        assert!(a.len() == 32 && a != b);
    }
}
