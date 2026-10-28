//! Repo leases — the primitive that stops two agents colliding on one resource.
//!
//! A lease is **advisory and time-bounded**. otto-tower cannot see what an
//! agent actually does with a leased resource — a git operation, a migration
//! run, a deploy — so it cannot prevent a determined agent from working on one
//! anyway; what it can do is make the collision visible before it happens and
//! give a well-behaved agent somewhere else to go. That limitation is deliberate
//! and is documented to customers rather than hidden — a lease is a coordination
//! signal, not a mutex.
//!
//! Time-bounded matters just as much: an agent that crashes mid-job must not
//! wedge a repository forever. Leases expire, and any acquire reaps the expired
//! ones first.

use crate::error::{Error, Result};
use crate::ids::{JobId, RepoId};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::Tx;
use serde::Serialize;
use sqlx::FromRow;

/// Default lease lifetime. Long enough that a working agent renewing on a normal
/// cadence never loses its lease mid-task, short enough that a crashed agent
/// frees the resource while a human is still in the room.
pub const DEFAULT_TTL_SECS: i64 = 900;

/// Upper bound on a client-requested TTL. Without a cap, one buggy agent could
/// take an effectively permanent lease and only an admin could clear it.
pub const MAX_TTL_SECS: i64 = 4 * 3600;

/// Upper bound on a resource string's length, matching
/// [`crate::idempotency::MAX_KEY_LEN`]'s reasoning: long enough for any
/// reasonable caller-chosen name, short enough that a released lease's row
/// (kept for the console's history, never deleted) cannot become unbounded
/// free storage. Generalizing `resource` past a git branch name removed the
/// informal length ceiling a branch name used to imply.
pub const MAX_RESOURCE_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub id: uuid::Uuid,
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub resource: String,
    pub holder_user_id: UserId,
    #[serde(serialize_with = "crate::agent_label::serialize_stored")]
    pub holder_label: Option<String>,
    pub job_id: Option<String>,
    /// The issue the lease's job is linked to (its `ticketRef`, such as
    /// `acme/api#928`), so "who is working on #928?" needs no second lookup.
    /// Filled in only by `list_leases`; `None` when the lease names no job,
    /// the job has no issue, or the job is one the caller cannot see.
    #[sqlx(default)]
    pub job_ticket_ref: Option<String>,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
    pub renewed_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub released_at: Option<chrono::DateTime<chrono::Utc>>,
}

const LEASE_COLS: &str = "id, org_id, repo_id, resource, holder_user_id, holder_label, job_id, \
                          acquired_at, renewed_at, expires_at, released_at";

/// [`LEASE_COLS`] qualified for `list_leases`'s join with `jobs j`. A separate
/// list, not an `l.` prefix on the shared one: that is also spliced into
/// `RETURNING` and into unaliased `SELECT`s, where a qualifier fails only at
/// runtime.
const LEASE_COLS_L: &str = "l.id, l.org_id, l.repo_id, l.resource, l.holder_user_id, \
                            l.holder_label, l.job_id, l.acquired_at, l.renewed_at, \
                            l.expires_at, l.released_at, j.ticket_ref AS job_ticket_ref";

/// Extension methods on [`Tx`] for this module's domain (see the crate docs for why
/// these are extension traits rather than inherent methods).
pub trait LeasesExt {
    /// Take a lease on `(repo, resource)`.
    ///
    /// Reaps expired leases for this resource first, then inserts. If a live
    /// lease is already held the error names the holder and the expiry, so the
    /// caller can decide between waiting, picking different work, or messaging
    /// the holder — all three are reasonable, and only the agent knows which.
    ///
    /// Re-acquiring a lease you already hold renews it instead of failing, so an
    /// agent that lost track of its own state converges rather than deadlocking
    /// against itself.
    fn acquire_lease(
        &mut self,
        repo_id: RepoId,
        resource: &str,
        holder: UserId,
        label: Option<&str>,
        job_id: Option<&JobId>,
        ttl_secs: Option<i64>,
    ) -> impl std::future::Future<Output = Result<Lease>> + Send;

