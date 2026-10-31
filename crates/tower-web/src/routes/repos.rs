//! Repos — the coordination anchor, from the console side.
//!
//! The same rows the MCP tools `register_repo` and `update_repo` write. There is
//! deliberately no second code path: both surfaces call the same `tower-core`
//! functions, so a repo registered by an agent and one registered by a human in
//! the console are the same thing, resolvable by the same remotes.
//!
//! The request types here are tower-web's own rather than `tower_core::NewRepo`
//! deserialized directly. `NewRepo` carries `created_by`, which the *server*
//! decides from the session — a client that could set it would be able to
//! attribute its registrations to somebody else. It also carries
//! `tracker_binding`, the free-form JSON blob from Milestone 1, which the
//! console deliberately no longer writes: `tracker_bindings` rows created
//! through `routes::trackers` are what webhook ingest and the sync engine
//! actually read, and two fields describing the same thing where only one is
//! consulted is a trap rather than a convenience.

use axum::extract::{Json, Path, State};
use axum::response::{IntoResponse, Response};
use savvagent_tenant::audit::Entry;
use savvagent_tenant::ids::TeamId;
use serde::{Deserialize, Serialize};
use tower_core::audit::action;
use tower_core::leases::Lease;
use tower_core::leases::LeasesExt;
use tower_core::repos::ReposExt;
use tower_core::repos::{Hold, NewRepo, Provider, Repo, RepoPatch};
use tower_core::scopes;
use tower_core::teams::{TeamScope, VerifiedTeam};

use crate::error::ApiResult;
use crate::session::OrgCtx;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterRepoRequest {
    /// The short handle agents will use. Unique per org.
    pub slug: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Every git remote that identifies this repo, in any form git prints.
    /// Normalized before storage, so SSH and HTTPS forms of one repo collapse
    /// to a single row.
    #[serde(default)]
    pub remotes: Vec<String>,
    #[serde(default)]
    pub provider: Option<Provider>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub team_id: Option<TeamId>,
    #[serde(default)]
    pub default_agent_type: Option<String>,
}

/// A partial update. Absent fields are left alone — see `tower_core::RepoPatch`
/// for why this is a PATCH and not a PUT, and why the slug is not in it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRepoRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    /// Absent leaves the team alone; an explicit `null` makes the repo
    /// org-wide. The two have to be distinguishable, or a team-scoped repo can
    /// never be unscoped, including after its team has been deleted at the platform.
    #[serde(default, deserialize_with = "super::double_option")]
    pub team_id: Option<Option<TeamId>>,
    #[serde(default)]
    pub default_agent_type: Option<String>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub add_remotes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListReposQuery {
    /// Include repos that have been soft-disabled. Off by default: a retired
    /// repo keeps its job history but should not clutter a picker.
    #[serde(default)]
    pub include_inactive: bool,
    /// Compute `hasActiveLease` on every returned repo. Off by default: this
    /// endpoint is also polled every 30 seconds by the org overview page and
    /// fetched by the queue pages' repo picker, neither of which shows lease
    /// presence, and the computation is one extra org-wide `list_leases` scan
    /// — worth paying only for the one caller (the Repos page) that renders
    /// it.
    #[serde(default)]
    pub include_lease_status: bool,
}

// Team visibility, per `docs/specs/2026-09-01-otto-factory-design.md`: a repo with
// a `team_id` is visible only to that team's members and org admins; a null
// `team_id` is org-wide. Membership is the platform's (`OrgCtx::team_scope`) and
// fails closed. A repo whose team the platform has since deleted keeps its
// dangling `team_id` (see `tower_core::platform_events`), which matches nobody's
// team list, so it is admin-only until reassigned.

