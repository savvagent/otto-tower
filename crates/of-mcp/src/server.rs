//! The MCP service: the shared state a tool call runs against, and the shape
//! every tool has in common.
//!
//! ## Where the caller comes from
//!
//! `rmcp` creates one service instance per MCP *session*, and a session spans
//! many HTTP requests. The authenticated principal therefore cannot live on the
//! service — it belongs to the request. The transport carries each request's
//! [`http::request::Parts`] into the tool call's context, and
//! [`crate::auth::require_bearer`] has already inserted a [`Principal`] into
//! that request's extensions, so every handler starts with
//! `Extension(parts): Extension<Parts>` and calls [`Factory::caller`].
//!
//! That indirection is doing real work: it is what makes token revocation take
//! effect on the next call rather than at the end of a session that may last
//! for days.
//!
//! ## Where the tenant comes from
//!
//! From the token, and only from the token. A token's org is fixed at issuance,
//! so no tool accepts an org argument and no tool consults one from anywhere
//! else. [`Factory::tx`] opens a transaction pinned to it, which is the only
//! way `of-core` will hand out tenant data at all.
//!
//! ## What a tool body looks like
//!
//! Check the scope, open the transaction, do exactly one thing, commit. Metering
//! (milestone 1 task 9) writes its counters inside that same transaction, before
//! the commit — a failed call must not be billed, and a successful one must not
//! be billed twice, and only a shared transaction gives both.

use std::sync::Arc;

