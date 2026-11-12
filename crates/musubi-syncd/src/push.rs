//! `POST /v1/webhooks/google`: push notifications of watch channels.
//!
//! The channel is looked up in the control store and the notification checked
//! with [`PushSource::verify`] before anything is written. A rejected one
//! enqueues nothing. An accepted change goes to [`crate::SyncEngine::on_push`],
//! whose push job's `dedup` key and short delay coalesce a burst of
//! notifications into one delta read, run when due. The reply does not wait
//! for the sync.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use musubi_model::provider::PushHint;
use musubi_model::{ControlStore, PushRejected, PushSource};

use crate::{ApiError, App};

pub(crate) async fn google(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let id = app
        .google
        .channel_of(&headers)
        .ok_or_else(|| ApiError::bad_request("not a channel notification"))?;
    let changed = app
        .run(move |a| {
            let ch = a
                .control
                .find_channel(&id)?
                .ok_or_else(ApiError::not_found)?;
            // A pending channel (reply not stored yet, or lost) has no token to
            // verify against: as unknown as no row at all.
            let sub = ch.open().ok_or_else(ApiError::not_found)?;
            match a.google.verify(&headers, sub) {
                Ok(PushHint::Handshake) => Ok(None),
                Ok(PushHint::Changed) => Ok(Some(ch)),
                Err(PushRejected::UnknownChannel) => Err(ApiError::not_found()),
                Err(PushRejected::BadToken) => Err(ApiError::unauthorized()),
                Err(PushRejected::Expired) => Err(ApiError::new(
                    StatusCode::GONE,
                    "expired",
                    "channel expired",
                )),
                Err(PushRejected::Malformed(m)) => Err(ApiError::bad_request(m)),
                Err(e) => Err(ApiError::bad_request(e.to_string())),
            }
        })
        .await?;
    if let Some(ch) = changed {
        app.engine
            .on_push(&ch)
            .await
            .map_err(|e| ApiError::internal(&e))?;
    }
    Ok(StatusCode::OK)
}