/// Resolve a repo slug as the caller may see it. A repo of a team the caller is
/// not on is `404` exactly like one that does not exist, and the error's list of
/// registered slugs omits it.
pub(crate) async fn resolve_visible(
    tx: &mut savvagent_tenant::Tx<'_>,
    scope: &TeamScope,
    slug: String,
) -> ApiResult<Repo> {
    Ok(tx
        .resolve_repo_visible(
            &tower_core::repos::RepoRef {
                slug: Some(slug),
                remote: None,
            },
            scope,
        )
        .await?)
}

/// A repo plus, when `?includeLeaseStatus=true` asked for it, whether anyone
/// holds a live lease on it right now. Computed once for the whole page from
/// the same live-lease read the `list_leases` handler already uses for a
/// single repo, grouped once instead of fetched per row — avoiding one query
/// per repo on every page load. Omitted from the wire response entirely
/// (rather than `false`) when not requested, so a caller that never asked
/// can't mistake "not computed" for "known absent".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoListItem {
    #[serde(flatten)]
    pub repo: Repo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_active_lease: Option<bool>,
}

/// `GET /api/orgs/{org}/repos`
pub async fn list_repos(
    State(state): State<AppState>,
    ctx: OrgCtx,
    axum::extract::Query(q): axum::extract::Query<ListReposQuery>,
) -> ApiResult<Json<Vec<RepoListItem>>> {
    ctx.require_scope(scopes::REPOS_READ)?;
    // Lease activity is a `jobs:read` fact on the MCP surface (`list_leases`);
    // asking for it here must not be a way around that.
    if q.include_lease_status {
        ctx.require_scope(scopes::JOBS_READ)?;
    }
    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    let repos = tx
        .list_repos_visible(q.include_inactive, None, &scope)
        .await?;

    let active: Option<std::collections::HashSet<_>> = if q.include_lease_status {
        Some(
            tx.list_leases(None)
                .await?
                .into_iter()
                .map(|l| l.repo_id)
                .collect(),
        )
    } else {
        None
    };
    tx.commit().await?;

    Ok(Json(
        repos
            .into_iter()
            .map(|repo| RepoListItem {
                has_active_lease: active.as_ref().map(|a| a.contains(&repo.id)),
                repo,
            })
            .collect(),
    ))
}

/// `POST /api/orgs/{org}/repos` — register a repo.
pub async fn register_repo(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Json(req): Json<RegisterRepoRequest>,
) -> ApiResult<Response> {
    ctx.require_admin()?;
    ctx.require_scope(scopes::REPOS_WRITE)?;

    // Fail closed. A team id is written onto the repo only after the platform
    // confirms it exists in this org: an unknown, foreign, or deleted team is
    // refused (404), and a platform that cannot answer is a 503. Neither is ever
    // read as "no team" -- a null team is org-wide.
    let team_id = match req.team_id {
        Some(team) => Some(VerifiedTeam::verify(&state.platform, ctx.org.id, team).await?),
        None => None,
    };

    // A remote some other repo already owns is refused naming that repo. For a
    // caller limited to some teams that repo may be hidden, so the refusal is
    // redacted the same way the MCP tools redact it. Owners and admins are always
    // `TeamScope::All` (`TeamScope::for_member`), so behind `require_admin` this
    // is a no-op; it is here so the `remote_taken` refusal does not rest on that
    // gate, which decides who may register repos, not what they may see.
    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    let repo = tx
        .register_repo(NewRepo {
            slug: req.slug,
            name: req.name,
            remotes: req.remotes,
            provider: req.provider,
            default_branch: req.default_branch,
            team_id,
            default_agent_type: req.default_agent_type,
            // The free-form `repos.tracker_binding` JSON blob is not writable
            // from the console. `tracker_bindings` — structured, linked to a
            // connection, and the only thing webhook ingest and the sync engine
            // read — replaced it; see `routes::trackers`. The column and its
            // `NewRepo`/`RepoPatch` fields stay, because the MCP tools still
            // accept them and this task is not a change to the agent surface.
            tracker_binding: None,
            // From the session, never from the request.
            created_by: Some(ctx.user.id),
        })
        .await
        .map_err(|e| scope.redact_foreign_ids(e))?;

    tx.audit(
        Entry::new(action::REPO_REGISTERED)
            .actor(ctx.user.id)
            .target("repo", repo.slug.clone()),
    )
    .await?;
    tx.commit().await?;

    Ok((http::StatusCode::CREATED, Json(repo)).into_response())
}

