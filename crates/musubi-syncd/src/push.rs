//! `POST /v1/webhooks/google`: push notifications of watch channels.
//!
//! The channel is looked up in the control store and the notification checked
//! with [`PushSource::verify`] before anything is written. A rejected one
//! enqueues nothing. An accepted change enqueues the engine's push job
//! ([`crate::SyncEngine::push_job`]); its `dedup` key and short delay coalesce a
//! burst of notifications into one delta read. The reply does not wait for the
//! sync.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use jiff::Timestamp;
use musubi_model::provider::PushHint;
use musubi_model::{ControlStore, PushRejected, PushSource};

use crate::{ApiError, App, SyncEngine};

pub(crate) async fn google(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let id = app
        .google
        .channel_of(&headers)
        .ok_or_else(|| ApiError::bad_request("not a channel notification"))?;
    app.run(move |a| {
        let ch = a
            .control
            .find_channel(&id)?
            .ok_or_else(ApiError::not_found)?;
        match a.google.verify(&headers, &ch.sub) {
            Ok(PushHint::Handshake) => {}
            Ok(PushHint::Changed) => {
                a.control.enqueue(&SyncEngine::push_job(
                    ch.tenant,
                    ch.account,
                    ch.collection,
                    Timestamp::now(),
                ))?;
            }
            Err(PushRejected::UnknownChannel) => return Err(ApiError::not_found()),
            Err(PushRejected::BadToken) => return Err(ApiError::unauthorized()),
            Err(PushRejected::Expired) => {
                return Err(ApiError::new(
                    StatusCode::GONE,
                    "expired",
                    "channel expired",
                ));
            }
            Err(PushRejected::Malformed(m)) => return Err(ApiError::bad_request(m)),
            Err(e) => return Err(ApiError::bad_request(e.to_string())),
        }
        Ok(StatusCode::OK)
    })
    .await
}
