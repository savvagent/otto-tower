//! Tracker connections and per-repo bindings, from the console side.
//!
//! Tasks 1–5 of Milestone 2 built a sync engine that reads
//! `tracker_connections` and `tracker_bindings`. This is the only thing that
//! writes them. Nothing in the MCP surface does, on purpose: connecting a
//! tracker is an admin act performed by a human in a browser, and the agent
//! side of the product has no business holding a provider's OAuth credentials.
//!
//! **The network work happens before the transaction, not inside it.** Both
//! connect flows make two provider round trips, and holding a pinned
//! transaction open across them would keep a database connection per admin
//! sitting on a consent screen. The cost of getting this order right is that a
//! failed *write* leaves a spent authorization code behind and the admin runs
//! the flow again; the cost of getting it wrong is a connection pool held
//! hostage by GitHub's latency.

use axum::extract::{Json, Path, State};
use axum::response::{IntoResponse, Response};
use of_core::audit::action;
use of_core::trackers::{
    delete_binding, delete_connection, get_connection, list_bindings_for_repo, list_connections,
    resolve_binding, upsert_binding, upsert_connection, Provider, TrackerBinding,
    TrackerConnection,
};
use of_trackers::github::GithubUserAuth;
use of_trackers::jira::JiraClient;
use otto_tenant::audit::Entry;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ApiError, ApiResult};
use crate::session::OrgCtx;
use crate::state::AppState;