/// `GET /api/orgs/{org}/repos/{repo}` — by slug.
pub async fn get_repo(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug)): Path<(String, String)>,
) -> ApiResult<Json<Repo>> {
    ctx.require_scope(scopes::REPOS_READ)?;
    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    let repo = resolve_visible(&mut tx, &scope, slug).await?;
    tx.commit().await?;
    Ok(Json(repo))
}

/// `PATCH /api/orgs/{org}/repos/{repo}`
pub async fn update_repo(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug)): Path<(String, String)>,
    Json(req): Json<UpdateRepoRequest>,
) -> ApiResult<Json<Repo>> {
    ctx.require_admin()?;
    ctx.require_scope(scopes::REPOS_WRITE)?;

    // Same rule as registration. `Some(None)` (an explicit null) makes the repo
    // org-wide, which needs no verification because it names no team; that is
    // also how an admin releases a repo whose team the platform has deleted.
    let team_id = match req.team_id {
        Some(Some(team)) => Some(Some(
            VerifiedTeam::verify(&state.platform, ctx.org.id, team).await?,
        )),
        Some(None) => Some(None),
        None => None,
    };

    // The target is resolved through the caller's scope and held for update, as
    // MCP's `update_repo` does: a repo of a team the caller is not on is `404`
    // like one that does not exist (its slug kept out of the error's list), and
    // an admin's team reassignment cannot commit between this check and the
    // write. `add_remotes` can also hit a remote another repo owns; that refusal
    // is redacted as in `register_repo`. Owners and admins are `TeamScope::All`,
    // so behind `require_admin` neither changes anything today.
    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    let repo = tx
        .resolve_repo_visible_held(
            &tower_core::repos::RepoRef {
                slug: Some(slug),
                remote: None,
            },
            &scope,
            Hold::ForUpdate,
        )
        .await?;

    let repo = tx
        .update_repo(
            repo.id,
            RepoPatch {
                name: req.name,
                default_branch: req.default_branch,
                team_id,
                default_agent_type: req.default_agent_type,
                // Not writable from the console — see `register_repo`.
                tracker_binding: None,
                active: req.active,
                add_remotes: req.add_remotes,
            },
        )
        .await
        .map_err(|e| scope.redact_foreign_ids(e))?;

    tx.audit(
        Entry::new(action::REPO_UPDATED)
            .actor(ctx.user.id)
            .target("repo", repo.slug.clone()),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(repo))
}

/// `GET /api/orgs/{org}/repos/{repo}/leases` — who is in this repo right now.
///
/// The console's answer to "why is my agent waiting?". Read-only, and open to
/// any member **of this repo's team** (or any admin): a lease is a
/// coordination signal, but a team-scoped repo's leases are exactly the kind
/// of team-scoped data `require_visible` exists to keep away from members of
/// other teams.
pub async fn list_leases(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug)): Path<(String, String)>,
) -> ApiResult<Json<Vec<Lease>>> {
    ctx.require_scope(scopes::REPOS_READ)?;
    // The same scope MCP `list_leases` requires, so switching transports
    // cannot widen what a token may read.
    ctx.require_scope(scopes::JOBS_READ)?;
    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    let repo = resolve_visible(&mut tx, &scope, slug).await?;
    let mut leases = tx.list_leases(Some(repo.id)).await?;
    tower_core::leases::hide_unseen_jobs(&mut tx, &mut leases, &scope).await?;
    tx.commit().await?;
    Ok(Json(leases))
}