use of_billing::Meter;
use of_core::teams::TeamScope;
use of_core::watch::Watcher;
use otto_resource::PlatformClient;
use otto_tenant::{Db, Tx};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{
    ErrorData, Implementation, InitializeRequestParams, InitializeResult, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{tool_handler, RoleServer, ServerHandler};

use crate::auth::{MissingScope, Principal};
use crate::error;
use crate::legacy::Legacy;

/// What the MCP surface tells a client it is, in the client's own words.
///
/// Read by an LLM before it calls anything, so it is written as operating
/// guidance rather than marketing: the three things an agent gets wrong on its
/// first session are naming the repo, claiming without checking `ready`, and
/// polling instead of waiting.
const INSTRUCTIONS: &str = "\
otto-tower coordinates agentic coding work across a team. Everything here is \
anchored on repositories: a job belongs to a repo, a lease is taken on a resource \
within a repo — a branch (spelled `branch:<name>`, e.g. `branch:main`), or anything \
else your team needs to serialize on, such as a staging slot or a migration lock — \
and a tool that cannot work out which repo you mean will refuse rather than guess.

Getting started in a new session:
  1. Call whoami to see which organization this token opens and what you may do.
  2. Identify your repo once. Pass `remote` with the output of \
`git remote get-url origin`, or `repo` with a registered slug. If nothing \
matches, list_repos shows what is registered and register_repo adds this one.
  3. Call ready to see claimable work, claim_jobs to take it, and complete_job \
or fail_job when you are done. Never start work you have not claimed: claiming \
is what stops two agents doing the same job.

Before editing a branch (or starting anything else others could collide with), \
take a lease on it with acquire_lease and renew it while you work. Leases are \
advisory — the server cannot see what you actually do with the resource — so \
they make collisions visible rather than impossible. list_leases answers \
'who else is in this repo \
right now'.

Use watch instead of polling. It blocks until something in your organization \
changes and returns 'changed' or 'timeout'; call it again either way. Your own \
messages never wake you.

Jobs carry a free-form `metadata` object that this server never reads or \
interprets. Put whatever your own workflow needs in it.

Coordination calls are free. Each completed job is billed on the \
organization's otto-tower meter, including one completed by closing its \
linked ticket. Call usage to see this month's completed \
jobs, the budget and any block.";

#[derive(Debug, Clone, Default)]
pub struct TrackerSyncConfig {
    pub github_app_id: Option<i64>,
    pub github_app_private_key: Option<String>,
    /// Where GitHub's REST API is, when it is not `https://api.github.com`:
    /// a mock GitHub in tests. No environment variable sets it, so a
    /// deployment always talks to the real one.
    pub github_api_base: Option<String>,
    pub jira_client_id: Option<String>,
    pub jira_client_secret: Option<String>,
    pub encryption_key: Option<String>,
}

/// The service. Cheap to clone, because `rmcp` builds one per session.
#[derive(Clone)]
pub struct Factory {
    db: Db,
    watcher: Arc<Watcher>,
    platform: Arc<PlatformClient>,
    meter: Arc<Meter>,
    tracker_sync: Arc<TrackerSyncConfig>,
    tool_router: ToolRouter<Self>,
}

impl Factory {
    pub fn new(db: Db, watcher: Arc<Watcher>, platform: Arc<PlatformClient>, meter: Meter) -> Self {
        Self::new_with_tracker_sync(db, watcher, platform, meter, TrackerSyncConfig::default())
    }

    pub fn new_with_tracker_sync(
        db: Db,
        watcher: Arc<Watcher>,
        platform: Arc<PlatformClient>,
        meter: Meter,
        tracker_sync: TrackerSyncConfig,
    ) -> Self {
        Self {
            db,
            watcher,
            platform,
            meter: Arc::new(meter),
            tracker_sync: Arc::new(tracker_sync),
            tool_router: crate::tools::router(),
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The otto platform: identity lookups (members, teams) and billing.
    pub fn platform(&self) -> &Arc<PlatformClient> {
        &self.platform
    }

    pub fn watcher(&self) -> &Arc<Watcher> {
        &self.watcher
    }

    pub fn meter(&self) -> &Meter {
        &self.meter
    }

    pub fn tracker_sync(&self) -> &TrackerSyncConfig {
        &self.tracker_sync
    }

    /// Record this free call inside the tool's own transaction. Never refuses
    /// and never asks the platform anything: every tool is free, and the one
    /// billable outcome, a completed job, goes through
    /// [`Self::charge_completion`] instead.
    ///
    /// Called immediately after the transaction opens and **before** the tool
    /// does anything. That ordering is what makes "every call is recorded,
    /// and a failed one is not" true rather than aspirational: the usage row
    /// lives in the same transaction as the work, so a tool that returns an
    /// error rolls back the record along with everything else. Recording
    /// afterwards would need a second transaction, which can fail on its own,
    /// and a history that disagrees with what happened is worse than none.
    ///
    /// `sync_ticket` (`tools::jobs`) is a second exception, alongside
    /// `watch`: its "work" is an outbound call to the tracker that has
    /// already happened by the time any Tx opens, so recording in the same
    /// Tx as the DB write-back would let a failed record roll back
    /// loop-safety state (`remote_revision`, rotated credentials) for a
    /// tracker call that cannot be un-made. It writes back first, in its own
    /// Tx, then records in a second one, so the write-back survives whatever
    /// happens to the record and a retry does not re-post to the tracker.
    ///
    /// A third, different-shaped exception: `add_job`/`send_message`'s
    /// idempotent-replay check. Unlike `watch`/`sync_ticket` above, this one
    /// has no unrollbackable external effect to protect — it is a plain
    /// read (`Tx::find_replayed_job`/`find_replayed_message`) inside the
    /// same transaction `charge` would use. It still has to run before
    /// `charge`: charging first and un-charging afterward on a replay would
    /// need either a second transaction (the same fragility `sync_ticket`'s
    /// two-transaction shape above exists to avoid) or a savepoint wrapped
    /// around `charge` itself, which buys nothing over simply not calling it
    /// in the first place. A replay is still recorded, just never billed —
    /// see [`Self::record_replay`], `tools::jobs::add_job` /
    /// `tools::coord::send_message`, and the idempotency-key design spec's
    /// §5/§8. This guarantee is for a *replay* — a caller finding a
    /// key that already resolved to a row. It does not cover two concurrent
    /// callers racing the *same brand-new* key: both can reach `charge`
    /// before either has inserted, so both are billed even though only one
    /// row is ever created. §8 names that gap and the reasoning for
    /// accepting it rather than making every `add_job`/`send_message` call
    /// pay the cost of charging after its insert to close it.
    pub async fn charge(
        &self,
        tx: &mut Tx<'_>,
        caller: &Principal,
        tool: &str,
    ) -> Result<of_billing::Charge, ErrorData> {
        self.meter
            .charge(tx, caller.user_id, tool)
            .await
            .map_err(|e| error::from_billing(&e))
    }

    /// [`Self::charge`] for the one billed outcome, a job `complete_job` is
    /// about to complete: records the completion, billed, through
    /// `of_billing::record_job_completed` (the one place that decides what a
    /// completion writes), or refuses it (`budget_exceeded` or
    /// `payment_required`, nothing written) when the platform says the org's
    /// billable calls are blocked.
    ///
    /// Same placement rule as `charge`: in the tool's own transaction, before
    /// the work, so a failed call rolls it back. `warmed` is what
    /// [`Meter::warm`] returned *before* that transaction was opened, so the
    /// platform round trip never holds a pooled connection and this makes no
    /// network call. A lookup that failed allows the call (it is still
    /// recorded): a platform outage never blocks work.
    pub async fn charge_completion(
        &self,
        tx: &mut Tx<'_>,
        caller: &Principal,
        warmed: &of_billing::Warmed,
    ) -> Result<of_billing::Charge, ErrorData> {
        self.meter
            .charge_completion(tx, caller.user_id, warmed)
            .await
            .map_err(|e| error::from_billing(&e))
    }

    /// Record an idempotent replay of `tool` as `Free`, never billed and
    /// never refused — see [`Self::charge`]'s doc comment for why this has
    /// to run instead of, not before, `charge` on a replay. Called by
    /// `add_job`/`send_message` once `Tx::find_replayed_job`/
    /// `find_replayed_message` finds an existing match, so "record every
    /// call regardless of class" (`of_billing::classify`'s module doc)
    /// still holds even though nothing new was created.
    pub async fn record_replay(
        &self,
        tx: &mut Tx<'_>,
        caller: &Principal,
        tool: &str,
    ) -> Result<(), ErrorData> {
        self.meter
            .record_replay(tx, caller.user_id, tool)
            .await
            .map_err(|e| error::from_billing(&e))
    }

    /// The principal for the HTTP request this tool call arrived on.
    pub fn caller(&self, parts: &http::request::Parts) -> Result<Principal, ErrorData> {
        crate::auth::principal_from(parts).ok_or_else(error::unauthenticated)
    }

    /// Which team-scoped repos and jobs the caller may see or touch: everything
    /// for an owner or admin, otherwise org-wide rows plus those of the teams the
    /// platform says they belong to. **Fails closed**: a platform that cannot
    /// answer is `platform_unavailable`, never "all teams". Ask before opening
    /// the transaction, so a slow platform holds no pooled connection.
    pub async fn team_scope(&self, caller: &Principal) -> Result<TeamScope, ErrorData> {
        // The role on the principal is as of the last introspection, cached for
        // up to a minute: a demoted admin would keep full visibility. Ask the
        // platform who this is now (its own, shorter cache), as the console does.
        // A user it no longer knows as a member has left the org since the token
        // was introspected: refused, not downgraded to a member of no teams. A
        // platform that cannot answer is refused too.
        let role = self
            .platform
            .member(caller.org_id.as_uuid(), caller.user_id.as_uuid())
            .await
            .map_err(|e| error::from_platform(&e))?
            .ok_or_else(|| error::from_core(&of_core::Error::AccessRevoked))?
            .role;
        TeamScope::for_member(&self.platform, caller.org_id, caller.user_id, role)
            .await
            .map_err(|e| error::from_core(&e))
    }

    /// Open a transaction pinned to the caller's org, refused if the org was
    /// deleted or the caller removed since their token was introspected (see
    /// `of_core::platform_events::begin_live`).
    ///
    /// Asks the platform nothing about billing: free tools have nothing to ask.
    /// The metered tool warms its own status lookup before calling this.
    pub async fn tx(&self, caller: &Principal) -> Result<Tx<'static>, ErrorData> {
        of_core::platform_events::begin_live(&self.db, caller.org_id, Some(caller.user_id))
            .await
            .map_err(|e| error::from_core(&e))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Factory {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is #[non_exhaustive], so this is built by mutation
        // rather than a struct literal with `..`.
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        // Left unset, this defaults to `Implementation::from_build_env()`,
        // which expands `CARGO_CRATE_NAME`/`CARGO_PKG_VERSION` in `rmcp`'s
        // own build context — every client would see "rmcp" 2.0.0 at
        // `initialize`, not otto-tower.
        info.server_info = Implementation::new("otto-tower", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }

    /// `rmcp`'s default, plus the deprecation notice when the handshake came
    /// through a legacy host.
    ///
    /// The notice has to be decided here and not in `get_info`: `get_info`
    /// sees no request, and the host is a property of the request — the
    /// transport carries its `Parts` (with the [`Legacy`] marker the outermost
    /// layer put there) into this call's context. Instructions are what an agent
    /// actually reads, so this is where a move to the new address reaches it.
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        context.peer.set_peer_info(request);
        let legacy = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Legacy>());
        Ok(info_for(self.get_info(), legacy))
    }
}

/// The handshake answer for a request that did, or did not, come through a
/// legacy host: on one, the instructions open with the deprecation paragraph.
fn info_for(mut info: InitializeResult, legacy: Option<&Legacy>) -> InitializeResult {
    if let Some(legacy) = legacy {
        let notice = legacy.notice();
        info.instructions = Some(match info.instructions.take() {
            Some(rest) => format!("{notice}\n\n{rest}"),
            None => notice,
        });
    }
    info
}

/// Map an `of-core`, `otto-tenant`, or billing failure into the MCP envelope at
/// the point of the call.
///
/// An extension trait rather than `From` impls plus `?`, because both error
/// types and the target are foreign to this crate and the orphan rule forbids
/// the conversion. Explicit `.mcp()?` reads fine and, unlike a blanket
/// conversion, makes it visible at every call site that a domain error is
/// crossing into protocol space — which is exactly where the tenant-safety
/// review wants to look.
pub trait McpResult<T> {
    fn mcp(self) -> Result<T, ErrorData>;
}

impl<T> McpResult<T> for of_core::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_core(&e))
    }
}