/// One connection, with nothing stored under `OF_ENCRYPTION_KEY` on it.
///
/// Deliberately not `of_core::TrackerConnection`, which derives `Serialize` and
/// would put `encrypted_credentials` on the wire. Ciphertext is not a secret in
/// the sense that leaking it grants access, but handing every admin's browser
/// the sealed JIRA refresh token is gratuitous exposure of exactly the material
/// the encryption key exists to protect. `hasCredentials` is the part the page
/// actually needs.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackerConnectionView {
    pub id: uuid::Uuid,
    pub provider: Provider,
    /// GitHub: the App installation id. JIRA: the cloud site id.
    pub external_id: String,
    pub has_credentials: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<TrackerConnection> for TrackerConnectionView {
    fn from(c: TrackerConnection) -> Self {
        Self {
            id: c.id,
            provider: c.provider,
            external_id: c.external_id,
            has_credentials: c.encrypted_credentials.is_some(),
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// What this deployment can take an admin through, per provider.
///
/// `configured` is the conjunction of every credential the flow needs,
/// computed on the server. A console that guessed would eventually offer a
/// Connect button on a deployment with no OAuth client, walking an admin
/// through installing an App they then have to uninstall by hand.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSetup {
    pub configured: bool,
    /// Where to send the browser to begin, minus its `state`. `None` whenever
    /// `configured` is false.
    pub start_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackerConnectionsView {
    pub connections: Vec<TrackerConnectionView>,
    pub github: ProviderSetup,
    pub jira: ProviderSetup,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackerBindingView {
    pub id: uuid::Uuid,
    pub repo_id: of_core::ids::RepoId,
    pub provider: Provider,
    pub external_ref: String,
    pub trigger_label: String,
    /// False when the org has no connection for this provider yet. The row is
    /// written and inert rather than refused — a repo may declare where its
    /// tickets live before an admin has connected the tracker.
    pub live: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<TrackerBinding> for TrackerBindingView {
    fn from(b: TrackerBinding) -> Self {
        Self {
            id: b.id,
            repo_id: b.repo_id,
            provider: b.provider,
            external_ref: b.external_ref,
            trigger_label: b.trigger_label,
            live: b.connection_id.is_some(),
            created_at: b.created_at,
            updated_at: b.updated_at,
        }
    }
}

/// The one-time artifact a provider handed the browser, on its way back.
///
/// One request type for both providers: GitHub sends an installation id
/// alongside its code and JIRA does not, and a second near-identical body would
/// be a second place for the two flows to drift.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectTrackerRequest {
    /// The authorization code from the provider's redirect. Single-use.
    pub code: String,
    /// GitHub only, and required there: the installation the admin is claiming.
    #[serde(default)]
    pub installation_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindRepoRequest {
    /// GitHub: `owner/repo`. JIRA: a project key such as `ACME`.
    pub external_ref: String,
    /// The label inbound sync watches for. Defaults to `otto-tower`, which is
    /// what the schema defaults to and what the docs tell customers to add.
    /// Bindings created before the default changed keep their stored label.
    #[serde(default)]
    pub trigger_label: Option<String>,
}

const DEFAULT_TRIGGER_LABEL: &str = "otto-tower";

/// Parse the `{provider}` path segment.
///
/// `Provider::from_str`'s error names both valid providers, which is the whole
/// point of routing this through `of-core` rather than matching two literals
/// here: a caller who typed `linear` learns what does exist.
fn provider_from_path(raw: &str) -> Result<Provider, ApiError> {
    Provider::from_str(raw).map_err(ApiError::from)
}

/// Turn a provider-call failure into something the admin can act on.
///
/// `of_trackers::Error`'s messages are already written for the person reading
/// them in a browser — "run Connect GitHub again", "the signed-in account does
/// not administer that installation". What this decides is the status: a
/// transport failure is the provider's fault (502, worth retrying), everything
/// else is something about this request (400, worth changing).
fn provider_error(e: of_trackers::Error) -> ApiError {
    match e {
        of_trackers::Error::Http { .. } => ApiError::new(
            http::StatusCode::BAD_GATEWAY,
            "tracker_unreachable",
            e.to_string(),
        ),
        other => ApiError::bad_request(other.to_string()),
    }
}

/// `GET /api/orgs/{org}/tracker-connections`
pub async fn list_tracker_connections(
    State(state): State<AppState>,
    ctx: OrgCtx,
) -> ApiResult<Json<TrackerConnectionsView>> {
    ctx.require_admin()?;
    ctx.require_scope(of_core::scopes::TRACKERS)?;

    let mut tx = ctx.begin(&state.db).await?;
    let connections = list_connections(&mut tx).await?;
    tx.commit().await?;

    Ok(Json(TrackerConnectionsView {
        connections: connections.into_iter().map(Into::into).collect(),
        github: ProviderSetup {
            configured: state.config.github_tracker_configured(),
            start_url: state.config.github_install_url(),
        },
        jira: ProviderSetup {
            configured: state.config.jira_tracker_configured(),
            start_url: state.config.jira_authorize_url(),
        },
    }))
}

/// `POST /api/orgs/{org}/tracker-connections/{provider}` — redeem what the
/// provider handed the browser, and record the connection.
///
/// A `POST` and never a `GET`, like every other single-use redemption in this
/// product: a link-preview fetcher that follows the provider's redirect URL
/// must burn nothing.
pub async fn connect_tracker(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, provider)): Path<(String, String)>,
    Json(req): Json<ConnectTrackerRequest>,
) -> ApiResult<Json<TrackerConnectionView>> {
    ctx.require_admin()?;
    ctx.require_scope(of_core::scopes::TRACKERS)?;
    let provider = provider_from_path(&provider)?;

    // Every provider round trip happens here, outside any transaction.
    let (external_id, sealed) = match provider {
        Provider::Github => (connect_github(&state, &req).await?, None),
        Provider::Jira => {
            let (site_id, sealed) = connect_jira(&state, &req).await?;
            (site_id, Some(sealed))
        }
    };

    let mut tx = ctx.begin(&state.db).await?;
    let connection = upsert_connection(&mut tx, provider, &external_id, sealed.as_ref(), None)
        .await
        .map_err(ApiError::from)?;
    tx.audit(
        Entry::new(action::TRACKER_CONNECTED)
            .actor(ctx.user.id)
            .target("tracker_connection", provider.to_string())
            // The external id is not a secret — it is an installation id or a
            // cloud site id — and it is the one field that tells an operator
            // reading the trail *which* installation was bound.
            .detail(serde_json::json!({ "externalId": external_id })),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(connection.into()))
}

/// Redeem GitHub's code and confirm the admin administers what they claimed.
///
/// The installation id is required rather than defaulted because it is the
/// entire subject of the verification: `tracker_connection_index` maps
/// `(provider, external_id)` globally to one org, so a wrong or invented id
/// claims somebody else's installation. See
/// `docs/specs/2026-09-04-tracker-console-design.md` §2.
async fn connect_github(state: &AppState, req: &ConnectTrackerRequest) -> Result<String, ApiError> {
    let (client_id, client_secret) = state
        .config
        .github_app_client_id
        .clone()
        .zip(state.config.github_app_client_secret.clone())
        .ok_or_else(|| {
            ApiError::bad_request(
                "GitHub tracker sync is not configured on this deployment (no GitHub App OAuth \
                 client). An operator sets OF_GITHUB_APP_SLUG, OF_GITHUB_APP_CLIENT_ID and \
                 OF_GITHUB_APP_CLIENT_SECRET.",
            )
        })?;

    let installation_id = req.installation_id.ok_or_else(|| {
        ApiError::bad_request(
            "connecting GitHub needs the installationId GitHub sent back with the code. If the \
             redirect carried no installation_id, the App was installed without completing the \
             setup redirect — run Connect GitHub again.",
        )
    })?;

    GithubUserAuth::new(client_id, client_secret)
        .map_err(provider_error)?
        .verify_installation_access(&req.code, installation_id)
        .await
        .map_err(provider_error)?;

    Ok(installation_id.to_string())
}

/// Redeem JIRA's code, read which site it was granted for, and seal the
/// refresh token.
///
/// The site id comes from Atlassian's own `accessible-resources`, never from
/// the request: the code proves this browser consented on this site, and the
/// resource list is Atlassian saying which site that was.
async fn connect_jira(
    state: &AppState,
    req: &ConnectTrackerRequest,
) -> Result<(String, otto_tenant::crypto::Sealed), ApiError> {
    let (client_id, client_secret) = state
        .config
        .jira_client_id
        .clone()
        .zip(state.config.jira_client_secret.clone())
        .ok_or_else(|| {
            ApiError::bad_request(
                "JIRA tracker sync is not configured on this deployment (no Atlassian OAuth \
                 client). An operator sets OF_JIRA_CLIENT_ID and OF_JIRA_CLIENT_SECRET.",
            )
        })?;

    let client = JiraClient::new(client_id, client_secret).map_err(provider_error)?;
    let tokens = client
        .exchange_code(&req.code, &state.config.tracker_callback_url())
        .await
        .map_err(provider_error)?;

    let sites = client
        .accessible_resources(&tokens.access_token)
        .await
        .map_err(provider_error)?;

    // One site per org is the v1 shape (`UNIQUE (org_id, provider)`), the code
    // is single-use so there is no second round trip to ask with, and choosing
    // one silently is how an org ends up syncing to a site nobody meant. Stop,
    // and name the sites so the admin knows which grant to narrow.
    let site = match sites.as_slice() {
        [only] => only,
        [] => {
            return Err(ApiError::bad_request(
                "that Atlassian authorization granted access to no JIRA site. Run Connect JIRA \
                 again and grant access to the site whose issues this org works from.",
            ))
        }
        many => {
            let names = many
                .iter()
                .map(|s| format!("{} ({})", s.name, s.url))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ApiError::bad_request(format!(
                "that Atlassian authorization covers {} JIRA sites ({names}), and \
                 Savvagent Tower stores one site per organization. Run Connect JIRA again and grant access to \
                 only the site this org works from.",
                many.len()
            )));
        }
    };

    let sealed = tokens
        .seal_refresh_token(&state.cipher)
        .map_err(provider_error)?;

    Ok((site.id.clone(), sealed))
}

/// `DELETE /api/orgs/{org}/tracker-connections/{provider}`
///
/// Allowed with live bindings on it. `tracker_bindings.connection_id` is
/// `ON DELETE SET NULL`, so those bindings go inert rather than invalid, and
/// `delete_connection` clears the global index row too — without which no other
/// org could ever claim that installation again.
pub async fn disconnect_tracker(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, provider)): Path<(String, String)>,
) -> ApiResult<Response> {
    ctx.require_admin()?;
    ctx.require_scope(of_core::scopes::TRACKERS)?;
    let provider = provider_from_path(&provider)?;

    let mut tx = ctx.begin(&state.db).await?;
    delete_connection(&mut tx, provider).await?;
    tx.audit(
        Entry::new(action::TRACKER_DISCONNECTED)
            .actor(ctx.user.id)
            .target("tracker_connection", provider.to_string()),
    )
    .await?;
    tx.commit().await?;

    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// `GET /api/orgs/{org}/repos/{repo}/tracker-bindings`
///
/// A member read, like the repo itself: this describes a repo the caller can
/// already see, and hiding it from the people whose agents work in that repo
/// would only make "why did my labelled issue do nothing?" harder to answer.
pub async fn list_repo_bindings(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug)): Path<(String, String)>,
) -> ApiResult<Json<Vec<TrackerBindingView>>> {
    ctx.require_scope(of_core::scopes::TRACKERS)?;
    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    let repo = resolve_repo(&mut tx, &scope, slug).await?;
    let bindings = list_bindings_for_repo(&mut tx, repo.id).await?;
    tx.commit().await?;

    Ok(Json(bindings.into_iter().map(Into::into).collect()))
}

