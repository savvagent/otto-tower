//! This service's audit trail, read-only.
//!
//! The domain events Savvagent Tower records itself (`repo.*`, `tracker.*`, `job.*`,
//! and the clean-up it performs for the platform's lifecycle webhooks), from the
//! `audit_events` table in this service's own database. Identity and sign-in
//! events are the platform's, in the platform's audit log.

use axum::extract::{Json, Query, State};
use of_core::scopes;
use otto_tenant::audit::AuditEvent;
use serde::Deserialize;

use crate::error::ApiResult;
use crate::session::OrgCtx;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditQuery {
    /// Restrict to one family of events — `repo.`, `tracker.`, `job.`, `platform.`.
    #[serde(default)]
    pub action_prefix: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/orgs/{org}/audit` — this service's log for the org.
///
/// Admin-only, unlike the rest of the console's reads: who connected which
/// tracker and who changed which repo is the trail an attacker with a
/// low-privilege session would want to read before deciding whom to target.
pub async fn get_audit(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<Vec<AuditEvent>>> {
    ctx.require_admin()?;
    ctx.require_scope(scopes::ORG_ADMIN)?;

    let mut tx = ctx.begin(&state.db).await?;
    let events = tx
        .audit_trail(q.action_prefix.as_deref(), q.limit.unwrap_or(100))
        .await?;
    tx.commit().await?;
    Ok(Json(events))
}