    /// Extend a lease you hold. Only the holder may renew — otherwise any agent
    /// could keep another's lease alive indefinitely.
    fn renew_lease(
        &mut self,
        lease_id: uuid::Uuid,
        holder: UserId,
        ttl_secs: Option<i64>,
    ) -> impl std::future::Future<Output = Result<Lease>> + Send;

    fn release_lease(
        &mut self,
        lease_id: uuid::Uuid,
        holder: UserId,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// The repo a lease (live or not) belongs to, if the lease exists.
    fn lease_repo_id(
        &mut self,
        lease_id: uuid::Uuid,
    ) -> impl std::future::Future<Output = Result<Option<RepoId>>> + Send;

    /// Live leases — "who is in this repo right now?".
    ///
    /// Filters expired rows in the query rather than relying on the reaper, so
    /// the answer is correct even when nothing has tried to acquire recently.
    ///
    /// Knows nothing about teams: each lease's `job_id` and `job_ticket_ref`
    /// come back for every job. Anything shown to a caller goes through
    /// [`hide_unseen_jobs`] first, or it names work of a team they are not on.
    fn list_leases(
        &mut self,
        repo_id: Option<RepoId>,
    ) -> impl std::future::Future<Output = Result<Vec<Lease>>> + Send;
}

impl LeasesExt for Tx<'_> {
    async fn acquire_lease(
        &mut self,
        repo_id: RepoId,
        resource: &str,
        holder: UserId,
        label: Option<&str>,
        job_id: Option<&JobId>,
        ttl_secs: Option<i64>,
    ) -> Result<Lease> {
        let resource = resource.trim();
        if resource.is_empty() {
            return Err(Error::Invalid("lease resource must not be empty".into()));
        }
        if resource.len() > MAX_RESOURCE_LEN {
            return Err(Error::Invalid(format!(
                "lease resource is {} bytes; the limit is {MAX_RESOURCE_LEN}",
                resource.len()
            )));
        }
        // Before any SQL, so a refused label writes nothing — not even the
        // reap below. `LeaseHeld` quotes it to other members' agents (#163).
        let label = crate::agent_label::validate(label)?;
        let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS).clamp(60, MAX_TTL_SECS);
        let org = self.org();

        // Reap expired leases on this resource. Marking them released (rather
        // than deleting) keeps the history readable in the console — "who had
        // this resource when the build broke?" is a question people actually ask.
        sqlx::query(
            "UPDATE repo_leases SET released_at = now() \
             WHERE org_id = $1 AND repo_id = $2 AND resource = $3 \
               AND released_at IS NULL AND expires_at <= now()",
        )
        .bind(org)
        .bind(repo_id)
        .bind(resource)
        .execute(self.conn())
        .await?;

        let existing: Option<Lease> = sqlx::query_as(&format!(
            "SELECT {LEASE_COLS} FROM repo_leases \
             WHERE org_id = $1 AND repo_id = $2 AND resource = $3 AND released_at IS NULL \
             FOR UPDATE"
        ))
        .bind(org)
        .bind(repo_id)
        .bind(resource)
        .fetch_optional(self.conn())
        .await?;

        if let Some(live) = existing {
            if live.holder_user_id == holder {
                return self.renew_lease(live.id, holder, Some(ttl)).await;
            }
            return Err(Error::LeaseHeld {
                resource: resource.to_string(),
                holder: crate::agent_label::holder(
                    live.holder_label.as_deref(),
                    live.holder_user_id,
                ),
                expires_at: live.expires_at,
            });
        }

