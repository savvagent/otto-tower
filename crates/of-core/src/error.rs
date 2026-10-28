//! Domain errors.
//!
//! Tenancy failures are not redefined here: they arrive as [`Error::Tenant`]
//! (`otto_tenant::Error`), wrapped rather than flattened, so that the substrate
//! owns their codes and wording. Identity questions are the platform's:
//! [`Error::Platform`] is "the platform could not answer", and every caller
//! treats it as a refusal. [`Error::code`] and [`Error::retriable`] delegate to
//! the wrapped errors.
//!
//! These are written to be readable by an LLM tool caller that has never seen
//! the docs: a failure says what went wrong, what the valid options were, and
//! what to call next. `NotFound` naming the org, `UnknownRepo` listing the
//! registered slugs, and `LeaseHeld` naming the holder all exist for that
//! reason — a bare "not found" makes an agent guess, and a guessing agent
//! retries wrongly.

use crate::ids::JobId;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("job {0} not found")]
    JobNotFound(JobId),

    /// An issue reference (`#928`, `owner/repo#928`) that names no job the
    /// caller can see. The same `job_not_found` code as [`Error::JobNotFound`],
    /// so an agent already branching on it keeps working; hidden and other
    /// orgs' jobs are indistinguishable from none.
    #[error(
        "no job you can see has a ticketRef matching {0:?}; list_jobs(ticket=…) shows what does"
    )]
    NoJobForTicket(String),

    /// An issue reference that more than one visible job carries, most often
    /// an issue whose first job failed and was queued again. `jobs` is newest
    /// first and at most ten long; `more` says there were others.
    #[error(
        "{ticket:?} matches {}; pass one of those ids{}; list_jobs(ticket={ticket:?}) shows \
         each one's status",
        ambiguous_ids(.jobs, *.more),
        full_form_hint(.ticket)
    )]
    AmbiguousTicket {
        ticket: String,
        jobs: Vec<JobId>,
        more: bool,
    },

    #[error("repo not found: {0}")]
    RepoNotFound(String),

    /// The agent's working directory could not be resolved to a registered repo.
    /// Deliberately never falls back to a default repo: silently queueing work
    /// against the wrong repository is worse than failing.
    ///
    /// `known` must be built by `repo_slug::render_known`, so a stored slug never
    /// reaches the sentence unchecked.
    #[error(
        "could not resolve a repo from {attempted}. Registered repos: {known}. \
         Register this one with register_repo, or pass an explicit repo slug."
    )]
    RepoUnresolved { attempted: String, known: String },

    #[error("repo slug {0:?} is already registered in this org")]
    RepoSlugTaken(String),

    #[error("remote {0:?} is already registered to {owner}", owner = crate::repo_slug::describe(.1))]
    RemoteTaken(String, String),

    #[error("job {job} is {actual}, not {expected}")]
    WrongStatus {
        job: JobId,
        actual: String,
        expected: String,
    },

    /// `reason` is pre-rendered per call site rather than built from a single
    /// fixed template, because the three cases `ensure_claim_held` raises this
    /// for are not interchangeable: a different account holds the claim, an
    /// expired claim has not yet been picked up by anyone, or a different
    /// *instance* of your own account has reclaimed it (the claim-generation
    /// fence, savvagent/otto-factory#103) — and "not you" is simply false in
    /// the last case, since the account genuinely is you. Every branch still
    /// ends with the same guidance: `get_job` is for the caller's own
    /// orientation only, never a source for a fresh `expected_attempts` to
    /// retry with — reading `attempts` back from `get_job` and feeding it into
    /// a retry is the exact bypass this fence exists to prevent — and
    /// `claim_jobs` is the only way to actually take the job back.
    #[error("job {job} {reason}")]
    AlreadyClaimed { job: JobId, reason: String },

    #[error(
        "ticket {ticket_ref} is already linked to job {job} — unlink it there first, \
         or use a different ticket_ref"
    )]
    TicketAlreadyLinked { ticket_ref: String, job: JobId },

    #[error("dependency cycle: {0} would depend on itself through {1}")]
    DependencyCycle(JobId, JobId),

    #[error("{resource} of this repo is leased by {holder} until {expires_at}")]
    LeaseHeld {
        resource: String,
        holder: String,
        expires_at: chrono::DateTime<chrono::Utc>,
    },

    #[error("lease {0} is not held by you")]
    LeaseNotHeld(String),

    /// A caller-chosen `agent` label failed [`crate::agent_label::validate`].
    /// `problem` describes what is wrong (its length, or the offending code
    /// point and position) and deliberately never contains the label itself:
    /// the label is exactly the text being refused, and echoing it would hand
    /// it straight back as tool output.
    #[error(
        "the agent label {problem}. Pass agent as a single line of at most {max} visible \
         characters with no control, line-break, or invisible formatting characters (for \
         example \"api-agent@ci-7\"), or omit it; nothing was changed.",
        max = crate::agent_label::MAX_LEN
    )]
    InvalidAgentLabel { problem: String },

    /// A repo slug failed [`crate::repo_slug::validate`]. `problem` never
    /// contains the slug itself, for the same reason as `InvalidAgentLabel`.
    #[error(
        "the repo slug {problem}. Pass slug as 1 to {max} characters: it must start with an \
         ASCII letter or digit and contain only ASCII letters, digits, '-', '_', '.' and '/' \
         (for example \"acme/api-v2\"); nothing was changed.",
        max = crate::repo_slug::MAX_LEN
    )]
    InvalidRepoSlug { problem: String },

    /// The platform does not know this team in this org (never existed, other
    /// tenant's, or deleted). Raised by `VerifiedTeam::verify` and
    /// `VerifiedTeam::by_slug`, and it is a refusal: an unknown team is never
    /// read as "org-wide".
    #[error(
        "team {team} was not found in this organization. Teams are managed in the \
         otto platform console; use a team id from there, or omit the team to make \
         the repo organization-wide."
    )]
    TeamNotFound { team: String },

    /// A message tool's team channel that is unknown in this org **or** one the
    /// caller is not on: one error for both, built from the slug the caller
    /// passed, so the message tools cannot be used to tell which teams exist.
    /// The same stable `team_not_found` code as [`Error::TeamNotFound`]; its
    /// own text because "omit the team to make the repo organization-wide"
    /// is wrong advice for a channel.
    #[error(
        "team {team} was not found in this organization, or you are not on it. Teams \
         are managed in the otto platform console; name a team you are on, or use a \
         repo's channel (repo or remote) instead."
    )]
    TeamChannelNotFound { team: String },

    #[error("{0}")]
    Invalid(String),

    /// A `SAVEPOINT`-guarded unique-violation recovery re-queried for the
    /// concurrent winner's row and found nothing — the winner's row vanished
    /// between the violation and the re-query (e.g. a concurrent delete). This
    /// is a transient server-side race, not a problem with the caller's
    /// request: unlike `Invalid`, retrying the identical call can plausibly
    /// succeed once the row settles. See `create_from_ticket`, `link_ticket`,
    /// `Tx::add_job`, and `Tx::send_message` for the four sites that raise it.
    #[error("{0}")]
    RaceLost(String),

    #[error(
        "idempotency_key {key:?} was already used for a different {tool} call in this \
         organization. Use a new key for a different request, or omit idempotency_key to \
         always create a new one."
    )]
    IdempotencyKeyConflict { key: String, tool: &'static str },

    /// The org was deleted, or the caller was removed from it, after the token
    /// was introspected. Re-checked inside the transaction, under the org's
    /// lifecycle lock, so a request already past authentication cannot write
    /// into an org that is being purged. Not retriable: the token is done here.
    #[error(
        "this organization no longer exists, or you are no longer a member of it; \
         nothing was changed. Sign in again to see which organizations you can use."
    )]
    AccessRevoked,

    /// One of the errors above, re-worded because its own message named a row
    /// the caller cannot see (the job a ticket is already linked to, the repo a
    /// remote belongs to, the far end of a dependency cycle). `code` is the
    /// original error's, so an agent branching on [`Error::code`] takes the same
    /// branch whether or not the detail was withheld.
    #[error("{message}")]
    Redacted {
        code: &'static str,
        message: &'static str,
    },

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    /// The platform could not answer an identity question (unreachable, 5xx,
    /// rejected our credential, unreadable body). Whatever was being decided is
    /// refused: callers fail closed on this, never open.
    #[error("the otto platform could not be reached to check this ({0}); nothing was changed")]
    Platform(#[from] otto_resource::Error),

    /// A failure from the tenant substrate (`otto-tenant`): an unknown org, a
    /// crypto or configuration fault, an isolation failure at startup.
    #[error(transparent)]
    Tenant(#[from] otto_tenant::Error),
}

impl Error {
    /// A stable, machine-readable code for the MCP error envelope. Agents
    /// branch on this; humans read the message.
    pub fn code(&self) -> &'static str {
        match self {
            Error::JobNotFound(_) | Error::NoJobForTicket(_) => "job_not_found",
            Error::AmbiguousTicket { .. } => "ambiguous_ticket_ref",
            Error::RepoNotFound(_) => "repo_not_found",
            Error::RepoUnresolved { .. } => "repo_unresolved",
            Error::RepoSlugTaken(_) => "repo_slug_taken",
            Error::RemoteTaken(..) => "remote_taken",
            Error::WrongStatus { .. } => "wrong_status",
            Error::AlreadyClaimed { .. } => "already_claimed",
            Error::TicketAlreadyLinked { .. } => "ticket_already_linked",
            Error::DependencyCycle(..) => "dependency_cycle",
            Error::LeaseHeld { .. } => "lease_held",
            Error::LeaseNotHeld(_) => "lease_not_held",
            Error::InvalidAgentLabel { .. } => "invalid_agent_label",
            Error::InvalidRepoSlug { .. } => "invalid_repo_slug",
            Error::TeamNotFound { .. } | Error::TeamChannelNotFound { .. } => "team_not_found",
            Error::Invalid(_) => "invalid_argument",
            Error::RaceLost(_) => "race_lost",
            Error::IdempotencyKeyConflict { .. } => "idempotency_key_conflict",
            Error::AccessRevoked => "access_revoked",
            Error::Redacted { code, .. } => code,
            Error::Db(_) => "internal_error",
            Error::Platform(_) => "platform_unavailable",
            Error::Tenant(e) => e.code(),
        }
    }

    /// Whether retrying the identical call could plausibly succeed. `LeaseHeld`
    /// is retriable (the lease expires); `DependencyCycle` is not (the request
    /// is wrong). Agents use this to decide between backing off and rethinking.
    ///
    /// `AlreadyClaimed` is **not** retriable, unlike `LeaseHeld`, even though
    /// both describe "someone else has this right now": a lease's holder can
    /// let it lapse without acting, so waiting and retrying the identical
    /// `acquire_lease` call can succeed on its own. `AlreadyClaimed` today is
    /// raised only by `ensure_claim_held` — a caller that is not (or is no
    /// longer) a job's claim holder — and retrying `complete_job`/`fail_job`/
    /// `cancel_job`/`renew_claim` with the same arguments can never succeed;
    /// the only way forward is a different call (`claim_jobs`, or `get_job`
    /// to see who holds it now). Telling an agent this is retriable would
    /// have it busy-loop a call that is structurally doomed.
    ///
    /// `RaceLost` is retriable for the same reason `Db` is: both describe a
    /// condition of the server's transaction, not the caller's arguments, and
    /// an identical retry can land in a different, successful outcome once the
    /// concurrent write that caused it has finished settling.
    pub fn retriable(&self) -> bool {
        match self {
            Error::LeaseHeld { .. } | Error::Db(_) | Error::RaceLost(_) => true,
            // A database fault is the same transient condition however many
            // crates it passed through on the way up.
            Error::Tenant(otto_tenant::Error::Db(_)) => true,
            Error::Platform(e) => e.is_retriable(),
            _ => false,
        }
    }
}

