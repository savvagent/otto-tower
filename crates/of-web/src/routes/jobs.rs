//! The queue, read-only.
//!
//! The console watches the queue; it does not drive it. Every write — enqueue,
//! claim, complete, fail, repend — belongs to the MCP surface, because the
//! agent doing the work is the only party that knows when the work is done.
//! Putting a "mark complete" button here would put a human in a position to lie
//! to the queue about state they cannot observe.
//!
//! So this module is three `GET`s: the list, one job, and the counters behind
//! the overview page. They exist because "why is my agent waiting?" is a
//! question a person asks in a browser, and answering it by telling them to
//! attach an MCP client is not an answer.
//!
//! **Filters name slugs, not ids.** A console URL carries `?repo=api-gateway`,
//! not a UUID, for the same reason repo and team routes take slugs: a slug is
//! what a person types and what a link can be shared as. An unregistered slug
//! is an error listing what *is* registered — `resolve_repo` and `resolve_team`
//! already write that message, and a filter that silently matched nothing would
//! render an empty queue that looks like a quiet one.

use of_core::jobs::JobsExt;
use of_core::repos::ReposExt;
use of_core::scopes;
use std::str::FromStr;

use axum::extract::{Json, Path, Query, State};
use of_core::ids::JobId;
use of_core::jobs::{Job, JobFilter, Stats, Status};
use serde::Deserialize;

use crate::error::{ApiError, ApiResult};
use crate::session::OrgCtx;
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListJobsQuery {
    /// `pending` | `in-progress` | `active` | `completed` | `failed` | `cancelled`.
    #[serde(default)]
    pub status: Option<String>,
    /// A registered repo slug.
    #[serde(default)]
    pub repo: Option<String>,
    /// A team slug.
    #[serde(default)]
    pub team: Option<String>,
    /// Only jobs this account queued. The "what did I ask for?" view.
    #[serde(default)]
    pub mine: bool,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsQuery {
    #[serde(default)]
    pub repo: Option<String>,
}

/// Resolve a team slug in the caller's org through the platform, failing closed.
async fn resolve_team(
    state: &AppState,
    ctx: &OrgCtx,
    slug: &str,
) -> ApiResult<otto_tenant::ids::TeamId> {
    let not_found = || {
        ApiError::not_found(format!(
            "no team {slug:?} in this organization; team slugs are managed in the otto \
             platform console"
        ))
    };
    // The slug goes into a URL path at the platform; one that could not name a
    // team is refused here, the same way the message tools refuse it.
    if !of_core::teams::team_slug_shape_ok(slug) {
        return Err(not_found());
    }
    match state
        .platform
        .team_by_slug(ctx.org.id.as_uuid(), slug)
        .await
    {
        // The platform scopes the lookup by org; check the answer anyway.
        Ok(Some(team)) if team.org_id == ctx.org.id.as_uuid() => Ok(team.id.into()),
        Ok(_) => Err(not_found()),
        Err(e) => Err(ApiError::platform_unavailable("resolve a team slug", e)),
    }
}

/// `GET /api/orgs/{org}/jobs` — the queue, newest first.
pub async fn list_jobs(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Query(q): Query<ListJobsQuery>,
) -> ApiResult<Json<Vec<Job>>> {
    // Parsed before the transaction opens: a typo in a query string should not
    // cost a pooled connection, and `Status::from_str` already names the valid
    // values, which is what an error here has to do.
    let status = q
        .status
        .as_deref()
        .map(Status::from_str)
        .transpose()
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    ctx.require_scope(scopes::JOBS_READ)?;

    // A team slug is the platform's to resolve. Asked before the transaction
    // opens, so a slow platform costs no pooled connection, and **failing closed**:
    // a slug the platform does not know is an error, never "no filter" (which
    // would list every team's jobs), and a platform that cannot answer is a 503.
    let team_id = match &q.team {
        Some(slug) => Some(resolve_team(&state, &ctx, slug).await?),
        None => None,
    };

    // Which teams' jobs the caller may see, from the platform and failing
    // closed (503), for the same reason and at the same point as the slug above.
    let scope = ctx.team_scope(&state.platform).await?;

    let mut tx = ctx.begin(&state.db).await?;

    let repo_id = match &q.repo {
        Some(slug) => Some(
            tx.resolve_repo_visible(
                &of_core::repos::RepoRef {
                    slug: Some(slug.clone()),
                    remote: None,
                },
                &scope,
            )
            .await?
            .id,
        ),
        None => None,
    };

    let jobs = tx
        .list_jobs(&JobFilter {
            status,
            repo_id,
            team_id,
            visible_teams: scope.restriction(),
            created_by: q.mine.then_some(ctx.user.id),
            agent_type: None,
            ticket: None,
            limit: q.limit,
        })
        .await?;
    tx.commit().await?;

    Ok(Json(jobs))
}

/// `GET /api/orgs/{org}/jobs/stats` — the counters behind the overview.
///
/// `blocked` is the one that is not a status: it counts pending jobs still
/// waiting on a dependency, which is the difference between a queue that is
/// idle and a queue that is stuck.
pub async fn job_stats(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Json<Stats>> {
    ctx.require_scope(scopes::JOBS_READ)?;
    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;

    let repo_id = match &q.repo {
        Some(slug) => Some(
            tx.resolve_repo_visible(
                &of_core::repos::RepoRef {
                    slug: Some(slug.clone()),
                    remote: None,
                },
                &scope,
            )
            .await?
            .id,
        ),
        None => None,
    };

    // The org-wide counters have no team dimension, so a member's numbers are
    // counted from the jobs they may see.
    let stats = match scope.restriction() {
        None => tx.stats(repo_id).await?,
        Some(teams) => tx.stats_for_teams(repo_id, &teams).await?,
    };
    tx.commit().await?;

    Ok(Json(stats))
}

/// `GET /api/orgs/{org}/jobs/{job}` — one job, with the ids it waits on.
pub async fn get_job(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, id)): Path<(String, String)>,
) -> ApiResult<Json<JobDetail>> {
    ctx.require_scope(scopes::JOBS_READ)?;
    let id = JobId::from(id);

    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    // A job of a team the caller is not on (its own, or its repo's) is not
    // found, not forbidden: they should not learn it exists.
    let job = tx.get_job_visible(&id, &scope).await?;
    // Nor are the ids of dependencies they could not open.
    let depends_on = tx.dependencies_of(&id).await?;
    let depends_on = if scope.is_all() {
        depends_on
    } else {
        tx.visible_job_ids(&depends_on, &scope).await?
    };
    tx.commit().await?;

    Ok(Json(JobDetail { job, depends_on }))
}

/// One job plus its dependencies.
///
/// Flattened rather than nested, so a caller that only wants the job reads it
/// exactly as it reads a list entry. The dependency ids come from a second
/// query and are not on `Job` itself, which is why they cannot simply be a
/// column.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobDetail {
    #[serde(flatten)]
    pub job: Job,
    /// Job ids that must reach `completed` before this one is claimable.
    pub depends_on: Vec<JobId>,
}
