//! The Savvagent platform's lifecycle webhooks.
//!
//! The platform owns orgs, teams, and memberships and tells every registered
//! resource server when it deletes one (`org.deleted`, `team.deleted`,
//! `member.removed`). This is the receiving end: verify the signature, then hand
//! the event to `tower_core::platform_events`, which does the (idempotent) clean-up.
//!
//! Mounted at `/platform/webhooks`, not under `/webhooks/{provider}`: that path
//! belongs to tracker deliveries and resolves its `{provider}` segment against
//! GitHub and JIRA only. A separate prefix means a platform event can never be
//! mistaken for a tracker one, and the two secrets can never be confused.
//!
//! **The status code is the retry protocol.** The platform redelivers anything
//! that is not a 2xx, with backoff, until its budget runs out. So:
//!
//! - a bad or missing signature is `401` and is never processed (the body is not
//!   even parsed before the MAC checks out);
//! - a handled event, a repeat, and an event type this version does not know are
//!   all `200`: retrying them would change nothing;
//! - a failure to apply (a database error) is `5xx`, so the platform tries again.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use savvagent_resource::webhook::{self, WebhookError};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

fn rejected() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_signature",
        "the Otto-Signature header is missing, malformed, stale, or does not match the body",
    )
}

pub async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<tower_core::platform_events::Outcome>> {
    let Some(signature) = headers
        .get(webhook::SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        tracing::warn!(
            "platform webhook without an {} header",
            webhook::SIGNATURE_HEADER
        );
        return Err(rejected());
    };

    let event = match webhook::verify(&state.config.platform_webhook_secret, signature, &body) {
        Ok(event) => event,
        // Signed correctly but not an event we can read: the two sides are on
        // incompatible versions. Not a signature problem, and not retriable by
        // the platform in any useful way, but say what it is.
        Err(WebhookError::BadBody(reason)) => {
            tracing::error!(%reason, "platform webhook was signed but unreadable");
            return Err(ApiError::bad_request(
                "the webhook body is not a valid event",
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "platform webhook rejected");
            return Err(rejected());
        }
    };

    let outcome = tower_core::platform_events::apply(&state.db, &event)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(outcome))
}
