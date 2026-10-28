//! Repo tools — how an agent's working directory becomes a thing the server
//! can coordinate on.

use of_core::repos::ReposExt;
use of_core::repos::{NewRepo, RepoPatch};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::{out, repo_for_write, repo_of, scope};
use crate::server::{Factory, McpResult};
use of_core::repos::Hold;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegisterRepoArgs {
    /// Short handle agents will use for this repo, unique in your organization
    /// (for example "contract-explorer" or "acme/api"). 1 to 128 characters:
    /// ASCII letters, digits, `-`, `_`, `.` and `/`, starting with a letter or
    /// digit. A slug outside that shape is refused with `invalid_repo_slug`,
    /// never trimmed or rewritten. Keep it stable: it appears in errors and in
    /// other people's scripts.
    pub slug: String,
    /// Human-readable name. Defaults to the slug.
    #[serde(default)]
    pub name: Option<String>,
    /// Every git remote URL that identifies this repo, in any spelling — SSH,
    /// HTTPS, with or without ".git". They are normalized, so
    /// "git@github.com:acme/api.git" and "https://github.com/acme/api" become
    /// one entry. Pass at least the output of `git remote get-url origin`.
    #[serde(default)]
    pub remotes: Vec<String>,
    /// Branch other tools assume when none is given. Defaults to "main".
    #[serde(default)]
    pub default_branch: Option<String>,
    /// Free-form hint about which coding agent usually works here (for example
    /// "claude-code"). Never enforced, never validated.
    #[serde(default)]
    pub default_agent_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListReposArgs {
    /// Include repos that have been retired. Defaults to false.
    #[serde(default)]
    pub include_inactive: bool,
    /// Maximum rows. Defaults to the server's own limit.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoRefArgs {
    /// A registered repo slug. Wins over `remote` when both are given.
    #[serde(default)]
    pub repo: Option<String>,
    /// A git remote URL in any form, normally `git remote get-url origin`.
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRepoArgs {
    /// Which repo to update: a registered slug.
    #[serde(default)]
    pub repo: Option<String>,
    /// Or a git remote URL identifying it.
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub default_agent_type: Option<String>,
    /// Retire or restore the repo. A retired repo keeps its job history but
    /// stops appearing in the default repo list.
    #[serde(default)]
    pub active: Option<bool>,
    /// Additional remote URLs that should resolve to this repo — a mirror, or
    /// the HTTPS form of a remote registered over SSH. Additive only.
    #[serde(default)]
    pub add_remotes: Vec<String>,
}

#[tool_router(router = repos_router, vis = "pub(crate)")]
impl Factory {
    /// Register a git repository so work can be queued against it.
    ///
    /// Call this when `resolve_repo` or any repo-taking tool tells you the
    /// working directory is not registered. Pass every remote spelling you know
    /// — they are normalized to one identity, so a teammate who cloned over
    /// HTTPS lands on the same repo you registered over SSH.
    #[tool(
        name = "register_repo",
        description = "Register a git repository in this organization so jobs, leases and \
                       messages can be anchored to it. Pass the output of `git remote get-url \
                       origin` as a remote; every spelling of the same repo resolves to one \
                       entry. The slug is a short identifier of 1 to 128 characters (ASCII letters, \
                       digits, `-`, `_`, `.`, `/`; it must start with a letter or digit, and no \
                       `/`-separated part may be empty, `.` or `..`): \
                       anything else is refused with `invalid_repo_slug` rather than \
                       rewritten, so choose one that is stable. Returns the registered repo."
    )]
    pub async fn register_repo(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RegisterRepoArgs>,
    ) -> Result<Json<out::RepoOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::REPOS_WRITE).mcp()?;

        // Any caller with repos:write may register: a repo registered here
        // takes no team, so it is org-wide and opens nothing new to anyone. The
        // scope still matters for the refusals, which must not name a repo of a
        // team the caller is not on (a remote that one already owns).
        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "register_repo").await?;
        let repo = tx
            .register_repo(NewRepo {
                slug: args.slug,
                name: args.name,
                remotes: args.remotes,
                default_branch: args.default_branch,
                default_agent_type: args.default_agent_type,
                created_by: Some(caller.user_id),
                ..Default::default()
            })
            .await
            .map_err(|e| team.redact_foreign_ids(e))
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::RepoOut { repo }))
    }

    /// What repos exist here.
    #[tool(
        name = "list_repos",
        description = "List the repositories registered in this organization, with their slugs, \
                       default branches and provider. Call this when a repo cannot be resolved, \
                       to find out what is available."
    )]
    pub async fn list_repos(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListReposArgs>,
    ) -> Result<Json<out::ReposOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::REPOS_READ).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "list_repos").await?;
        let repos = tx
            .list_repos_visible(args.include_inactive, args.limit, &team)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::ReposOut { repos }))
    }

    /// Turn a working directory into a repo identity.
    #[tool(
        name = "resolve_repo",
        description = "Work out which registered repository a slug or git remote URL refers to. \
                       Use it once at the start of a session to confirm where you are. Fails \
                       with the list of registered slugs rather than guessing."
    )]
    pub async fn resolve_repo(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoRefArgs>,
    ) -> Result<Json<out::RepoOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::REPOS_READ).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "resolve_repo").await?;
        let repo = repo_of(&mut tx, &team, args.repo, args.remote).await?;
        tx.commit().await.mcp()?;

        Ok(Json(out::RepoOut { repo }))
    }

    /// Change a repo's settings, or attach another remote to it.
    #[tool(
        name = "update_repo",
        description = "Change a registered repository's name, default branch, default agent \
                       type, or active flag, or attach additional remote URLs to it. Only the \
                       fields you pass are changed. The slug cannot be changed, because other \
                       people's scripts name it."
    )]
    pub async fn update_repo(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateRepoArgs>,
    ) -> Result<Json<out::RepoOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::REPOS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "update_repo").await?;
        let repo = repo_for_write(&mut tx, &team, args.repo, args.remote, Hold::ForUpdate).await?;
        let updated = tx
            .update_repo(
                repo.id,
                RepoPatch {
                    name: args.name,
                    default_branch: args.default_branch,
                    default_agent_type: args.default_agent_type,
                    active: args.active,
                    add_remotes: args.add_remotes,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| team.redact_foreign_ids(e))
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::RepoOut { repo: updated }))
    }
}