/// `PUT /api/orgs/{org}/repos/{repo}/tracker-bindings/{provider}`
pub async fn bind_repo(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug, provider)): Path<(String, String, String)>,
    Json(req): Json<BindRepoRequest>,
) -> ApiResult<Json<TrackerBindingView>> {
    ctx.require_admin()?;
    ctx.require_scope(of_core::scopes::TRACKERS)?;
    let provider = provider_from_path(&provider)?;
    validate_external_ref(provider, &req.external_ref)?;

    let trigger_label = req
        .trigger_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(DEFAULT_TRIGGER_LABEL)
        .to_string();

    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    let repo = resolve_repo(&mut tx, &scope, slug).await?;

    // The connection is looked up, never taken from the request. A binding
    // pointing at another provider's connection is not a shape any caller
    // should be able to construct, and `None` here is the legitimate
    // "bound before the tracker was connected" case rather than an error.
    let connection_id = get_connection(&mut tx, provider).await?.map(|c| c.id);

    let binding = upsert_binding(
        &mut tx,
        repo.id,
        connection_id,
        provider,
        &req.external_ref,
        &trigger_label,
    )
    .await?;

    tx.audit(
        Entry::new(action::TRACKER_BOUND)
            .actor(ctx.user.id)
            .target("repo", repo.slug.clone())
            .detail(serde_json::json!({
                "provider": provider.to_string(),
                "externalRef": req.external_ref,
                "triggerLabel": trigger_label,
            })),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(binding.into()))
}

