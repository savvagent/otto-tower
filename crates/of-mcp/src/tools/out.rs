//! What tools return.
//!
//! Every result is an **object with one named field**, never a bare array or a
//! bare scalar. Two reasons, and the first is not negotiable: MCP requires a
//! tool's `outputSchema` to have root type `object`, so `Json<Vec<Job>>` is not
//! a legal result. The second is that an envelope can grow — the day `list_jobs`
//! wants to return a cursor alongside the rows, `{"jobs": [...]}` gains a field
//! and every existing caller still works, where a top-level array would have to
//! break them all.
//!
//! The payloads are `of-core`'s own domain types rather than view structs
//! mirrored into this crate. A parallel set of types would need updating in two
//! places whenever a column is added, and the copy that got missed would be the
//! one agents read. `of-core` derives `JsonSchema` alongside the `Serialize` it
//! already had, which is the same concern rather than a new one.

use of_core::ids::JobId;
use of_core::jobs::{BlockerOutcome, Job, Stats};
use of_core::leases::Lease;
use of_core::messages::Message;
use of_core::repos::Repo;
use otto_tenant::ids::{OrgId, UserId};
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoOut {
    pub repo: Repo,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReposOut {
    pub repos: Vec<Repo>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobOut {
    pub job: Job,
}

/// `sync_ticket`'s result: the job as it now stands, and what became of each
/// "Blocked by" reference in its issue.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncTicketOut {
    pub job: Job,
    /// One entry per issue named on a "Blocked by" line of the GitHub issue's
    /// body, in order. Empty for a JIRA job, or an issue with no such line.
    pub blockers: Vec<BlockerOutcome>,
    /// Whether "Blocked by" lines were read into dependencies at all. False
    /// for a JIRA job, and for a token without `jobs:write`; then an empty
    /// `blockers` says nothing about what the issue's body contains.
    pub blockers_imported: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobsOut {
    pub jobs: Vec<Job>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeletedOut {
    pub deleted: JobId,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DependenciesOut {
    pub job: JobId,
    /// Everything this job now waits for, after the change.
    pub dependencies: Vec<JobId>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatsOut {
    pub stats: Stats,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaseOut {
    pub lease: Lease,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeasesOut {
    pub leases: Vec<Lease>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleasedOut {
    pub released: uuid::Uuid,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageOut {
    pub message: Message,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessagesOut {
    pub messages: Vec<Message>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CursorOut {
    /// Where the read cursor actually landed, which is not always what was
    /// asked for — it is clamped to the newest message that exists.
    pub cursor: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnreadOut {
    pub unread: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WatchOutcome {
    /// Something changed. Refetch whatever you care about.
    Changed,
    /// Nothing happened in the time allowed. Call `watch` again.
    Timeout,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchOut {
    pub outcome: WatchOutcome,
    /// How long the call actually blocked — not the timeout it asked for. On a
    /// wake-up these differ, and an agent pacing itself off this field needs the
    /// real number.
    pub waited_seconds: u64,
}

/// A member's role in an org. The platform owns roles; this is how one is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl From<otto_resource::Role> for Role {
    fn from(r: otto_resource::Role) -> Self {
        match r {
            otto_resource::Role::Owner => Role::Owner,
            otto_resource::Role::Admin => Role::Admin,
            otto_resource::Role::Member => Role::Member,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmI {
    /// Which service answered: `Savvagent Tower`. Matches `resource_name` in the
    /// protected-resource metadata, so an agent holding several Savvagent
    /// connections can tell them apart.
    pub service: &'static str,
    pub user: UserOut,
    pub org: OrgOut,
    /// The caller's role in this org, as the platform last reported it.
    pub role: Option<Role>,
    pub token: TokenOut,
    /// This month's completed jobs, budget and any block for this org. Here as
    /// well as in `usage` because an agent that calls one thing at the start of
    /// a session calls this one, and finding out it is nearly out of budget
    /// after the work is queued is finding out too late.
    pub usage: of_billing::Status,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserOut {
    pub id: UserId,
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrgOut {
    pub id: OrgId,
    pub slug: Option<String>,
    pub name: Option<String>,
    /// `free`, `team`, `business`, or `enterprise`.
    pub plan: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenOut {
    /// "oauth" for a browser-authorized token, "pat" for a personal access
    /// token pasted into a client's configuration.
    pub kind: &'static str,
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageOut {
    pub usage: of_billing::Status,
}
