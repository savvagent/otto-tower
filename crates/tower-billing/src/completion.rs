//! What a completed job writes to the usage outbox: the one place that decides.
//!
//! A job reaches `completed` two ways: an agent's `complete_job`, and an
//! inbound tracker close of the linked ticket. Both record the completion
//! through [`record_job_completed`], and nothing else writes a completion row,
//! so the two paths cannot drift apart on what a completion costs.
//!
//! **Exactly one row per completion**, written in the transaction that moves
//! the job to `completed`, so it commits or rolls back with the transition:
//!
//! - **billed**: `meter = otto-tower/jobs_completed`, quantity 1,
//!   `billable = true`. The platform prices it.
//! - **not billed**: a plain, unmetered `billable = false` row. Still recorded,
//!   so the history shows the completion and `org_counters.jobs_completed_total`
//!   always equals the number of completion rows.
//!
//! *Whether* a completion is billed is the caller's decision, made from
//! things only it knows: `complete_job` bills (and is refused instead when the
//! org is blocked; see [`crate::Meter::charge_completion`]); an inbound close
//! bills only a job an agent held, for an org that is not blocked (see
//! `tower_web::routes::webhooks`).

use savvagent_tenant::ids::UserId;
use savvagent_tenant::Tx;
use uuid::Uuid;

use crate::error::Result;
use crate::outbox::{self, Metered};

/// The tool name a completion through `complete_job` is recorded under.
pub const TOOL_COMPLETE_JOB: &str = "complete_job";

/// The tool name a completion through an inbound tracker close is recorded
/// under. Not an MCP tool: no agent made the call.
pub const TOOL_INBOUND_TICKET_CLOSE: &str = "inbound_ticket_close";

/// How a job came to be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSource {
    /// An agent called `complete_job`.
    CompleteJob,
    /// The linked ticket was closed in the tracker and its webhook arrived.
    InboundTicketClose,
}

impl CompletionSource {
    /// Every source, for code that has to know every tool a completion is
    /// recorded under.
    pub const ALL: [CompletionSource; 2] = [Self::CompleteJob, Self::InboundTicketClose];

    /// The `tool` the completion row is recorded under.
    ///
    /// ```
    /// use tower_billing::CompletionSource;
    /// assert_eq!(CompletionSource::CompleteJob.tool(), "complete_job");
    /// assert_eq!(CompletionSource::InboundTicketClose.tool(), "inbound_ticket_close");
    /// ```
    pub const fn tool(self) -> &'static str {
        match self {
            Self::CompleteJob => TOOL_COMPLETE_JOB,
            Self::InboundTicketClose => TOOL_INBOUND_TICKET_CLOSE,
        }
    }
}

/// Record one completed job in `tx`: billed on the completed-jobs meter, or an
/// unmetered, unbilled row. Returns the row's `event_id`.
///
/// Call it in the transaction that completes the job, exactly once per
/// completion. `user` is the agent that completed it, `None` for a tracker
/// close.
pub async fn record_job_completed(
    tx: &mut Tx<'_>,
    user: Option<UserId>,
    source: CompletionSource,
    billed: bool,
) -> Result<Uuid> {
    let metered = billed.then(Metered::jobs_completed);
    outbox::enqueue(tx, user, source.tool(), billed, metered.as_ref()).await
}