/// `DELETE /api/orgs/{org}/repos/{repo}/tracker-bindings/{provider}`
pub async fn unbind_repo(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug, provider)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    ctx.require_admin()?;
    ctx.require_scope(of_core::scopes::TRACKERS)?;
    let provider = provider_from_path(&provider)?;

    let scope = ctx.team_scope(&state.platform).await?;
    let mut tx = ctx.begin(&state.db).await?;
    let repo = resolve_repo(&mut tx, &scope, slug).await?;
    if let Some(binding) = resolve_binding(&mut tx, repo.id, provider).await? {
        delete_binding(&mut tx, binding.id).await?;
        tx.audit(
            Entry::new(action::TRACKER_UNBOUND)
                .actor(ctx.user.id)
                .target("repo", repo.slug.clone())
                .detail(serde_json::json!({ "provider": provider.to_string() })),
        )
        .await?;
    }
    tx.commit().await?;

    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// Resolve a repo slug under the same team-visibility rule `routes::repos`
/// applies, so a team-scoped repo's tracker binding is not readable by members
/// of other teams — the binding names the customer's JIRA project.
async fn resolve_repo(
    tx: &mut otto_tenant::Tx<'_>,
    scope: &of_core::teams::TeamScope,
    slug: String,
) -> ApiResult<of_core::repos::Repo> {
    super::repos::resolve_visible(tx, scope, slug).await
}

