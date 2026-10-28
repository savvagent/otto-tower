//! Coordination tools — leases, the message channel, and change notification.
//!
//! These are what make a queue into coordination. Leases answer "is anyone else
//! using this resource"; messages let agents hand off context a job field
//! cannot hold; `watch` lets an agent sit still until something happens
//! instead of asking every few seconds.

use of_core::jobs::JobsExt;
use of_core::leases::LeasesExt;
use of_core::messages::MessagesExt;
use std::time::Duration;

use of_core::messages::{ChannelRef, InboxQuery, MessageKind, NewMessage, SenderKind};
use of_core::repos::{Hold, ReposExt};
use of_core::watch::Outcome;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::{
    channel_arg, channel_of, ensure_job_visible, job_ref_for_write, maybe_repo_of, out,
    repo_for_write, scope, ChannelArg,
};
use crate::server::{Factory, McpResult};

/// Default long-poll duration, and the ceiling.
///
/// Thirty seconds is short enough to sit inside every load balancer and proxy
/// idle timeout that agents connect through, and long enough that an idle agent
/// makes two calls a minute rather than sixty. The ceiling is a minute for the
/// same reason: a longer poll is not more efficient if an intermediary silently
/// severs it and the agent has to reconnect anyway.
const WATCH_DEFAULT_SECS: u64 = 30;
const WATCH_MAX_SECS: u64 = 60;