fn ambiguous_ids(jobs: &[JobId], more: bool) -> String {
    let list = jobs
        .iter()
        .map(JobId::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if more {
        format!("{list} and more")
    } else {
        list
    }
}

/// The full form only disambiguates a short reference; a full one that is
/// still ambiguous needs an id.
fn full_form_hint(ticket: &str) -> String {
    if ticket.starts_with('#') {
        format!(", or the full owner/repo{ticket} form")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Retrying the identical call with the identical key will fail
    /// identically every time — the caller must change the key or the
    /// payload, not back off and try again.
    #[test]
    fn idempotency_key_conflict_is_not_retriable() {
        let e = Error::IdempotencyKeyConflict {
            key: "k".into(),
            tool: "add_job",
        };
        assert!(!e.retriable());
        assert_eq!(e.code(), "idempotency_key_conflict");
    }

    /// The channel lookup's own text, with the same stable code as the
    /// repo and job paths' `TeamNotFound`, so a caller branching on
    /// `team_not_found` handles both.
    #[test]
    fn team_channel_not_found_shares_team_not_founds_code() {
        let e = Error::TeamChannelNotFound {
            team: "\"platform\"".into(),
        };
        assert_eq!(e.code(), "team_not_found");
        assert!(!e.retriable());
        assert!(e.to_string().contains("or you are not on it"), "{e}");
        let plain = Error::TeamNotFound { team: "t".into() };
        assert!(!plain.to_string().contains("not on it"), "{plain}");
    }

    #[test]
    fn ticket_errors_have_stable_codes_and_are_not_retriable() {
        let none = Error::NoJobForTicket("#928".into());
        assert_eq!(none.code(), "job_not_found");
        assert!(!none.retriable());
        assert!(none.to_string().contains("\"#928\""), "{none}");

        let two = Error::AmbiguousTicket {
            ticket: "#928".into(),
            jobs: vec![JobId::from("job-61"), JobId::from("job-52")],
            more: false,
        };
        assert_eq!(two.code(), "ambiguous_ticket_ref");
        assert!(!two.retriable());
        let msg = two.to_string();
        assert!(msg.contains("job-61, job-52;"), "{msg}");
        assert!(msg.contains("owner/repo#928"), "{msg}");
        assert!(msg.contains("list_jobs(ticket=\"#928\")"), "{msg}");

        let many = Error::AmbiguousTicket {
            ticket: "acme/api#5".into(),
            jobs: vec![JobId::from("job-2"), JobId::from("job-1")],
            more: true,
        };
        let msg = many.to_string();
        assert!(msg.contains("job-2, job-1 and more"), "{msg}");
        assert!(!msg.contains("owner/repo"), "{msg}");
    }

    #[test]
    fn race_lost_is_retriable() {
        let e = Error::RaceLost("x".into());
        assert!(e.retriable());
        assert_eq!(e.code(), "race_lost");
    }
}