/// Refuse a binding that could never match an inbound event.
///
/// Webhook ingest matches a GitHub event on `repository.full_name` and a JIRA
/// event on `fields.project.key`. A binding in any other shape is not wrong
/// later — it is silently inert forever, and the symptom is a labelled issue
/// that appears to do nothing.
fn validate_external_ref(provider: Provider, external_ref: &str) -> Result<(), ApiError> {
    match provider {
        Provider::Github => {
            let mut parts = external_ref.split('/');
            let valid = matches!(
                (parts.next(), parts.next(), parts.next()),
                (Some(owner), Some(repo), None)
                    if !owner.is_empty()
                        && !repo.is_empty()
                        && !owner.contains(char::is_whitespace)
                        && !repo.contains(char::is_whitespace)
            );
            if valid {
                Ok(())
            } else {
                Err(ApiError::bad_request(format!(
                    "a GitHub tracker binding is the repository's owner and name, as \
                     \"owner/repo\" — GitHub sends that exact string as repository.full_name on \
                     every webhook, and a binding in any other shape never matches one. Got {external_ref:?}."
                )))
            }
        }
        Provider::Jira => {
            // Asked of the existing grammar rather than restated here.
            // `validate_jira_issue_key` is what every outbound JIRA call
            // already checks a ticket against, so a project key it would reject
            // is one whose tickets could be bound and then never synced. A
            // second copy of the rule is a second thing to drift.
            let valid =
                of_trackers::jira::validate_jira_issue_key(&format!("{external_ref}-1")).is_ok();
            if valid {
                Ok(())
            } else {
                Err(ApiError::bad_request(format!(
                    "a JIRA tracker binding is a project key such as \"ACME\" — upper-case \
                     letters and digits, starting with a letter. JIRA sends it as \
                     fields.project.key on every webhook, and a binding in any other shape \
                     never matches one. Got {external_ref:?}."
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_refs_are_owner_slash_repo_and_jira_refs_are_project_keys() {
        for good in ["acme/api", "acme-inc/api.rs", "a/b"] {
            validate_external_ref(Provider::Github, good).expect(good);
        }
        for bad in ["acme", "acme/api/extra", "acme/", "/api", "acme /api", ""] {
            validate_external_ref(Provider::Github, bad).expect_err(bad);
        }
        for good in ["ACME", "DF", "A1"] {
            validate_external_ref(Provider::Jira, good).expect(good);
        }
        // `PROJ_X` is the one worth naming: it looks like a plausible key and
        // is not one, so a binding to it would be stored and its tickets would
        // then be refused by the same grammar every outbound call applies.
        for bad in [
            "acme-123",
            "not a key",
            "1ACME",
            "ACME-1",
            "PROJ_X",
            "acme",
            "",
        ] {
            validate_external_ref(Provider::Jira, bad).expect_err(bad);
        }
    }

    /// The reason the view type exists. If somebody ever swaps it back for the
    /// domain row, this is what says no.
    #[test]
    fn the_connection_view_carries_no_ciphertext() {
        let view = TrackerConnectionView {
            id: uuid::Uuid::nil(),
            provider: Provider::Jira,
            external_id: "site-1".into(),
            has_credentials: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&view).expect("serialize");
        assert!(!json.contains("encrypted"), "{json}");
        assert!(json.contains("hasCredentials"), "{json}");
    }
}
