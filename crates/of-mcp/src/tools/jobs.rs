//! Job tools — the queue itself.
//!
//! The lifecycle is `pending → in-progress → completed | failed`, with an
//! optional `active` refinement of `in-progress` an agent can set once it has
//! actually started (see `activate_job`) and `repend_job` returning a
//! terminal job to `pending` for one more dispatch. The one rule an agent has
//! to internalize is that **claiming is what makes work yours**: `ready`
//! shows what is claimable, `claim_jobs` takes it atomically, and starting
//! work you have not claimed is how two agents end up writing the same file.

use of_core::audit::action;
use of_core::jobs::JobsExt;
use of_core::jobs::{Job, JobFilter, NewJob, Status, Tracker};
use of_core::ticket_ref::{JobRef, TicketQuery};
use of_core::trackers::{
    decode_stored_secret, get_connection, resolve_binding, upsert_connection, Provider,
    TrackerBinding, TrackerConnection,
};
use of_trackers::github::GithubAppClient;
use of_trackers::jira::JiraClient;
use of_trackers::sync::{
    blocked_by, normalize_remote_revision, outbound_decision, parse_remote_revision,
    select_jira_transition, JobTransition,
};
use otto_tenant::audit::Entry;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::{
    ensure_job_visible, ensure_jobs_visible, job_ref, job_refs, maybe_repo_of, out,
    redact_foreign_ids, repo_for_write, retain_visible, scope, visible_ids, visible_job,
};
use crate::server::{Factory, McpResult};
use of_core::repos::Hold;

/// `list_jobs`'s `ticket` filter. Blank is absent, as for the repo filters.
/// A job id is refused rather than ignored: a filter that silently matched
/// every job would answer a question the caller did not ask.
fn ticket_filter(raw: Option<&str>) -> of_core::Result<Option<TicketQuery>> {
    let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else {
        return Ok(None);
    };
    match JobRef::parse(raw)? {
        JobRef::Ticket(t) => Ok(Some(t)),
        JobRef::Id(_) => Err(of_core::Error::Invalid(
            "ticket takes an issue reference like \"#928\" or \"owner/repo#928\"; to fetch a \
             job by its id, call get_job"
                .into(),
        )),
    }
}

fn provider_of(tracker: Tracker) -> Provider {
    match tracker {
        Tracker::Github => Provider::Github,
        Tracker::Jira => Provider::Jira,
    }
}

/// Turn a transient tracker-sync failure (a `String` error from the shared
/// binding-resolution step or an outbound `sync_github_job`/`sync_jira_job`
/// call) into the tool's own error, distinct from the fixed, non-retriable
/// `Error::Invalid` messages `sync_ticket` returns for a configuration
/// problem it can already name precisely (`NotConfigured`/`Broken`). Every
/// site this wraps is something a later retry can plausibly fix — a
/// database hiccup resolving the binding, a tracker outage, a rate limit —
/// which is what `retriable: true` promises the caller.
fn tracker_sync_error(message: impl Into<String>, retriable: bool) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode::INTERNAL_ERROR,
        message.into(),
        Some(serde_json::json!({
            "code": "tracker_sync_failed",
            "retriable": retriable,
        })),
    )
}

fn parse_github_ticket_ref(ticket_ref: &str) -> Option<(&str, &str, i64)> {
    let (owner_repo, issue) = ticket_ref.split_once('#')?;
    let (owner, repo) = owner_repo.split_once('/')?;
    let issue_number = issue.parse().ok()?;
    Some((owner, repo, issue_number))
}