        let lease = sqlx::query_as(&format!(
            "INSERT INTO repo_leases (org_id, repo_id, resource, holder_user_id, holder_label, \
                                      job_id, expires_at) \
             VALUES ($1,$2,$3,$4,$5,$6, now() + make_interval(secs => $7)) \
             RETURNING {LEASE_COLS}"
        ))
        .bind(org)
        .bind(repo_id)
        .bind(resource)
        .bind(holder)
        .bind(label)
        .bind(job_id.map(|j| j.0.as_str()))
        .bind(ttl as f64)
        .fetch_one(self.conn())
        .await?;

        Ok(lease)
    }

    async fn renew_lease(
        &mut self,
        lease_id: uuid::Uuid,
        holder: UserId,
        ttl_secs: Option<i64>,
    ) -> Result<Lease> {
        let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS).clamp(60, MAX_TTL_SECS);
        let org = self.org();

        sqlx::query_as(&format!(
            "UPDATE repo_leases \
             SET renewed_at = now(), expires_at = now() + make_interval(secs => $4) \
             WHERE org_id = $1 AND id = $2 AND holder_user_id = $3 AND released_at IS NULL \
             RETURNING {LEASE_COLS}"
        ))
        .bind(org)
        .bind(lease_id)
        .bind(holder)
        .bind(ttl as f64)
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| Error::LeaseNotHeld(lease_id.to_string()))
    }

    async fn release_lease(&mut self, lease_id: uuid::Uuid, holder: UserId) -> Result<()> {
        let org = self.org();
        let n = sqlx::query(
            "UPDATE repo_leases SET released_at = now() \
             WHERE org_id = $1 AND id = $2 AND holder_user_id = $3 AND released_at IS NULL",
        )
        .bind(org)
        .bind(lease_id)
        .bind(holder)
        .execute(self.conn())
        .await?
        .rows_affected();

        if n == 0 {
            return Err(Error::LeaseNotHeld(lease_id.to_string()));
        }
        Ok(())
    }

    async fn lease_repo_id(&mut self, lease_id: uuid::Uuid) -> Result<Option<RepoId>> {
        let org = self.org();
        Ok(
            sqlx::query_scalar("SELECT repo_id FROM repo_leases WHERE org_id = $1 AND id = $2")
                .bind(org)
                .bind(lease_id)
                .fetch_optional(self.conn())
                .await?,
        )
    }

    async fn list_leases(&mut self, repo_id: Option<RepoId>) -> Result<Vec<Lease>> {
        let org = self.org();
        let leases = sqlx::query_as(&format!(
            "SELECT {LEASE_COLS_L} FROM repo_leases l \
             LEFT JOIN jobs j ON j.org_id = l.org_id AND j.id = l.job_id \
             WHERE l.org_id = $1 AND l.released_at IS NULL AND l.expires_at > now() \
               AND ($2::uuid IS NULL OR l.repo_id = $2) \
             ORDER BY l.acquired_at"
        ))
        .bind(org)
        .bind(repo_id)
        .fetch_all(self.conn())
        .await?;
        Ok(leases)
    }
}

/// Blank the `job_id` (and with it `job_ticket_ref`) of leases whose job
/// `scope` cannot see: a lease on a visible repo can still name a job of a team
/// the caller is not on, and that job's issue is withheld exactly as its id is.
pub async fn hide_unseen_jobs(
    tx: &mut Tx<'_>,
    leases: &mut [Lease],
    scope: &crate::teams::TeamScope,
) -> Result<()> {
    use crate::jobs::JobsExt;
    if scope.is_all() {
        return Ok(());
    }
    let named: Vec<JobId> = leases
        .iter()
        .filter_map(|l| l.job_id.clone().map(JobId))
        .collect();
    if named.is_empty() {
        return Ok(());
    }
    let seen: std::collections::HashSet<String> = tx
        .visible_job_ids(&named, scope)
        .await?
        .into_iter()
        .map(|j| j.0)
        .collect();
    for l in leases {
        if l.job_id.as_ref().is_some_and(|j| !seen.contains(j)) {
            l.job_id = None;
            l.job_ticket_ref = None;
        }
    }
    Ok(())
}