/// Properties of the two constants above rather than of any code path, so they
/// are checked when the crate compiles. A poll longer than a minute is not more
/// efficient — it is severed by an intermediary and reconnected anyway.
const _: () = assert!(WATCH_DEFAULT_SECS <= WATCH_MAX_SECS);
const _: () = assert!(WATCH_MAX_SECS <= 60);

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcquireLeaseArgs {
    /// A free-form name for whatever you are taking exclusive use of — a
    /// branch, a staging slot, a migration lock, anything your team needs to
    /// serialize on. For the branch case, use the form `branch:<name>` (e.g.
    /// `branch:main`) so every caller converges on the same spelling. Pass
    /// exactly one of `resource` or `branch`, never both.
    #[serde(default)]
    pub resource: Option<String>,
    /// Deprecated shorthand for `resource: "branch:<branch>"`. Prefer
    /// `resource` directly; this is kept only so existing callers naming a
    /// branch keep working. Pass exactly one of `resource` or `branch`, never
    /// both — supplying both is refused rather than guessed.
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    /// How you want to appear to teammates who run list_leases, for example
    /// "api-agent@ci-7". At most 128 characters on one line, with no
    /// control, line-break, or invisible formatting characters; anything else
    /// is refused with invalid_agent_label rather than shortened. Blank is the
    /// same as omitting it.
    #[serde(default)]
    pub agent: Option<String>,
    /// The job this lease is for, if there is one. A job id like "job-42", or
    /// the job's issue as "owner/repo#928" (a short "#928" is refused here, as
    /// on every tool that changes a job: it can match an issue in any repo).
    #[serde(default)]
    pub job: Option<String>,
    /// How long to hold it before it expires. Defaults to 15 minutes, capped
    /// at 4 hours. Renew rather than asking for a long one: a lease that
    /// outlives a crashed agent blocks the resource for everyone.
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RenewLeaseArgs {
    /// The lease id returned by acquire_lease.
    pub lease: String,
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseLeaseArgs {
    /// The lease id returned by acquire_lease.
    pub lease: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoScopeArgs {
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendMessageArgs {
    /// What you want to say.
    pub body: String,
    /// "note" (default), "request" when you need an answer, or "response".
    #[serde(default)]
    pub kind: Option<String>,
    /// The repo whose channel this goes to, by registered slug. Pass this (or
    /// remote) or team, not both.
    #[serde(default)]
    pub repo: Option<String>,
    /// The repo whose channel this goes to, as `git remote get-url origin`
    /// printed it. Pass this (or repo) or team, not both.
    #[serde(default)]
    pub remote: Option<String>,
    /// A team's slug, for that team's channel across repos. Pass this or
    /// repo/remote, not both.
    #[serde(default)]
    pub team: Option<String>,
    /// The job this is about, if any. A job id like "job-42", or the job's
    /// issue as "owner/repo#928" (a short "#928" is refused here, as on every
    /// tool that changes a job: it can match an issue in any repo).
    #[serde(default)]
    pub job: Option<String>,
    /// The id of the message you are replying to.
    #[serde(default)]
    pub in_reply_to: Option<i64>,
    /// How you want to be identified, for example "api-agent@ci-7". At most
    /// 128 characters on one line, with no control, line-break, or invisible
    /// formatting characters; anything else is refused with
    /// invalid_agent_label rather than shortened. Blank is the same as
    /// omitting it.
    #[serde(default)]
    pub agent: Option<String>,
    /// A caller-chosen key. Replaying send_message with the same key and the
    /// same arguments returns the original message unchanged instead of
    /// posting a second one — call this every time if your connection to
    /// the server can drop between the call committing and its response
    /// arriving, which is the situation a retry cannot otherwise tell apart
    /// from "never happened". Reusing a key with different arguments is an
    /// error. Omit it and every call posts a new message, as today.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InboxArgs {
    /// The repo whose channel to read, by registered slug. Name exactly one
    /// channel: repo (or remote), or team.
    #[serde(default)]
    pub repo: Option<String>,
    /// The repo whose channel to read, as `git remote get-url origin` printed
    /// it.
    #[serde(default)]
    pub remote: Option<String>,
    /// A team's slug, for that team's channel. A team you are not on is
    /// team_not_found.
    #[serde(default)]
    pub team: Option<String>,
    /// Only messages you have not acknowledged and did not send. Defaults to
    /// true.
    #[serde(default = "default_true")]
    pub unread_only: bool,
    /// Maximum messages to return. Defaults to 50, capped at 200.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Newest first. Defaults to false, which is conversational order.
    #[serde(default)]
    pub newest_first: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AckMessagesArgs {
    /// The highest message id you have read in this channel: an id inbox
    /// returned for this same channel, or 0. Any other id (another channel's,
    /// one you cannot see, one that does not exist) is invalid_argument.
    pub up_to: i64,
    /// The repo whose channel to acknowledge, by registered slug. Name exactly one
    /// channel: repo (or remote), or team.
    #[serde(default)]
    pub repo: Option<String>,
    /// The repo whose channel to acknowledge, as `git remote get-url origin` printed
    /// it.
    #[serde(default)]
    pub remote: Option<String>,
    /// A team's slug, for that team's channel. A team you are not on is
    /// team_not_found.
    #[serde(default)]
    pub team: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChannelArgs {
    /// The repo whose channel to count, by registered slug. Name exactly one
    /// channel: repo (or remote), or team.
    #[serde(default)]
    pub repo: Option<String>,
    /// The repo whose channel to count, as `git remote get-url origin` printed
    /// it.
    #[serde(default)]
    pub remote: Option<String>,
    /// A team's slug, for that team's channel. A team you are not on is
    /// team_not_found.
    #[serde(default)]
    pub team: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchArgs {
    /// How long to wait before giving up and returning "timeout". Defaults to
    /// 30 seconds, capped at 60.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// Parse the caller's message kind.
fn message_kind(raw: Option<&str>) -> Result<MessageKind, ErrorData> {
    match raw.map(str::trim) {
        None | Some("") | Some("note") => Ok(MessageKind::Note),
        Some("request") => Ok(MessageKind::Request),
        Some("response") => Ok(MessageKind::Response),
        Some(other) => Err(ErrorData::invalid_params(
            format!("unknown message kind {other:?}; expected note, request or response"),
            None,
        )),
    }
}

#[tool_router(router = coord_router, vis = "pub(crate)")]
impl Factory {
    #[tool(
        name = "acquire_lease",
        description = "Announce that you are taking exclusive use of something in a \
                       repository — a branch, or anything else your team needs to serialize \
                       on, such as a staging slot or a migration lock — so other agents can \
                       see it and go elsewhere. Two agents in two different repositories can \
                       hold the same resource name independently; a lease only ever \
                       serializes within one repository. Pass `resource` as a free-form \
                       name; for a branch, use the form `branch:<name>` (e.g. `branch:main`) \
                       so every caller converges on the same spelling — a bare `main` and \
                       `branch:main` are different resources. `branch` is accepted as a \
                       deprecated shorthand for `resource: \"branch:<branch>\"`; pass one or \
                       the other, never both. Take one before you start and renew it while \
                       you work. If someone already holds it the error names them and says \
                       when it expires, so you can wait, message them, or pick different \
                       work. Leases are advisory: the server cannot see what you actually do \
                       with the resource, so this makes collisions visible rather than \
                       impossible. agent is a short label (one line, at most 128 visible \
                       characters) that teammates see as the lease's holder; a longer or \
                       multi-line one is refused with invalid_agent_label rather than \
                       shortened."
    )]
    pub async fn acquire_lease(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<AcquireLeaseArgs>,
    ) -> Result<Json<out::LeaseOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        // A blank string and an absent field are the same request ("I gave you
        // nothing usable"), so both are folded to `None` before matching —
        // otherwise `resource: Some("")` would mask a perfectly good `branch`
        // instead of falling back to it, and would reach `of-core`'s guard only
        // after this handler had already opened a transaction and charged the
        // meter for a call that was always going to fail.
        let resource = args
            .resource
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        let branch = args
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty());

        let resource = match (resource, branch) {
            // Both given is refused rather than guessed: `resource` silently
            // winning would mean a caller who meant the branch alias (and got
            // the resource field populated by, say, a stale default) leases the
            // wrong thing with no error — "errors that guess are worse than
            // errors that stop."
            (Some(_), Some(_)) => {
                return Err(ErrorData::invalid_params(
                    "acquire_lease got both resource and the deprecated branch; pass only \
                     one (resource, or branch as a shorthand for \"branch:<branch>\")",
                    None,
                ))
            }
            (Some(r), None) => r.to_string(),
            (None, Some(b)) => format!("branch:{b}"),
            (None, None) => {
                return Err(ErrorData::invalid_params(
                    "acquire_lease needs resource (or the deprecated branch)",
                    None,
                ))
            }
        };

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "acquire_lease").await?;
        let repo = repo_for_write(&mut tx, &team, args.repo, args.remote, Hold::Shared).await?;
        let job = job_arg(&mut tx, &team, args.job.as_deref()).await?;
        let mut lease = tx
            .acquire_lease(
                repo.id,
                &resource,
                caller.user_id,
                args.agent.as_deref(),
                job.as_ref(),
                args.ttl_seconds,
            )
            .await
            .mcp()?;
        // Acquiring a resource this caller already holds renews the existing
        // lease, which can name a job they have since lost sight of.
        hide_unseen_job(&mut tx, &team, &mut lease).await?;
        tx.commit().await.mcp()?;

        Ok(Json(out::LeaseOut { lease }))
    }

    #[tool(
        name = "renew_lease",
        description = "Extend a lease you hold, before it expires. Renew on a cadence \
                       comfortably shorter than the TTL: if it lapses, another agent may take \
                       the resource while you are still in it."
    )]
    pub async fn renew_lease(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RenewLeaseArgs>,
    ) -> Result<Json<out::LeaseOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let id = lease_id(&args.lease)?;
        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "renew_lease").await?;
        ensure_lease_visible(&mut tx, &team, id).await?;
        let mut lease = tx
            .renew_lease(id, caller.user_id, args.ttl_seconds)
            .await
            .mcp()?;
        hide_unseen_job(&mut tx, &team, &mut lease).await?;
        tx.commit().await.mcp()?;

        Ok(Json(out::LeaseOut { lease }))
    }

    #[tool(
        name = "release_lease",
        description = "Give up a lease you hold, freeing the resource immediately instead of \
                       waiting for it to expire. Do this as soon as you stop working."
    )]
    pub async fn release_lease(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ReleaseLeaseArgs>,
    ) -> Result<Json<out::ReleasedOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let id = lease_id(&args.lease)?;
        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "release_lease").await?;
        ensure_lease_visible(&mut tx, &team, id).await?;
        tx.release_lease(id, caller.user_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::ReleasedOut { released: id }))
    }

    #[tool(
        name = "list_leases",
        description = "Who is working where right now: the live leases across the organization \
                       or one repository, with the holder, the resource, and when each expires. \
                       Expired leases are not listed."
    )]
    pub async fn list_leases(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::LeasesOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let team = self.team_scope(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "list_leases").await?;
        let repo_id = maybe_repo_of(&mut tx, &team, args.repo, args.remote).await?;
        let mut leases = tx.list_leases(repo_id).await.mcp()?;
        if !team.is_all() {
            // Leases hang off repos: drop those of repos the caller cannot see.
            let hidden = tx.hidden_repo_ids(&team).await.mcp()?;
            leases.retain(|l| !hidden.contains(&l.repo_id));
            of_core::leases::hide_unseen_jobs(&mut tx, &mut leases, &team)
                .await
                .mcp()?;
        }
        tx.commit().await.mcp()?;

        Ok(Json(out::LeasesOut { leases }))
    }

    #[tool(
        name = "send_message",
        description = "Post a note to one repo's or one team's shared channel: pass repo (or \
                       remote) for coordination about that repo, or team (a team slug) for a \
                       team's work across repos. Exactly one is required; neither or both is \
                       invalid_argument, and an unknown team or one you are not on is \
                       team_not_found. Use it \
                       for hand-offs and questions that do not fit in a job field — 'I have \
                       left the migration half-applied on this branch' is exactly the kind of \
                       thing that belongs here. Pass idempotencyKey if your connection can drop \
                       before you see the response, so a retry returns the original message \
                       instead of posting a duplicate. agent is a short label (one line, at \
                       most 128 visible characters) shown to readers as the sender; a longer or \
                       multi-line one is refused with invalid_agent_label rather than \
                       shortened."
    )]
    pub async fn send_message(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SendMessageArgs>,
    ) -> Result<Json<out::MessageOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::MESSAGES).mcp()?;

        let kind = message_kind(args.kind.as_deref())?;

        // Both platform calls (the scope, and a team channel's slug) before the
        // transaction opens, so a slow platform holds no pooled connection.
        let team = self.team_scope(&caller).await?;
        let arg = channel_arg(self, &caller, &team, args.repo, args.remote, args.team).await?;
        let team_id = match &arg {
            ChannelArg::Team(t) => Some(*t),
            ChannelArg::Repo { .. } => None,
        };
        let mut tx = self.tx(&caller).await?;

        // No key: reproduce today's behavior exactly, charging as the
        // literal first thing after the transaction opens — see add_job's
        // identical comment (tools::jobs) and Factory::charge's doc comment.
        let Some(key) = args.idempotency_key else {
            self.charge(&mut tx, &caller, "send_message").await?;
            let repo_id = repo_id_of(channel_of(&mut tx, &team, arg, true).await?);
            let job_id = job_arg(&mut tx, &team, args.job.as_deref()).await?;
            if let Some(parent) = args.in_reply_to {
                tx.reply_parent(parent, &team).await.mcp()?;
            }
            let message = tx
                .send_message(
                    caller.user_id,
                    NewMessage {
                        body: args.body,
                        team_id,
                        kind,
                        // Always `Agent` from this surface. A human writing
                        // in the console is the same user authenticating the
                        // same way, so this is a rendering hint and never an
                        // authorization claim — which is why it is set here
                        // rather than accepted from the caller.
                        sender_kind: SenderKind::Agent,
                        sender_label: args.agent,
                        repo_id,
                        job_id,
                        in_reply_to: args.in_reply_to,
                        ..Default::default()
                    },
                )
                .await
                .mcp()?;
            tx.commit().await.mcp()?;
            return Ok(Json(out::MessageOut { message }));
        };

        // A key was supplied: replay-vs-new must be resolved before
        // charging, which needs the channel to build the payload to check.
        let repo_id = repo_id_of(channel_of(&mut tx, &team, arg, true).await?);
        // Resolved before the fingerprint is taken, so a replay that names the
        // job by its issue once and by its id the next still replays.
        // A replay is re-resolved on every call: if a second job has since
        // linked the same issue, a retry that named the job as "#N" is refused
        // as ambiguous_ticket_ref rather than replayed. That is acceptable,
        // because the reference no longer names one job.
        let job_id = job_arg(&mut tx, &team, args.job.as_deref()).await?;
        if let Some(parent) = args.in_reply_to {
            tx.reply_parent(parent, &team).await.mcp()?;
        }
        let new_message = NewMessage {
            body: args.body,
            team_id,
            kind,
            sender_kind: SenderKind::Agent,
            sender_label: args.agent,
            repo_id,
            job_id,
            in_reply_to: args.in_reply_to,
            idempotency_key: Some(key),
        };

        if let Some(existing) = tx
            .find_replayed_message(caller.user_id, &new_message)
            .await
            .mcp()?
        {
            // The original is the caller's own, but what it is tied to may have
            // moved out of their teams since: not theirs to read back now.
            if !tx.message_readable(existing.id, &team).await.mcp()? {
                // Not `*_not_found`, which would invite a retry with the same
                // key: the original exists, so the key is spent.
                return Err(of_core::Error::Redacted {
                    code: "idempotency_key_conflict",
                    message: "that idempotency key already sent a message you can no longer \
                              see (what it was tied to has moved to a team you are not on); \
                              use a new key",
                })
                .mcp();
            }
            self.record_replay(&mut tx, &caller, "send_message").await?;
            tx.commit().await.mcp()?;
            return Ok(Json(out::MessageOut { message: existing }));
        }

        self.charge(&mut tx, &caller, "send_message").await?;
        let message = tx.send_message(caller.user_id, new_message).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::MessageOut { message }))
    }

    #[tool(
        name = "inbox",
        description = "Read the messages in a repo's or a team's channel. One channel at a \
                       time, named the same way as send_message (repo, remote, or team): \
                       exactly one is required, and neither or both is invalid_argument. An \
                       unknown team or one you are not on is team_not_found. Defaults to \
                       unread only, excluding your own. Call ack_messages for the same channel \
                       with the highest id you have read so the next call does not repeat \
                       them."
    )]
    pub async fn inbox(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<InboxArgs>,
    ) -> Result<Json<out::MessagesOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::MESSAGES).mcp()?;

        let team = self.team_scope(&caller).await?;
        let arg = channel_arg(self, &caller, &team, args.repo, args.remote, args.team).await?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "inbox").await?;
        let q = InboxQuery {
            channel: channel_of(&mut tx, &team, arg, false).await?,
            visible_teams: team.restriction(),
            unread_only: args.unread_only,
            limit: args.limit.unwrap_or(50),
            newest_first: args.newest_first,
        };
        let messages = tx.inbox(caller.user_id, &q).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::MessagesOut { messages }))
    }

    #[tool(
        name = "ack_messages",
        description = "Move your read cursor for one channel to a message id, so those messages \
                       stop appearing in that channel's unread inbox. One channel at a time, \
                       named the same way as send_message (repo, remote, or team): exactly one \
                       is required, and neither or both is invalid_argument. An unknown team \
                       or one you are not on is team_not_found. Each channel has its own cursor, so acking \
                       one never marks another read. up_to must be an id inbox returned for \
                       this same channel (or 0); any other id, including another channel's \
                       or one that does not exist, is invalid_argument and moves nothing. \
                       The cursor never moves backwards; returns where it landed."
    )]
    pub async fn ack_messages(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<AckMessagesArgs>,
    ) -> Result<Json<out::CursorOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::MESSAGES).mcp()?;

        let team = self.team_scope(&caller).await?;
        let arg = channel_arg(self, &caller, &team, args.repo, args.remote, args.team).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "ack_messages").await?;
        let channel = channel_of(&mut tx, &team, arg, false).await?;
        let cursor = tx
            .ack_messages_for(caller.user_id, channel, args.up_to, &team)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::CursorOut { cursor }))
    }

    #[tool(
        name = "unread_count",
        description = "How many messages are waiting for you in one channel. One channel at a \
                       time, named the same way as send_message (repo, remote, or team): \
                       exactly one is required, and neither or both is invalid_argument. An \
                       unknown team or one you are not on is team_not_found. Cheap for a repo channel; a team channel \
                       also costs one lookup at the platform to resolve the team. Call it \
                       before deciding whether reading that channel's inbox is worth it."
    )]
    pub async fn unread_count(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ChannelArgs>,
    ) -> Result<Json<out::UnreadOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::MESSAGES).mcp()?;

        let team = self.team_scope(&caller).await?;
        let arg = channel_arg(self, &caller, &team, args.repo, args.remote, args.team).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "unread_count").await?;
        let channel = channel_of(&mut tx, &team, arg, false).await?;
        let unread = tx
            .unread_count_for(caller.user_id, channel, &team)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::UnreadOut { unread }))
    }

    #[tool(
        name = "watch",
        description = "Block until something changes in your organization — a job, a lease, or \
                       a message — then return \"changed\". Returns \"timeout\" if nothing \
                       happens first. Use this instead of calling list_jobs in a loop: it is \
                       the difference between reacting in a second and hammering the server. \
                       Messages you sent yourself never wake you. It tells you only that \
                       something changed, so refetch what you care about afterwards."
    )]
    pub async fn watch(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<WatchArgs>,
    ) -> Result<Json<out::WatchOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let secs = args
            .timeout_seconds
            .unwrap_or(WATCH_DEFAULT_SECS)
            .clamp(1, WATCH_MAX_SECS);

        // Metered in a transaction of its own, committed before the wait
        // starts. Every other tool records inside the transaction doing the
        // work, which is what makes a failed call unbilled — but this call's
        // "work" is to sit still for thirty seconds, and holding a transaction
        // open for that long would pin a pool connection per idle agent and
        // exhaust the pool long before the queue was ever busy. `watch` is
        // free, so there is no billing consequence to recording it up front.
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "watch").await?;
        tx.commit().await.mcp()?;

        // Timed, not assumed: on a wake-up the caller waited less than the
        // budget, and reporting the budget back tells an agent pacing itself
        // that a queue which answered in two seconds took thirty.
        let started = std::time::Instant::now();
        let outcome = self
            .watcher()
            .wait(
                caller.org_id,
                Some(caller.user_id),
                Duration::from_secs(secs),
            )
            .await;

        Ok(Json(out::WatchOut {
            outcome: match outcome {
                Outcome::Changed => out::WatchOutcome::Changed,
                Outcome::Timeout => out::WatchOutcome::Timeout,
            },
            waited_seconds: match outcome {
                // Measured, because a wake-up can happen at any point and the
                // caller needs the real number to pace itself.
                Outcome::Changed => started.elapsed().as_secs(),
                // The budget, not the measurement. A timeout waited the whole
                // allowance by definition, and `as_secs()` truncates — 29.998s
                // would be reported as 29 and read as "there is a second of
                // headroom left", which is how a polling loop drifts faster
                // than it was told to.
                Outcome::Timeout => secs,
            },
        }))
    }
}