struct JiraSyncOutcome {
    remote_revision: Option<String>,
    rotated_credentials: Option<otto_tenant::crypto::Sealed>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddJobArgs {
    /// One line saying what needs doing. This is what other agents and humans
    /// see in listings, so make it specific.
    pub title: String,
    /// Everything the agent that picks this up will need. It may have none of
    /// your context.
    #[serde(default)]
    pub description: Option<String>,
    /// Registered repo slug this job belongs to.
    #[serde(default)]
    pub repo: Option<String>,
    /// Or the git remote URL identifying it, from `git remote get-url origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Issue or ticket this job corresponds to, as a free-form reference such
    /// as "PROJ-123" or "acme/api#42". Recorded, not resolved.
    #[serde(default)]
    pub ticket_ref: Option<String>,
    /// Free-form hint about which kind of agent should take this. Never
    /// enforced — any agent may claim any job.
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Arbitrary JSON this server stores and never interprets. Your own
    /// workflow's fields go here.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Jobs that must be completed before this one can be claimed. Each is a job
    /// id like "job-42", or the job's issue as "#928" or "owner/repo#928".
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// A caller-chosen key. Replaying add_job with the same key and the same
    /// arguments returns the original job unchanged instead of creating a
    /// second one — call this every time if your connection to the server
    /// can drop between the call committing and its response arriving,
    /// which is the situation a retry cannot otherwise tell apart from
    /// "never happened". Reusing a key with different arguments is an
    /// error. Omit it and every call creates a new job, as today.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobArgs {
    /// A job id like "job-42", or the job's issue as "#928" or "owner/repo#928".
    pub job: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListJobsArgs {
    /// Only jobs in this state: "pending", "in-progress", "active",
    /// "completed", "failed" or "cancelled". Omit for all of them.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    /// Only jobs you queued yourself. Defaults to false.
    #[serde(default)]
    pub mine: bool,
    /// A routing hint, not access control: restrict to jobs with this exact
    /// `agent_type` plus jobs with none at all (unrouted work anyone may
    /// take). Omit to see every job regardless of `agent_type`.
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Only the jobs linked to this issue, as "#928" (that number in any
    /// repo) or "owner/repo#928". The way to see every job an ambiguous issue
    /// reference matched. A job id here is invalid_argument; use get_job.
    /// Blank is the same as omitting it.
    #[serde(default)]
    pub ticket: Option<String>,
    /// Maximum rows. Defaults to the server's own limit.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoScopeArgs {
    /// Narrow to one repo by slug. Omit to cover the whole organization.
    #[serde(default)]
    pub repo: Option<String>,
    /// Or narrow by git remote URL.
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadyArgs {
    /// Narrow to one repo by slug. Omit to cover the whole organization.
    #[serde(default)]
    pub repo: Option<String>,
    /// Or narrow by git remote URL.
    #[serde(default)]
    pub remote: Option<String>,
    /// A routing hint for a mixed fleet, not access control: restrict to jobs
    /// with this exact `agentType` plus jobs with none set at all (unrouted
    /// work anyone may take). Omit to see every ready job.
    #[serde(default)]
    pub agent_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateJobArgs {
    /// The job to change. A job id like "job-42", or the job's issue as "#928" or "owner/repo#928".
    pub job: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Replaces the whole metadata object; it is not merged.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimJobsArgs {
    /// The jobs to take. All or none succeed. Each is a job id like "job-42", or the job's issue as
    /// "#928" or "owner/repo#928".
    pub jobs: Vec<String>,
    /// How you want to be identified to teammates looking at the queue, for
    /// example "api-agent@ci-7". At most 128 characters on one line, with no
    /// control, line-break, or invisible formatting characters; anything else
    /// is refused with invalid_agent_label rather than shortened. Blank is the
    /// same as omitting it.
    #[serde(default)]
    pub agent: Option<String>,
    /// Seconds before this claim expires if never renewed. Defaults to a
    /// server-chosen TTL (900s) if omitted, clamped to between 60 seconds and
    /// 4 hours. Extend it with renew_claim while you keep working — an
    /// unrenewed claim expires and the job becomes claimable by someone else.
    #[serde(default)]
    pub ttl: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteJobArgs {
    /// The job you finished. A job id like "job-42", or the job's issue as "#928" or
    /// "owner/repo#928".
    pub job: String,
    /// What you did, for whoever reads this later.
    #[serde(default)]
    pub result: Option<String>,
    /// Optional: the `attempts` value from your own claim_jobs response when
    /// you claimed this job, or from your own last successful renew_claim
    /// response — never a value read from get_job, which shows the job's
    /// current state for orientation only and may belong to whoever holds
    /// the claim now, not to you. A generation number, not a retry count. If
    /// a different process under your own account has since reclaimed this
    /// job after your claim lapsed, `attempts` has moved on; supplying the
    /// value you actually hold makes this call fail (naming the current
    /// holder) instead of silently overwriting their in-flight work. Omit
    /// it to keep matching by account alone, as before.
    #[serde(default)]
    pub expected_attempts: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailJobArgs {
    /// The job you could not finish. A job id like "job-42", or the job's issue as "#928" or
    /// "owner/repo#928".
    pub job: String,
    /// Why it failed, specifically enough that the next attempt can do better.
    #[serde(default)]
    pub error: Option<String>,
    /// Optional: the `attempts` value from your own claim_jobs response when
    /// you claimed this job, or from your own last successful renew_claim
    /// response — never a value read from get_job, which shows the job's
    /// current state for orientation only and may belong to whoever holds
    /// the claim now, not to you. A generation number, not a retry count. If
    /// a different process under your own account has since reclaimed this
    /// job after your claim lapsed, `attempts` has moved on; supplying the
    /// value you actually hold makes this call fail (naming the current
    /// holder) instead of silently overwriting their in-flight work. Omit
    /// it to keep matching by account alone, as before.
    #[serde(default)]
    pub expected_attempts: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewClaimArgs {
    /// The job you are still working on. A job id like "job-42", or the job's issue as "#928" or
    /// "owner/repo#928".
    pub job: String,
    /// Seconds from now until the claim expires — not added to whatever
    /// time was left on it. Same default (900s) and clamp range (60 seconds
    /// to 4 hours) as claim_jobs's ttl if omitted.
    #[serde(default)]
    pub ttl: Option<i64>,
    /// Optional: the `attempts` value from your own claim_jobs response when
    /// you claimed this job, or from your own last successful renew_claim
    /// response — never a value read from get_job, which shows the job's
    /// current state for orientation only and may belong to whoever holds
    /// the claim now, not to you. A generation number, not a retry count. If
    /// a different process under your own account has since reclaimed this
    /// job after your claim lapsed, `attempts` has moved on; supplying the
    /// value you actually hold makes this call fail (naming the current
    /// holder) instead of silently renewing an expiry that belongs to their
    /// claim, not yours. Omit it to keep matching by account alone, as
    /// before.
    #[serde(default)]
    pub expected_attempts: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestCancelArgs {
    /// The job to ask to stop. A job id like "job-42", or the job's issue as "#928" or
    /// "owner/repo#928".
    pub job: String,
    /// Why, for whoever is holding it and for the audit trail.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelJobArgs {
    /// The job you were working on and are stopping. A job id like "job-42", or the job's issue as
    /// "#928" or "owner/repo#928".
    pub job: String,
    /// What you were doing when you stopped, for whoever reads this later.
    #[serde(default)]
    pub note: Option<String>,
    /// Optional: the `attempts` value from your own claim_jobs response when
    /// you claimed this job, or from your own last successful renew_claim
    /// response — never a value read from get_job, which shows the job's
    /// current state for orientation only and may belong to whoever holds
    /// the claim now, not to you. A generation number, not a retry count. If
    /// a different process under your own account has since reclaimed this
    /// job after your claim lapsed, `attempts` has moved on; supplying the
    /// value you actually hold makes this call fail (naming the current
    /// holder) instead of silently cancelling their in-flight work. Omit it
    /// to keep matching by account alone, as before.
    #[serde(default)]
    pub expected_attempts: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetDependenciesArgs {
    /// The job whose dependencies are changing. A job id like "job-42", or the job's issue as
    /// "#928" or "owner/repo#928".
    pub job: String,
    /// Jobs this job should wait for. Each is a job id like "job-42", or the
    /// job's issue as "#928" or "owner/repo#928".
    #[serde(default)]
    pub add: Vec<String>,
    /// Jobs it should stop waiting for. Each is a job id like "job-42", or the
    /// job's issue as "#928" or "owner/repo#928".
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkTicketArgs {
    /// The job to attach a tracker ticket to. A job id like "job-42", or the job's issue as "#928"
    /// or "owner/repo#928".
    pub job: String,
    /// Which tracker the ticket lives in: "jira" or "github".
    pub tracker: Tracker,
    /// The ticket's reference in that tracker, e.g. "PROJ-123" or
    /// "acme/api#42". Recorded, not resolved.
    pub ticket_ref: String,
}

/// The outcome of looking up whether a job's repo actually has somewhere to
/// sync to. Kept three-way rather than collapsing to `Option` so a broken
/// invariant (a binding whose `connection_id` points at no
/// `tracker_connections` row) never gets reported the same way as an
/// ordinary "nothing configured yet" gap — the two need different messages
/// wherever a caller can see them (`sync_ticket`), and the same distinction
/// is worth preserving even where nothing is currently surfaced
/// (`sync_job_after_transition`, which maps both to a silent no-op).
enum BindingLookup {
    /// No `tracker_bindings` row for this repo/provider, or one exists but
    /// `connection_id IS NULL` (configured, not yet activated). Both mean the
    /// same thing to a caller: nothing is wired up yet.
    NotConfigured,
    /// The binding's `connection_id` names a connection that no longer
    /// exists — a data-integrity problem, not a configuration gap. Already
    /// logged as the invariant violation it is by the time this is returned.
    Broken,
    Ready(Box<TrackerBinding>, TrackerConnection),
}

#[tool_router(router = jobs_router, vis = "pub(crate)")]
impl Factory {
    /// Resolve the connection a job's repo should sync outbound writes
    /// through, in its own short transaction (no HTTP call is ever made while
    /// holding one open). Shared by the fire-and-forget post-transition sync
    /// and `sync_ticket`, which react to the three-way result differently —
    /// see [`BindingLookup`].
    async fn resolve_tracker_binding(
        &self,
        org_id: otto_tenant::ids::OrgId,
        repo_id: of_core::ids::RepoId,
        provider: Provider,
    ) -> Result<BindingLookup, String> {
        let mut tx = of_core::platform_events::begin_live(self.db(), org_id, None)
            .await
            .map_err(|error| error.to_string())?;
        let binding = resolve_binding(&mut tx, repo_id, provider)
            .await
            .map_err(|error| error.to_string())?;
        let Some(binding) = binding else {
            tx.commit().await.map_err(|error| error.to_string())?;
            return Ok(BindingLookup::NotConfigured);
        };
        let Some(connection_id) = binding.connection_id else {
            tx.commit().await.map_err(|error| error.to_string())?;
            return Ok(BindingLookup::NotConfigured);
        };
        let connection = get_connection(&mut tx, provider)
            .await
            .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())?;
        // `get_connection` looks up by (org, provider), not by id.
        // `tracker_bindings.connection_id` has `ON DELETE SET NULL` against
        // `tracker_connections`, and `tracker_connections` carries
        // `UNIQUE (org_id, provider)` — together those mean a non-null
        // `connection_id` is, today, always the id of the one connection row
        // `get_connection` would return for this provider; there is no
        // current write path that leaves it pointing at a since-replaced
        // row. Comparing ids explicitly costs nothing and keeps that true
        // even if a future change (a differently-behaved delete path, a
        // relaxed constraint) would otherwise let this resolve silently
        // through the wrong connection instead of surfacing `Broken`.
        let connection = match connection {
            Some(connection) if connection.id == connection_id => connection,
            _ => {
                // Reachable today only if the invariant above stops holding.
                // Log it as the invariant violation it would be instead of
                // letting outbound sync silently vanish with no operator
                // signal, matching how the inbound webhook route treats the
                // same broken-invariant shape.
                tracing::error!(
                    repo_id = %repo_id,
                    provider = %provider,
                    "tracker binding names a connection id that no longer matches this org's \
                     tracker_connections row for this provider"
                );
                return Ok(BindingLookup::Broken);
            }
        };
        Ok(BindingLookup::Ready(Box::new(binding), connection))
    }

    async fn sync_jobs_after_transition(
        &self,
        jobs: &[Job],
        transition: JobTransition,
        detail: Option<&str>,
    ) {
        for job in jobs {
            if let Err(error) = self
                .sync_job_after_transition(job, transition, detail)
                .await
            {
                tracing::error!(job_id = %job.id, error = %error, "tracker write-back failed");
            }
        }
    }

    async fn sync_job_after_transition(
        &self,
        job: &Job,
        transition: JobTransition,
        detail: Option<&str>,
    ) -> Result<(), String> {
        let Some(tracker) = job.tracker else {
            return Ok(());
        };
        let Some(ticket_ref) = job.ticket_ref.as_deref() else {
            return Ok(());
        };

        let provider = provider_of(tracker);
        let (binding, connection) = match self
            .resolve_tracker_binding(job.org_id, job.repo_id, provider)
            .await?
        {
            // Nothing to sync to yet, or a broken binding an operator needs
            // to fix — neither is a failure of the queue transition that
            // just happened, so both are a silent no-op here exactly as
            // before this helper was extracted.
            BindingLookup::NotConfigured | BindingLookup::Broken => return Ok(()),
            BindingLookup::Ready(binding, connection) => (binding, connection),
        };

        let plan = outbound_decision(transition, tracker, detail);
        match tracker {
            Tracker::Github => {
                let outcome = self
                    .sync_github_job(job, ticket_ref, &connection.external_id, &plan)
                    .await?;
                if let Some(remote_revision) = outcome {
                    let mut tx = of_core::platform_events::begin_live(self.db(), job.org_id, None)
                        .await
                        .map_err(|error| error.to_string())?;
                    tx.set_remote_revision(&job.id, &remote_revision)
                        .await
                        .map_err(|error| error.to_string())?;
                    tx.commit().await.map_err(|error| error.to_string())?;
                }
            }
            Tracker::Jira => {
                let outcome = self
                    .sync_jira_job(job, ticket_ref, &binding.external_ref, &connection, &plan)
                    .await?;
                if outcome.remote_revision.is_some() || outcome.rotated_credentials.is_some() {
                    let mut tx = of_core::platform_events::begin_live(self.db(), job.org_id, None)
                        .await
                        .map_err(|error| error.to_string())?;
                    if let Some(remote_revision) = outcome.remote_revision.as_deref() {
                        tx.set_remote_revision(&job.id, remote_revision)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    if let Some(rotated) = outcome.rotated_credentials.as_ref() {
                        let webhook_secret = connection
                            .encrypted_webhook_secret
                            .as_deref()
                            .map(decode_stored_secret)
                            .transpose()
                            .map_err(|error| error.to_string())?;
                        upsert_connection(
                            &mut tx,
                            Provider::Jira,
                            &connection.external_id,
                            Some(rotated),
                            webhook_secret.as_ref(),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    }
                    tx.commit().await.map_err(|error| error.to_string())?;
                }
            }
        }

        Ok(())
    }

    /// A GitHub App client, pointed at `TrackerSyncConfig::github_api_base`
    /// when one is set (a test's mock GitHub).
    fn github_client(&self, app_id: i64, private_key: String) -> Result<GithubAppClient, String> {
        let client =
            GithubAppClient::new(app_id, private_key).map_err(|error| error.to_string())?;
        Ok(match self.tracker_sync().github_api_base.clone() {
            Some(base) => client.with_api_base(base),
            None => client,
        })
    }

    /// The App's credentials, or why there are none.
    fn github_app(&self) -> Result<(i64, String), String> {
        let app_id = self
            .tracker_sync()
            .github_app_id
            .ok_or_else(|| "GitHub tracker sync is not configured (missing App id)".to_string())?;
        let private_key = self
            .tracker_sync()
            .github_app_private_key
            .clone()
            .ok_or_else(|| {
                "GitHub tracker sync is not configured (missing App private key)".to_string()
            })?;
        Ok((app_id, private_key))
    }

    /// `sync_ticket`'s read of a GitHub job's issue: its body and revision.
    async fn read_github_issue(
        &self,
        job: &Job,
        external_id: &str,
        owner: &str,
        repo: &str,
        issue_number: i64,
    ) -> Result<of_trackers::github::IssueView, String> {
        let (app_id, private_key) = self.github_app()?;
        let installation_id = external_id.parse::<i64>().map_err(|error| {
            format!(
                "tracker connection for job {} has a non-numeric GitHub installation id {external_id:?}: {error}",
                job.id
            )
        })?;
        self.github_client(app_id, private_key)?
            .get_issue(installation_id, owner, repo, issue_number)
            .await
            .map_err(|error| error.to_string())
    }

    async fn sync_github_job(
        &self,
        job: &Job,
        ticket_ref: &str,
        external_id: &str,
        plan: &of_trackers::sync::OutboundDecision,
    ) -> Result<Option<String>, String> {
        let (app_id, private_key) = self.github_app()?;
        let installation_id = external_id.parse::<i64>().map_err(|error| {
            format!(
                "tracker connection for job {} has a non-numeric GitHub installation id {external_id:?}: {error}",
                job.id
            )
        })?;
        let (owner, repo, issue_number) = parse_github_ticket_ref(ticket_ref).ok_or_else(|| {
            format!(
                "job {} has an invalid GitHub ticket_ref {ticket_ref:?}",
                job.id
            )
        })?;
        let client = self.github_client(app_id, private_key)?;

        client
            .post_comment(installation_id, owner, repo, issue_number, &plan.comment)
            .await
            .map_err(|error| error.to_string())?;
        let remote_revision = if let Some(close) = plan.github_close.as_ref() {
            match client
                .set_issue_state(
                    installation_id,
                    owner,
                    repo,
                    issue_number,
                    "closed",
                    Some(&close.state_reason),
                )
                .await
            {
                Ok(updated_at) => updated_at,
                Err(error) => return Err(error.to_string()),
            }
        } else {
            client
                .get_issue_updated_at(installation_id, owner, repo, issue_number)
                .await
                .map_err(|error| error.to_string())?
        };

        Ok(normalize_remote_revision(remote_revision.as_deref()))
    }

    async fn sync_jira_job(
        &self,
        job: &Job,
        ticket_ref: &str,
        binding_external_ref: &str,
        connection: &of_core::trackers::TrackerConnection,
        plan: &of_trackers::sync::OutboundDecision,
    ) -> Result<JiraSyncOutcome, String> {
        let client_id =
            self.tracker_sync().jira_client_id.clone().ok_or_else(|| {
                "JIRA tracker sync is not configured (missing client id)".to_string()
            })?;
        let client_secret = self
            .tracker_sync()
            .jira_client_secret
            .clone()
            .ok_or_else(|| {
                "JIRA tracker sync is not configured (missing client secret)".to_string()
            })?;
        let encryption_key = self
            .tracker_sync()
            .encryption_key
            .as_deref()
            .ok_or_else(|| {
                "JIRA tracker sync is not configured (missing encryption key)".to_string()
            })?;
        let cipher = otto_tenant::crypto::Cipher::from_base64_key(encryption_key)
            .map_err(|error| error.to_string())?;
        let encoded = connection.encrypted_credentials.as_deref().ok_or_else(|| {
            format!(
                "JIRA connection for job {} is missing stored credentials",
                job.id
            )
        })?;
        let sealed = decode_stored_secret(encoded).map_err(|error| error.to_string())?;
        let refresh_token =
            JiraClient::open_refresh_token(&cipher, &sealed).map_err(|error| error.to_string())?;
        let client =
            JiraClient::new(client_id, client_secret).map_err(|error| error.to_string())?;
        let tokens = client
            .refresh_access_token(&refresh_token)
            .await
            .map_err(|error| error.to_string())?;

        client
            .post_comment(
                &tokens.access_token,
                &connection.external_id,
                ticket_ref,
                &plan.comment,
            )
            .await
            .map_err(|error| error.to_string())?;

        if let Some(target) = plan.jira_transition.as_ref() {
            let transitions = client
                .list_transitions(&tokens.access_token, &connection.external_id, ticket_ref)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(transition) = select_jira_transition(target, &transitions) {
                client
                    .transition_issue(
                        &tokens.access_token,
                        &connection.external_id,
                        ticket_ref,
                        &transition.id,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            } else {
                tracing::warn!(
                    job_id = %job.id,
                    provider = "jira",
                    external_ref = %binding_external_ref,
                    ticket_ref = %ticket_ref,
                    "no reachable JIRA transition matched the requested target; comment posted only"
                );
            }
        }

        let remote_revision = client
            .get_issue_updated_at(&tokens.access_token, &connection.external_id, ticket_ref)
            .await
            .map_err(|error| error.to_string())?;
        let rotated_credentials = if tokens.refresh_token != refresh_token {
            Some(
                tokens
                    .seal_refresh_token(&cipher)
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };

        Ok(JiraSyncOutcome {
            remote_revision: normalize_remote_revision(remote_revision.as_deref()),
            rotated_credentials,
        })
    }

    #[tool(
        name = "add_job",
        description = "Queue a new job against a repository. Give it a specific title and \
                       enough description that an agent with none of your context can pick it \
                       up. Use dependsOn for work that must wait for other jobs. Returns the \
                       created job, including its id. Pass idempotencyKey if your connection \
                       can drop before you see the response, so a retry returns the original \
                       job instead of creating a duplicate."
    )]
    pub async fn add_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<AddJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;

        // No key: reproduce today's behavior exactly, including charging as
        // the literal first thing after the transaction opens (see
        // Factory::charge's doc comment) — there is no replay question to
        // answer first, so nothing should run ahead of it.
        let Some(key) = args.idempotency_key else {
            self.charge(&mut tx, &caller, "add_job").await?;
            let repo = repo_for_write(&mut tx, &team, args.repo, args.remote, Hold::Shared).await?;
            let depends_on = job_refs(&mut tx, &team, &args.depends_on).await?;
            ensure_jobs_visible(&mut tx, &team, &depends_on).await?;
            let job = tx
                .add_job(NewJob {
                    repo_id: repo.id,
                    title: args.title,
                    description: args.description,
                    ticket_ref: args.ticket_ref,
                    agent_type: args.agent_type,
                    metadata: args.metadata,
                    depends_on,
                    created_by: Some(caller.user_id),
                    ..Default::default()
                })
                .await
                .mcp()?;
            tx.commit().await.mcp()?;
            return Ok(Json(out::JobOut { job }));
        };

        // A key was supplied: replay-vs-new must be resolved before
        // charging (a replay must never be billed, and there is no way to
        // un-charge after the fact — see Factory::charge's third exception),
        // which needs `repo.id` to build the payload to check.
        let repo = repo_for_write(&mut tx, &team, args.repo, args.remote, Hold::Shared).await?;
        // Resolved before the fingerprint is taken, so a replay that spells a
        // dependency as its issue once and as its id the next still replays.
        // A replay is re-resolved on every call: if a second job has since
        // linked the same issue, a retry that spelled the dependency as "#N" is
        // refused as ambiguous_ticket_ref rather than replayed. That is
        // acceptable, because the reference no longer names one job.
        let depends_on = job_refs(&mut tx, &team, &args.depends_on).await?;
        ensure_jobs_visible(&mut tx, &team, &depends_on).await?;
        let new_job = NewJob {
            repo_id: repo.id,
            title: args.title,
            description: args.description,
            ticket_ref: args.ticket_ref,
            agent_type: args.agent_type,
            metadata: args.metadata,
            depends_on,
            created_by: Some(caller.user_id),
            idempotency_key: Some(key),
            ..Default::default()
        };

        if let Some(existing) = tx.find_replayed_job(&new_job).await.mcp()? {
            // The fingerprint covers what the caller sent, not the team the job
            // inherited from its repo then. If the repo has been reassigned
            // since, the repo can be visible while the original job is not.
            // Not `job_not_found`, which would invite a retry with the same
            // key: the original exists, so the key is spent.
            if let Err(e) = tx.get_job_visible(&existing.id, &team).await {
                if !matches!(e, of_core::Error::JobNotFound(_)) {
                    return Err(e).mcp();
                }
                return Err(of_core::Error::Redacted {
                    code: "idempotency_key_conflict",
                    message: "that idempotency key already created a job you can no longer see \
                              (its repo has moved to a team you are not on); use a new key for \
                              a new job",
                })
                .mcp();
            }
            self.record_replay(&mut tx, &caller, "add_job").await?;
            tx.commit().await.mcp()?;
            return Ok(Json(out::JobOut { job: existing }));
        }

        self.charge(&mut tx, &caller, "add_job").await?;
        let job = tx.add_job(new_job).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "get_job",
        description = "Fetch one job, with its full description, metadata, status and who \
                       claimed it. Name it by id (\"job-42\") or by its issue (\"#928\" or \
                       \"owner/repo#928\"), as every tool that takes a job accepts. An issue \
                       more than one job you can see carries is refused with \
                       ambiguous_ticket_ref, listing their ids; one no job carries is \
                       job_not_found."
    )]
    pub async fn get_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "get_job").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        let job = visible_job(&mut tx, &team, &id).await?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "list_jobs",
        description = "List jobs, optionally filtered by status, repository, whether you queued \
                       them, agentType, or ticket. ticket is an issue (\"#928\" or \
                       \"owner/repo#928\") and returns every job linked to it, which is what to \
                       call when an issue reference was refused as ambiguous_ticket_ref. \
                       agentType is a routing hint for a mixed fleet, not access control: \
                       passing it also returns jobs with no agentType set, since unrouted \
                       work is anyone's to take. To find work you can actually take, \
                       prefer `ready` — this returns jobs regardless of whether their \
                       dependencies are satisfied."
    )]
    pub async fn list_jobs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListJobsArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        // Parse before opening a transaction: a bad status is the caller's
        // mistake and does not need a database round trip to detect.
        let status = args
            .status
            .as_deref()
            .map(str::parse::<Status>)
            .transpose()
            .mcp()?;
        let ticket = ticket_filter(args.ticket.as_deref()).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "list_jobs").await?;
        let repo_id = maybe_repo_of(&mut tx, &team, args.repo, args.remote).await?;
        let jobs = tx
            .list_jobs(&JobFilter {
                status,
                repo_id,
                visible_teams: team.restriction(),
                created_by: args.mine.then_some(caller.user_id),
                agent_type: args.agent_type,
                ticket,
                limit: args.limit,
                ..Default::default()
            })
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "update_job",
        description = "Change a job's title, description, agent type hint, or metadata. Only \
                       the fields you pass change, except metadata, which is replaced whole. \
                       Does not change status — use claim_jobs, complete_job, fail_job or \
                       repend_job for that."
    )]
    pub async fn update_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "update_job").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx
            .update_job(
                &id,
                args.title.as_deref(),
                args.description.as_deref(),
                args.agent_type.as_deref(),
                args.metadata.as_ref(),
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "delete_job",
        description = "Remove a job from the queue permanently. Prefer fail_job for work that \
                       was attempted and did not succeed: deleting loses the history of why."
    )]
    pub async fn delete_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::DeletedOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "delete_job").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        // Deleting cascades to whatever depends on this job. If that includes
        // work the caller cannot see, it is not theirs to change.
        if tx.has_hidden_dependents(&id, &team).await.mcp()? {
            return Err(of_core::Error::Invalid(
                "this job cannot be deleted by you; ask an organization admin".into(),
            ))
            .mcp();
        }
        tx.delete_job(&id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::DeletedOut { deleted: id }))
    }