impl<T> McpResult<T> for otto_tenant::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_tenant(&e))
    }
}

impl<T> McpResult<T> for Result<T, MissingScope> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_scope(&e))
    }
}

impl<T> McpResult<T> for otto_resource::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_platform(&e))
    }
}

impl<T> McpResult<T> for of_billing::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_billing(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instructions are the only documentation an agent gets. If they stop
    /// naming the tools an agent needs first, its opening moves get worse and
    /// nothing else in the test suite notices.
    #[test]
    fn the_instructions_name_the_opening_moves() {
        for tool in [
            "whoami",
            "list_repos",
            "register_repo",
            "ready",
            "claim_jobs",
            "complete_job",
            "acquire_lease",
            "watch",
            "usage",
        ] {
            assert!(
                INSTRUCTIONS.contains(tool),
                "the instructions should tell an agent about {tool}"
            );
        }
    }

    /// The deprecation paragraph opens the instructions over a legacy host and
    /// is absent otherwise; the operating guidance follows it unchanged.
    #[test]
    fn the_instructions_open_with_the_notice_only_over_a_legacy_host() {
        let base = || {
            let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
            info.instructions = Some(INSTRUCTIONS.to_string());
            info
        };

        let canonical = info_for(base(), None);
        assert_eq!(canonical.instructions.as_deref(), Some(INSTRUCTIONS));

        let deprecation = crate::legacy::Deprecation::new(
            &url::Url::parse("https://tower.example.com").unwrap(),
            &url::Url::parse("https://tower.example.com/mcp").unwrap(),
            ["factory.example.com"],
            None,
        );
        let mut headers = http::HeaderMap::new();
        headers.insert("x-forwarded-host", "factory.example.com".parse().unwrap());
        let legacy = deprecation
            .classify(&headers, &"/mcp".parse().unwrap())
            .expect("legacy");

        let instructions = info_for(base(), Some(&legacy))
            .instructions
            .expect("instructions");
        assert!(instructions.starts_with(&legacy.notice()), "{instructions}");
        assert!(instructions.ends_with(INSTRUCTIONS), "{instructions}");

        // With nothing to follow it, the notice stands alone: no dangling blank
        // lines.
        let mut bare = base();
        bare.instructions = None;
        assert_eq!(
            info_for(bare, Some(&legacy)).instructions,
            Some(legacy.notice())
        );
    }

    /// Constraint 3: coding-agent agnostic. The instructions must not name a
    /// specific client, or they become advice for one agent and noise for the
    /// rest.
    #[test]
    fn the_instructions_name_no_particular_agent() {
        let lowered = INSTRUCTIONS.to_lowercase();
        for client in ["claude", "copilot", "cursor", "codex", "gemini"] {
            assert!(
                !lowered.contains(client),
                "the instructions must stay client-neutral, but mention {client}"
            );
        }
    }
}