/// A lease of a repo the caller cannot see is `lease_not_held`, the same answer
/// as for a lease that does not exist, so a lease id confirms nothing and cannot
/// be acted on after leaving the team.
async fn ensure_lease_visible(
    tx: &mut otto_tenant::Tx<'_>,
    team: &of_core::teams::TeamScope,
    id: uuid::Uuid,
) -> Result<(), ErrorData> {
    if team.is_all() {
        return Ok(());
    }
    if let Some(repo) = tx.lease_repo_id(id).await.mcp()? {
        // Held, like any write's repo: the renew or release that follows must
        // not land after the repo has moved out of the caller's teams.
        if !tx.hold_repo_visible(repo, team, Hold::Shared).await.mcp()? {
            return Err(of_core::Error::LeaseNotHeld(id.to_string())).mcp();
        }
    }
    Ok(())
}

/// The repo a message's channel names, for `NewMessage::repo_id`; a team
/// channel's message carries no repo.
fn repo_id_of(channel: ChannelRef) -> Option<of_core::ids::RepoId> {
    match channel {
        ChannelRef::Repo(r) => Some(r),
        ChannelRef::Team(_) => None,
    }
}

/// An optional job argument, resolved (an id or a full issue reference, as
/// for any write) and checked visible with its repo held, since every caller
/// goes on to write.
async fn job_arg(
    tx: &mut otto_tenant::Tx<'_>,
    team: &of_core::teams::TeamScope,
    raw: Option<&str>,
) -> Result<Option<of_core::ids::JobId>, ErrorData> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let job = job_ref_for_write(tx, team, raw).await?;
    ensure_job_visible(tx, team, &job).await?;
    Ok(Some(job))
}