    #[tool(
        name = "claim_jobs",
        description = "Atomically take one or more pending jobs, moving them to in-progress \
                       under your name. All of them succeed or none do, so a partial claim can \
                       never leave you believing you own work you do not. Fails if any job is \
                       already claimed or still blocked by an unfinished dependency. Claim \
                       before you start working. Claims expire (900s by default, or your ttl); \
                       renew_claim pushes a claim you hold forward. Once a claim has already \
                       expired, though, it cannot be renewed or finalized by its original \
                       holder any more — complete_job/fail_job/cancel_job/renew_claim all \
                       refuse it server-side — and claim_jobs, called again on the same job, \
                       is the only way back in. Hold onto each returned job's \
                       `attempts` value yourself if you might later call complete_job, \
                       fail_job, cancel_job, or renew_claim with expectedAttempts to guard \
                       against a same-account process having reclaimed the job out from under \
                       you in the meantime — it is your claim's generation number, and it is \
                       not repeated anywhere else you would otherwise think to look. agent is \
                       a short label (one line, at most 128 visible characters) that teammates \
                       see as the claim's holder; a longer or multi-line one is refused with \
                       invalid_agent_label rather than shortened."
    )]
    pub async fn claim_jobs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ClaimJobsArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "claim_jobs").await?;
        let ids = job_refs(&mut tx, &team, &args.jobs).await?;
        ensure_jobs_visible(&mut tx, &team, &ids).await?;
        let jobs = tx
            .claim_jobs(&ids, caller.user_id, args.agent.as_deref(), args.ttl)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        let out = Json(out::JobsOut { jobs: jobs.clone() });
        // The ticket comment names the label as stored — trimmed, and absent
        // when blank — never the raw argument, the same as `sync_ticket` does.
        // One claim stores one label on every job it takes.
        let label = jobs.first().and_then(|j| j.claimed_by_label.as_deref());
        self.sync_jobs_after_transition(&jobs, JobTransition::Claimed, label)
            .await;

        Ok(out)
    }

    #[tool(
        name = "renew_claim",
        description = "Push a claim you hold forward, the way renew_lease extends a \
                       resource lease. Call this on a cadence comfortably shorter than the claim's TTL \
                       while a long-running job is still in progress: an unrenewed claim \
                       expires and the job becomes claimable by someone else, which is what \
                       lets a crashed agent's abandoned job be picked back up. Fails if you are \
                       not the job's current claim holder — and once a claim has already \
                       expired, this cannot bring it back, even for its original holder: call \
                       claim_jobs again on the same job instead. Renewing comfortably before \
                       expiry, not after, is the only way to keep a claim alive. Pass \
                       expectedAttempts if you also want to guard against renewing a claim \
                       generation a different process under your own account has since \
                       reclaimed — see its own description."
    )]
    pub async fn renew_claim(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RenewClaimArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "renew_claim").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx
            .renew_claim(&id, caller.user_id, args.ttl, args.expected_attempts)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "complete_job",
        description = "Mark a job you claimed as completed, with a summary of what was done. \
                       Anything that depends on it becomes claimable. Fails if you are not the \
                       job's current claim holder — including when your own claim has already \
                       expired and nobody has reclaimed it yet: an expired claim cannot be \
                       finalized any more, even by its original holder, and must be reclaimed \
                       with claim_jobs first. Pass expectedAttempts if you also want to \
                       guard against completing over a claim generation a different process \
                       under your own account has since reclaimed — see its own description."
    )]
    pub async fn complete_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CompleteJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        // The one call that can be refused for billing: look the org's status
        // up now, before a pooled connection is held, so a slow platform costs
        // this call time and nothing else. `charge_completion` judges this
        // answer and makes no lookup of its own.
        let warmed = self.meter().warm(caller.org_id).await;
        let mut tx = self.tx(&caller).await?;
        self.charge_completion(&mut tx, &caller, &warmed).await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx
            .complete_job(
                &id,
                caller.user_id,
                args.result.as_deref(),
                args.expected_attempts,
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        let out = Json(out::JobOut { job: job.clone() });
        self.sync_jobs_after_transition(
            std::slice::from_ref(&job),
            JobTransition::Completed,
            args.result.as_deref(),
        )
        .await;

        Ok(out)
    }

    #[tool(
        name = "fail_job",
        description = "Mark a job you claimed as failed, recording why. Use this rather than \
                       leaving a job in-progress when you cannot finish it — an abandoned claim \
                       blocks everything downstream and tells nobody anything. Fails if you are \
                       not the job's current claim holder — including when your own claim has \
                       already expired and nobody has reclaimed it yet: an expired claim cannot \
                       be finalized any more, even by its original holder, and must be \
                       reclaimed with claim_jobs first. Pass expectedAttempts if you also \
                       want to guard against failing over a claim generation a different \
                       process under your own account has since reclaimed — see its own \
                       description."
    )]
    pub async fn fail_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<FailJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "fail_job").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx
            .fail_job(
                &id,
                caller.user_id,
                args.error.as_deref(),
                args.expected_attempts,
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        let out = Json(out::JobOut { job: job.clone() });
        self.sync_jobs_after_transition(
            std::slice::from_ref(&job),
            JobTransition::Failed,
            args.error.as_deref(),
        )
        .await;

        Ok(out)
    }

    #[tool(
        name = "request_cancel",
        description = "Ask a job to stop. If nobody has claimed it yet, this cancels it \
                       immediately — there is no agent to wait on. If it is claimed \
                       (in-progress or active), this only sets a flag: the server cannot \
                       stop the agent holding it, any more than it can stop a git push. The \
                       holder sees the request the next time it calls get_job or wakes from \
                       watch, and is expected to call cancel_job once it actually stops (or \
                       fail_job, if it disagrees and finishes anyway). Fails if the job is \
                       already completed, failed, or cancelled. Anything that depends on this \
                       job stays blocked until the dependency actually completes — repend_job \
                       only returns this job to pending for another attempt, it does not \
                       complete it, so anything waiting on it is still waiting; use \
                       set_dependencies instead if you need to remove the dependency entirely."
    )]
    pub async fn request_cancel(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RequestCancelArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "request_cancel").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx
            .request_cancel(&id, caller.user_id, args.reason.as_deref())
            .await
            .mcp()?;

        tx.audit(
            Entry::new(action::JOB_CANCEL_REQUESTED)
                .actor(caller.user_id)
                .target("job", job.id.to_string())
                .detail(serde_json::json!({ "reason": args.reason })),
        )
        .await
        .mcp()?;

        // A pending job with nobody to wait on is finalized in the same call —
        // record that outcome too, distinctly, so the audit trail always shows
        // both events regardless of which path a job took.
        let finalized_now = job.status == Status::Cancelled;
        if finalized_now {
            tx.audit(
                Entry::new(action::JOB_CANCELLED)
                    .actor(caller.user_id)
                    .target("job", job.id.to_string())
                    .detail(serde_json::json!({ "reason": args.reason })),
            )
            .await
            .mcp()?;
        }
        tx.commit().await.mcp()?;

        let out = Json(out::JobOut { job: job.clone() });
        if finalized_now {
            let detail = args
                .reason
                .clone()
                .unwrap_or_else(|| "Cancelled before being claimed.".to_string());
            self.sync_jobs_after_transition(
                std::slice::from_ref(&job),
                JobTransition::Cancelled,
                Some(&detail),
            )
            .await;
        }
        Ok(out)
    }

    #[tool(
        name = "cancel_job",
        description = "Finalize a job you were working on as cancelled, because you were \
                       asked to stop via request_cancel and are complying. Fails if this job \
                       never had a cancellation requested — if you are stopping for your own \
                       reasons, call fail_job instead, so the audit trail keeps distinguishing \
                       'asked to stop, and did' from an ordinary failure. Also fails if the \
                       job is not currently in-progress or active — still pending, or already \
                       completed, failed, or cancelled — or if you are not its current claim \
                       holder. Unlike complete_job/fail_job/renew_claim, this one still \
                       succeeds for a claim that has already expired, as long as the \
                       cancellation was requested before it lapsed — that is what lets you \
                       honestly record having complied even if the stop took longer than the \
                       claim's TTL; if nobody asked this job to stop before your claim expired, \
                       it is refused like the others and must be reclaimed with claim_jobs \
                       first. Pass expectedAttempts if you also want to guard against \
                       cancelling a claim generation a different process under your own \
                       account has since reclaimed — see its own description."
    )]
    pub async fn cancel_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CancelJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "cancel_job").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx
            .cancel_job(
                &id,
                caller.user_id,
                args.note.as_deref(),
                args.expected_attempts,
            )
            .await
            .mcp()?;
        tx.audit(
            Entry::new(action::JOB_CANCELLED)
                .actor(caller.user_id)
                .target("job", job.id.to_string())
                .detail(serde_json::json!({ "note": args.note })),
        )
        .await
        .mcp()?;
        tx.commit().await.mcp()?;

        let out = Json(out::JobOut { job: job.clone() });
        let detail = args
            .note
            .clone()
            .or_else(|| job.cancel_reason.clone())
            .unwrap_or_else(|| "Cancelled.".to_string());
        self.sync_jobs_after_transition(
            std::slice::from_ref(&job),
            JobTransition::Cancelled,
            Some(&detail),
        )
        .await;
        Ok(out)
    }

    #[tool(
        name = "activate_job",
        description = "Confirm a job you claimed is being actively worked on right now, not \
                       just claimed. This is what lets a console or API client tell a stalled \
                       claim apart from real progress — call it once you actually start, not \
                       at claim time. Optional: complete_job and fail_job both accept a job \
                       that never called this."
    )]
    pub async fn activate_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "activate_job").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx.activate_job(&id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "repend_job",
        description = "Return a completed, failed, cancelled, in-progress, or active job to \
                       pending so it can be claimed again. The attempt count is preserved, so \
                       repeated failures stay visible."
    )]
    pub async fn repend_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "repend_job").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx.repend_job(&id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "set_dependencies",
        description = "Add or remove the jobs one job waits for. A job with unfinished \
                       dependencies cannot be claimed. A change that would make a job depend \
                       on itself, directly or through a chain, is rejected. Returns the \
                       resulting dependency list."
    )]
    pub async fn set_dependencies(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SetDependenciesArgs>,
    ) -> Result<Json<out::DependenciesOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "set_dependencies").await?;
        let job = job_ref(&mut tx, &team, &args.job).await?;
        let add = job_refs(&mut tx, &team, &args.add).await?;
        let remove = job_refs(&mut tx, &team, &args.remove).await?;
        ensure_jobs_visible(
            &mut tx,
            &team,
            &[vec![job.clone()], add.clone(), remove.clone()].concat(),
        )
        .await?;
        let deps = tx
            .set_dependencies(&job, &add, &remove)
            .await
            .map_err(|e| redact_foreign_ids(&team, e))
            .mcp()?;
        // Dependencies the caller cannot see are not theirs to be told about.
        let deps = visible_ids(&mut tx, &team, deps).await?;
        tx.commit().await.mcp()?;

        Ok(Json(out::DependenciesOut {
            job,
            dependencies: deps,
        }))
    }

    #[tool(
        name = "ready",
        description = "List the jobs that can be claimed right now: pending, with every \
                       dependency completed. This is the tool to call when looking for work. \
                       A job whose claim has expired also appears here, even though its status \
                       still says in-progress or active — see claim_jobs. \
                       agentType narrows to jobs meant for your kind of agent, plus unrouted \
                       jobs with no agentType set — it is a routing hint for a mixed fleet, not \
                       access control."
    )]
    pub async fn ready(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ReadyArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "ready").await?;
        let repo_id = maybe_repo_of(&mut tx, &team, args.repo, args.remote).await?;
        let mut jobs = tx.ready(repo_id, args.agent_type.as_deref()).await.mcp()?;
        retain_visible(&mut tx, &team, &mut jobs).await?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "blocked",
        description = "List pending jobs that cannot be claimed yet because something they \
                       depend on is unfinished. Useful for working out what to unblock first."
    )]
    pub async fn blocked(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "blocked").await?;
        let repo_id = maybe_repo_of(&mut tx, &team, args.repo, args.remote).await?;
        let mut jobs = tx.blocked(repo_id).await.mcp()?;
        retain_visible(&mut tx, &team, &mut jobs).await?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "stats",
        description = "Counts of jobs by state — pending, in-progress, active, completed, \
                       failed, cancelled, and how many of the pending ones are blocked — for \
                       the whole organization or one repository."
    )]
    pub async fn stats(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::StatsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "stats").await?;
        let repo_id = maybe_repo_of(&mut tx, &team, args.repo, args.remote).await?;
        let stats = match team.restriction() {
            None => tx.stats(repo_id).await.mcp()?,
            Some(teams) => tx.stats_for_teams(repo_id, &teams).await.mcp()?,
        };
        tx.commit().await.mcp()?;

        Ok(Json(out::StatsOut { stats }))
    }

    #[tool(
        name = "link_ticket",
        description = "Attach a tracker ticket to a job so future transitions (claim, complete, \
                       fail) post updates to it, and so sync_ticket can force a write-back on \
                       demand. Use this for a job that was queued by hand (add_job) or claimed \
                       before anyone thought to link it — a job created from an inbound webhook \
                       already has this set. Fails with ticket_already_linked if another live \
                       job in this repo already owns that ticket_ref."
    )]
    pub async fn link_ticket(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<LinkTicketArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::TRACKERS).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "link_ticket").await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        ensure_job_visible(&mut tx, &team, &id).await?;
        let job = tx
            .link_ticket(&id, args.tracker, &args.ticket_ref)
            .await
            .map_err(|e| redact_foreign_ids(&team, e))
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "sync_ticket",
        description = "Force an immediate outbound write-back to the ticket a job is linked to, \
                       reflecting the job's current status (in-progress, active, completed, \
                       failed, or cancelled) as a comment and, where the tracker supports it, a \
                       status transition. \
                       Use this after link_ticket, when nothing has been posted yet because no \
                       transition has fired since the link was made, or to retry after a \
                       tracker outage — unlike the automatic write-back after claim_jobs, \
                       complete_job, fail_job, and cancel_job (and after request_cancel, but \
                       only when it immediately finalizes a pending job with nobody to wait \
                       on; a request that only flags a claimed job does not sync until \
                       cancel_job later finalizes it), this call surfaces a tracker failure as \
                       its own error rather than swallowing it, because talking to the tracker is \
                       the entire point of calling it. \
                       For a GitHub job it also reads the issue first: each \"Blocked by #N\" \
                       (or \"Blocked by owner/repo#N\") line in the issue's body adds a \
                       dependency on the job linked to that issue, when exactly one job you can \
                       see has it. Each reference's outcome is in blockers: added, already (it \
                       was a dependency), no_job (no job has that issue; nothing changed), \
                       ambiguous (several do, listed in jobs; add the right one with \
                       set_dependencies), or cycle (that job is this one or already waits on \
                       it; nothing changed). Nothing is ever removed. The read also sets the \
                       job's ticketStale when the issue changed after its description was \
                       written. A pending GitHub job is read only: nothing is posted to it. \
                       Requires the job to already be linked via link_ticket and its repo to \
                       have an active tracker binding; a pending JIRA job is refused, because \
                       there is nothing to read or post for it yet."
    )]
    pub async fn sync_ticket(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::SyncTicketOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::TRACKERS).mcp()?;

        // Reading the job records nothing: the outbound call is the entire
        // point of this tool, and whether it succeeds is not known until
        // after it returns. The call is recorded later, only on success,
        // after the write-back (see below). It is free, so nothing here can
        // refuse it for billing.
        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        let id = job_ref(&mut tx, &team, &args.job).await?;
        let job = visible_job(&mut tx, &team, &id).await?;
        tx.commit().await.mcp()?;

        let Some(tracker) = job.tracker else {
            return Err(of_core::Error::Invalid(format!(
                "job {} has no tracker linked — call link_ticket first",
                job.id
            )))
            .mcp();
        };
        let Some(ticket_ref) = job.ticket_ref.clone() else {
            return Err(of_core::Error::Invalid(format!(
                "job {} has no ticket_ref linked — call link_ticket first",
                job.id
            )))
            .mcp();
        };
        let provider = provider_of(tracker);
        let (binding, connection) = match self
            .resolve_tracker_binding(job.org_id, job.repo_id, provider)
            .await
            .map_err(|error| tracker_sync_error(error, true))?
        {
            BindingLookup::NotConfigured => {
                return Err(of_core::Error::Invalid(format!(
                    "this repo has no active {provider} binding — configure one before \
                     calling sync_ticket"
                )))
                .mcp();
            }
            BindingLookup::Broken => {
                return Err(of_core::Error::Invalid(format!(
                    "this repo's {provider} binding points at a connection that no longer \
                     exists — this needs an operator to fix, not a retry"
                )))
                .mcp();
            }
            BindingLookup::Ready(binding, connection) => (binding, connection),
        };

        let mut blockers = Vec::new();
        if tracker == Tracker::Github {
            // A malformed ticket_ref (anything not "owner/repo#N") is
            // caller input, not a tracker outage: it will never succeed
            // no matter how many times it's retried. Catching it here,
            // ahead of the outbound calls, keeps it out of
            // `tracker_sync_error`'s retriable bucket — the parse
            // failure inside `sync_github_job` itself is a `String`
            // with no room to carry that distinction, so it must not be
            // the thing this checks.
            let Some((owner, repo, issue_number)) = parse_github_ticket_ref(&ticket_ref) else {
                return Err(of_core::Error::Invalid(format!(
                    "job {} has a ticket_ref {ticket_ref:?} that is not a valid GitHub \
                     issue reference (expected \"owner/repo#number\") — relink it with \
                     link_ticket before calling sync_ticket again",
                    job.id
                )))
                .mcp();
            };
            let issue = self
                .read_github_issue(&job, &connection.external_id, owner, repo, issue_number)
                .await
                .map_err(|error| tracker_sync_error(error, true))?;

            // A short "#N" on the issue means an issue in the same repository.
            let mut tickets: Vec<TicketQuery> = Vec::new();
            for raw in blocked_by(issue.body.as_deref().unwrap_or_default()) {
                let qualified = if raw.starts_with('#') {
                    format!("{owner}/{repo}{raw}")
                } else {
                    raw
                };
                match JobRef::parse(&qualified) {
                    Ok(JobRef::Ticket(t)) if !tickets.contains(&t) => tickets.push(t),
                    Ok(_) => {}
                    // Reachable only if the job's own owner/repo is outside the
                    // reference grammar, in which case no job could match it.
                    Err(error) => {
                        tracing::warn!(job_id = %job.id, %error, "skipping a Blocked by reference");
                    }
                }
            }

            // Before any write-back: a tracker failure after this leaves the
            // dependencies in place, and a retry reports them as `already`.
            let mut tx = self.tx(&caller).await?;
            // An `updated_at` newer than the revision otto-tower last stored
            // is a change it did not make. Whether it makes the job stale is
            // decided at read time, against when the description was written.
            let changed = issue.updated_at.as_deref().and_then(parse_remote_revision);
            let stored = job
                .remote_revision
                .as_deref()
                .and_then(parse_remote_revision);
            if let Some(at) = changed.filter(|at| stored.is_none_or(|s| *at > s)) {
                tx.note_ticket_changed(&job.id, at).await.mcp()?;
            }
            blockers = tx
                .add_ticket_dependencies(&job.id, &tickets, &team)
                .await
                .map_err(|e| redact_foreign_ids(&team, e))
                .mcp()?;
            if job.status == Status::Pending {
                // Nothing to post for a job nobody has picked up yet: the read
                // is the whole call. Recorded last, as below.
                let job = tx.get_job_visible(&job.id, &team).await.mcp()?;
                tx.commit().await.mcp()?;
                let mut charge_tx = self.tx(&caller).await?;
                self.charge(&mut charge_tx, &caller, "sync_ticket").await?;
                charge_tx.commit().await.mcp()?;
                return Ok(Json(out::SyncTicketOut { job, blockers }));
            }
            tx.commit().await.mcp()?;
        }

        let (transition, detail) = match job.status {
            // Only a JIRA job gets here pending: a pending GitHub job has
            // returned above, after its read. JIRA has no read path here, so
            // a pending JIRA job has nothing to post and nothing to read. This
            // runs after the binding lookup, so a job with nowhere to sync to
            // hears that first.
            Status::Pending => {
                return Err(of_core::Error::Invalid(format!(
                    "job {} is still pending — nothing has happened to it yet to sync to its ticket",
                    job.id
                )))
                .mcp();
            }
            // A label stored before #163's policy is withheld here as it is
            // from every other output, rather than posted to the ticket.
            Status::InProgress | Status::Active => (
                JobTransition::Claimed,
                of_core::agent_label::displayable(job.claimed_by_label.as_deref())
                    .map(str::to_string),
            ),
            Status::Completed => (JobTransition::Completed, job.result.clone()),
            Status::Failed => (JobTransition::Failed, job.error.clone()),
            // No dedicated outbound "cancelled" signal exists for either tracker (no
            // `not_planned` GitHub close reason, no distinct JIRA status category), so
            // `JobTransition::Cancelled` gets the same comment-only, no-close,
            // no-transition shape as `Failed` — but its own variant, not a reuse of
            // `Failed` itself, because that arm's JIRA handling transitions a ticket to
            // the "new" status category, which would announce "still needs doing" about
            // work someone just asked to stop. The detail prefers `error`, which is
            // populated whenever `cancel_job` (as opposed to `request_cancel`'s
            // immediate-finalize path) produced the cancellation, then falls back to
            // `cancel_reason` and finally a fixed string, so the tracker comment is
            // never `outbound_decision`'s literal "Cancelled." default when a more
            // specific one is available.
            Status::Cancelled => (
                JobTransition::Cancelled,
                Some(
                    job.error
                        .clone()
                        .or_else(|| job.cancel_reason.clone())
                        .unwrap_or_else(|| "Cancelled.".into()),
                ),
            ),
        };

        let plan = outbound_decision(transition, tracker, detail.as_deref());
        let job = match tracker {
            Tracker::Github => {
                let outcome = self
                    .sync_github_job(&job, &ticket_ref, &connection.external_id, &plan)
                    .await
                    .map_err(|error| tracker_sync_error(error, true))?;
                // The write-back commits on its own, before the call is even
                // recorded: this is loop-safety state for a call to the
                // tracker that has already happened, and it must survive
                // regardless of whether recording this call succeeds.
                // Recording in the same Tx as the write-back would roll the
                // write-back back on a failed record, leaving remote_revision
                // stale and making a caller's retry re-post to the tracker a
                // second time for a sync that already went through.
                let mut tx = self.tx(&caller).await?;
                if let Some(remote_revision) = outcome {
                    tx.set_remote_revision(&job.id, &remote_revision)
                        .await
                        .mcp()?;
                }
                // Re-read under the caller's scope, as of now: see the note at
                // the end of this function.
                let job = tx.get_job_visible(&job.id, &team).await;
                tx.commit().await.mcp()?;
                let mut charge_tx = self.tx(&caller).await?;
                self.charge(&mut charge_tx, &caller, "sync_ticket").await?;
                charge_tx.commit().await.mcp()?;
                job.mcp()?
            }
            Tracker::Jira => {
                // Same reasoning as the GitHub arm's pre-parse above: an
                // issue key that doesn't match JIRA's own `PROJECT-123`
                // grammar is caller input, not a tracker outage, and will
                // never succeed no matter how many times it's retried.
                // `sync_jira_job` also validates this (`Client::*` calls
                // `validate_jira_issue_key` before building a request), but
                // that failure comes back as a `String` with no room to
                // mark it non-retriable — checking it here, ahead of the
                // call, is what keeps it out of `tracker_sync_error`'s
                // retriable bucket.
                if of_trackers::jira::validate_jira_issue_key(&ticket_ref).is_err() {
                    return Err(of_core::Error::Invalid(format!(
                        "job {} has a ticket_ref {ticket_ref:?} that is not a valid JIRA \
                         issue key (expected \"PROJECT-123\") — relink it with link_ticket \
                         before calling sync_ticket again",
                        job.id
                    )))
                    .mcp();
                }
                let outcome = self
                    .sync_jira_job(&job, &ticket_ref, &binding.external_ref, &connection, &plan)
                    .await
                    .map_err(|error| tracker_sync_error(error, true))?;
                // Same reasoning as the GitHub arm above: the write-back (the
                // revision and any rotated JIRA credentials) commits before
                // the call is recorded, so a failed record never erases
                // loop-safety state for a tracker call that already happened.
                let mut tx = self.tx(&caller).await?;
                if let Some(remote_revision) = outcome.remote_revision.as_deref() {
                    tx.set_remote_revision(&job.id, remote_revision)
                        .await
                        .mcp()?;
                }
                let job = tx.get_job_visible(&job.id, &team).await;
                if let Some(rotated) = outcome.rotated_credentials.as_ref() {
                    let webhook_secret = connection
                        .encrypted_webhook_secret
                        .as_deref()
                        .map(decode_stored_secret)
                        .transpose()
                        .mcp()?;
                    upsert_connection(
                        &mut tx,
                        Provider::Jira,
                        &connection.external_id,
                        Some(rotated),
                        webhook_secret.as_ref(),
                    )
                    .await
                    .mcp()?;
                }
                tx.commit().await.mcp()?;
                let mut charge_tx = self.tx(&caller).await?;
                self.charge(&mut charge_tx, &caller, "sync_ticket").await?;
                charge_tx.commit().await.mcp()?;
                job.mcp()?
            }
        };

        // Authorization is decided once, when the call starts, as for any
        // request already in flight: the tracker round trip cannot be held
        // inside a transaction, and once it has happened its write-back is
        // loop-safety state that must land whoever may see the job now. What
        // is decided again is what the caller gets back. A job whose repo moved
        // out of their teams mid-call is `job_not_found`, not returned.
        Ok(Json(out::SyncTicketOut { job, blockers }))
    }
}