/// Blank a returned lease's `job_id` if the caller cannot see that job, as
/// `list_leases` does for every lease it returns.
async fn hide_unseen_job(
    tx: &mut otto_tenant::Tx<'_>,
    team: &of_core::teams::TeamScope,
    lease: &mut of_core::leases::Lease,
) -> Result<(), ErrorData> {
    of_core::leases::hide_unseen_jobs(tx, std::slice::from_mut(lease), team)
        .await
        .mcp()
}

/// Parse a lease id, refusing anything that is not a UUID before it reaches a
/// query.
fn lease_id(raw: &str) -> Result<uuid::Uuid, ErrorData> {
    raw.trim().parse().map_err(|_| {
        ErrorData::invalid_params(
            format!("{raw:?} is not a lease id; pass the id returned by acquire_lease"),
            None,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use of_core::leases::MAX_TTL_SECS;

    #[test]
    fn message_kinds_are_parsed_and_bad_ones_named() {
        assert_eq!(message_kind(None).unwrap(), MessageKind::Note);
        assert_eq!(message_kind(Some("")).unwrap(), MessageKind::Note);
        assert_eq!(message_kind(Some("request")).unwrap(), MessageKind::Request);
        assert_eq!(
            message_kind(Some("response")).unwrap(),
            MessageKind::Response
        );

        let err = message_kind(Some("urgent")).unwrap_err();
        assert!(err.message.contains("urgent"));
        assert!(
            err.message.contains("note"),
            "the error should list the valid kinds"
        );
    }

    /// A malformed lease id must be refused with something an agent can act on,
    /// not passed to the database to fail there.
    #[test]
    fn lease_ids_must_be_uuids() {
        assert!(lease_id("not-a-uuid").is_err());
        let err = lease_id("job-4").unwrap_err();
        assert!(err.message.contains("acquire_lease"));
        assert!(lease_id("  0192f0c8-0000-7000-8000-000000000000  ").is_ok());
    }

    /// The advertised ceiling has to stay inside what `of-core` will actually
    /// grant, or the tool documents a TTL the queue silently clamps away.
    #[test]
    fn the_documented_lease_ceiling_matches_the_domain() {
        assert_eq!(MAX_TTL_SECS, 4 * 3600, "the description says 4 hours");
    }
}
