//! The shared agent-to-agent message channel.
//!
//! Coordination chatter, not a payload transport: "taking the auth refactor,
//! stay out of crates/auth", "job-42 is blocked on the migration, who owns it?".
//! Bodies are bounded because every unread message is re-served on every inbox
//! read by every member — an oversized body is paid for many times over.

use crate::error::{Error, Result};
use crate::ids::{JobId, RepoId};
use crate::teams::TeamScope;
use savvagent_tenant::ids::{OrgId, TeamId, UserId};
use savvagent_tenant::Tx;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Upper bound on a message body, in bytes.
///
/// Bytes rather than chars because the point is the storage and transfer cost,
/// and a multi-byte body can only be shorter in chars, never longer in bytes.
/// 16 KiB is several times the longest plausible hand-off note.
pub const MAX_BODY_LEN: usize = 16 * 1024;

/// Cap on one inbox read, so no caller can force an unbounded read out of a
/// shared server by asking for a huge limit.
pub const INBOX_LIMIT_MAX: i64 = 200;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    sqlx::Type,
    schemars::JsonSchema,
)]
#[sqlx(type_name = "message_kind", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    #[default]
    Note,
    Request,
    Response,
}

/// Who typed a message.
///
/// A rendering hint, not a security claim: a human in the console and their
/// agent session authenticate as the same user, so this cannot be used to
/// distinguish them for authorization. The authoritative `sender_user_id` is
/// always set server-side from the authenticated principal and is never accepted
/// from the client.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    sqlx::Type,
    schemars::JsonSchema,
)]
#[sqlx(type_name = "sender_kind", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SenderKind {
    #[default]
    Agent,
    Human,
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: i64,
    pub org_id: OrgId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub sender_user_id: UserId,
    #[serde(serialize_with = "crate::agent_label::serialize_stored")]
    pub sender_label: Option<String>,
    pub sender_kind: SenderKind,
    pub team_id: Option<TeamId>,
    pub kind: MessageKind,
    pub body: String,
    pub repo_id: Option<RepoId>,
    pub job_id: Option<String>,
    pub in_reply_to: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct NewMessage {
    pub body: String,
    /// The channel: exactly one of `team_id` (that team's channel) and
    /// `repo_id` (that repo's) must be set; `send_message` refuses neither and
    /// both.
    pub team_id: Option<crate::teams::VerifiedTeam>,
    pub kind: MessageKind,
    pub sender_kind: SenderKind,
    pub sender_label: Option<String>,
    pub repo_id: Option<RepoId>,
    pub job_id: Option<JobId>,
    pub in_reply_to: Option<i64>,
    /// Caller-supplied replay key. `None` (the default) reproduces today's
    /// behavior exactly. See `jobs::NewJob::idempotency_key` — the identical
    /// shape, applied here.
    pub idempotency_key: Option<String>,
}

/// One message channel: a repo's or a team's. Every message is in exactly one,
/// and every read, count, and cursor is of exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRef {
    Repo(RepoId),
    Team(TeamId),
}

/// SQL: message `m` is in channel `c`, compared against the value bound as
/// `$n`, and that value. The column is chosen here, in Rust, between two fixed
/// strings and never built from input; the same reason `inbox` picks its
/// `ORDER BY` direction from two literals rather than binding it.
///
/// Exclusive: a repo channel's message has no team and a team channel's has no
/// repo. `messages_channel_ck` is `NOT VALID`, so a legacy row naming both can
/// still exist; it belongs to neither channel, and matching on one column
/// alone would serve it in both.
fn channel_sql(c: ChannelRef, n: usize) -> (String, uuid::Uuid) {
    match c {
        ChannelRef::Repo(r) => (
            format!("(m.repo_id = ${n} AND m.team_id IS NULL)"),
            r.as_uuid(),
        ),
        ChannelRef::Team(t) => (
            format!("(m.team_id = ${n} AND m.repo_id IS NULL)"),
            t.as_uuid(),
        ),
    }
}

/// [`channel_sql`]'s twin for an unaliased `message_cursors` row, bound to the
/// same `$n`. A cursor is per channel, so a read that does not key on the
/// channel would get several rows back once a member has read two.
fn cursor_channel_sql(c: ChannelRef, n: usize) -> String {
    match c {
        ChannelRef::Repo(_) => format!("repo_id = ${n}"),
        ChannelRef::Team(_) => format!("team_id = ${n}"),
    }
}

#[derive(Debug, Clone)]
pub struct InboxQuery {
    /// The one channel to read.
    pub channel: ChannelRef,
    /// Only messages past my cursor in this channel that I did not send.
    pub unread_only: bool,
    pub limit: i64,
    /// Newest first, so a limit-capped read keeps the most recent messages
    /// rather than the oldest. False is oldest first, conversational order.
    pub newest_first: bool,
    /// The reader's team visibility: when set, messages tied to a team, repo, or
    /// job outside it are left out. `None` is unrestricted.
    pub visible_teams: Option<Vec<TeamId>>,
}

/// SQL: message `m` is visible under the `uuid[]` bound as `$n` (NULL = all).
/// A message tied to a team, a repo, or a job the reader cannot see is not
/// theirs to read, whatever channel it is in. `scope_teams` is what it was
/// tied to when sent, which outlives a deleted repo or job (see
/// `0004_message_scope.sql`); the live checks catch a repo moved since.
fn message_visible_sql(n: usize) -> String {
    format!(
        "(${n}::uuid[] IS NULL OR (m.scope_teams <@ ${n}::uuid[] \
           AND (m.team_id IS NULL OR m.team_id = ANY(${n})) \
           AND (m.repo_id IS NULL OR NOT EXISTS (SELECT 1 FROM repos vr \
                 WHERE vr.org_id = m.org_id AND vr.id = m.repo_id \
                   AND vr.team_id IS NOT NULL AND NOT vr.team_id = ANY(${n}))) \
           AND (m.job_id IS NULL OR NOT EXISTS (SELECT 1 FROM jobs vj \
                 JOIN repos vr ON vr.org_id = vj.org_id AND vr.id = vj.repo_id \
                 WHERE vj.org_id = m.org_id AND vj.id = m.job_id \
                   AND ((vj.team_id IS NOT NULL AND NOT vj.team_id = ANY(${n})) \
                     OR (vr.team_id IS NOT NULL AND NOT vr.team_id = ANY(${n})))))))"
    )
}

fn team_uuids(teams: Option<&Vec<TeamId>>) -> Option<Vec<uuid::Uuid>> {
    teams.map(|v| v.iter().map(|t| t.as_uuid()).collect())
}

/// SQL: the `scope_teams` of a message being inserted, from the insert's own
/// parameters — `$1` org, `$5` team, `$8` repo, `$9` job, `$10` the message it
/// replies to. Every team it is bound to, and its thread's: a reply posted
/// without a repo or job of its own is still part of a team's conversation.
const SCOPE_TEAMS_SQL: &str = "ARRAY(SELECT DISTINCT t FROM unnest(ARRAY[$5::uuid, \
       (SELECT r.team_id FROM repos r WHERE r.org_id = $1 AND r.id = $8), \
       (SELECT j.team_id FROM jobs j WHERE j.org_id = $1 AND j.id = $9), \
       (SELECT r.team_id FROM jobs j JOIN repos r ON r.org_id = j.org_id AND r.id = j.repo_id \
         WHERE j.org_id = $1 AND j.id = $9)] \
     || COALESCE((SELECT p.scope_teams FROM messages p WHERE p.org_id = $1 AND p.id = $10), \
                 '{}'::uuid[])) AS t WHERE t IS NOT NULL)";

const MSG_COLS: &str = "id, org_id, created_at, sender_user_id, sender_label, sender_kind, \
                        team_id, kind, body, repo_id, job_id, in_reply_to";

/// Fingerprint the fields that define "the same `send_message` call" — see
/// the `idempotency` module doc and `jobs::job_idempotency_fingerprint`,
/// which this mirrors. `sender` is threaded through separately (it is a
/// `send_message` parameter, not a `NewMessage` field) because two different
/// callers reusing the same key is a materially different request, not a
/// replay of each other's.
fn message_idempotency_fingerprint(sender: UserId, new: &NewMessage) -> Vec<u8> {
    let NewMessage {
        body,
        team_id,
        kind,
        sender_kind,
        sender_label,
        repo_id,
        job_id,
        in_reply_to,
        // The key names the replay lookup; it is never part of what makes
        // two calls "the same call".
        idempotency_key: _,
    } = new;

    crate::idempotency::fingerprint(&serde_json::json!({
        "sender": sender,
        "body": body.trim(),
        // Messages are no longer addressed to a person, but the key stays,
        // always null: every stored keyed message hashed it, so dropping it
        // would turn each pre-migration keyed retry into
        // idempotency_key_conflict — the same precedent as the label policy
        // (#163) keeping the label hashed as sent.
        "recipientUserId": serde_json::Value::Null,
        "teamId": team_id.map(|t| t.id()),
        "kind": kind,
        "senderKind": sender_kind,
        "senderLabel": sender_label,
        "repoId": repo_id,
        "jobId": job_id.as_ref().map(|j| j.0.as_str()),
        "inReplyTo": in_reply_to,
    }))
}

/// Extension methods on [`Tx`] for this module's domain (see the crate docs for why
/// these are extension traits rather than inherent methods).
pub trait MessagesExt {
    /// Resolve `new.idempotency_key` against an already-completed
    /// `send_message` call, doing no writes and touching no meter. See
    /// `jobs::Tx::find_replayed_job` — the identical shape.
    fn find_replayed_message(
        &mut self,
        sender: UserId,
        new: &NewMessage,
    ) -> impl std::future::Future<Output = Result<Option<Message>>> + Send;

    fn send_message(
        &mut self,
        sender: UserId,
        new: NewMessage,
    ) -> impl std::future::Future<Output = Result<Message>> + Send;

    /// Messages in one channel (`q.channel`) visible to `reader`.
    fn inbox(
        &mut self,
        reader: UserId,
        q: &InboxQuery,
    ) -> impl std::future::Future<Output = Result<Vec<Message>>> + Send;

    /// Advance `reader`'s read cursor for `channel` to `up_to`.
    ///
    /// `up_to` must be 0 or the id of a message in `channel` that `scope` may
    /// see. Ids are global and cursors are per channel, so another channel's id
    /// (or a made-up large one) would silently mark this whole channel read,
    /// and would suppress messages not yet written; it is refused instead.
    /// A message that does not exist and one the reader cannot see are the
    /// same [`Error::Invalid`], so an ack cannot probe for hidden ids. The
    /// cursor never moves backwards; returns where it landed.
    fn ack_messages_for(
        &mut self,
        reader: UserId,
        channel: ChannelRef,
        up_to: i64,
        scope: &TeamScope,
    ) -> impl std::future::Future<Output = Result<i64>> + Send;

    /// Whether message `id` exists and `scope` covers every team it is tied to.
    /// A message is in a shared channel, so nothing about who sent it matters.
    fn message_readable(
        &mut self,
        id: i64,
        scope: &TeamScope,
    ) -> impl std::future::Future<Output = Result<bool>> + Send;

    /// Refuse an `in_reply_to` the replier could not read. A message that does
    /// not exist and one tied to a team, repo, or job outside `scope` are the
    /// same [`Error::Invalid`], so a reply cannot be used to probe for hidden
    /// message ids. The parent may be in another channel: the reply inherits
    /// its `scope_teams`, so that cannot widen who sees anything.
    ///
    /// For a restricted `scope` the repos behind the parent (its own and its
    /// job's) are held until the transaction ends, as any write's repo is: the
    /// reply copies the parent's scope when it is inserted, and an admin moving
    /// one of those repos in between would leave the reply readable by a team
    /// that can no longer read the parent.
    fn reply_parent(
        &mut self,
        parent: i64,
        scope: &TeamScope,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Unread messages in `channel` that `scope` may see and `reader` did not
    /// send.
    fn unread_count_for(
        &mut self,
        reader: UserId,
        channel: ChannelRef,
        scope: &TeamScope,
    ) -> impl std::future::Future<Output = Result<i64>> + Send;
}

impl MessagesExt for Tx<'_> {
    async fn find_replayed_message(
        &mut self,
        sender: UserId,
        new: &NewMessage,
    ) -> Result<Option<Message>> {
        let Some(key) = new.idempotency_key.as_deref() else {
            return Ok(None);
        };
        crate::idempotency::validate(key)?;
        // Bound the body before it is ever hashed, not just before it is
        // ever stored: without this, a caller replaying an already-used key
        // with an oversized body would pay for a full SHA-256 over it on
        // every retry, on a path that runs before metering.
        let body = new.body.trim();
        if body.is_empty() {
            return Err(Error::Invalid("message body must not be empty".into()));
        }
        if body.len() > MAX_BODY_LEN {
            return Err(Error::Invalid(format!(
                "message body is {} bytes; the limit is {MAX_BODY_LEN}",
                body.len()
            )));
        }
        let org = self.org();
        let existing: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT id, idempotency_payload_hash FROM messages \
             WHERE org_id = $1 AND idempotency_key = $2",
        )
        .bind(org)
        .bind(key)
        .fetch_optional(self.conn())
        .await?;
        let Some((id, stored_hash)) = existing else {
            // Nothing to replay, so this call will insert: the agent label
            // policy (#163) applies here, before the caller is metered, with
            // the same refusal `send_message` itself gives.
            crate::agent_label::validate(new.sender_label.as_deref())?;
            return Ok(None);
        };

        // Deliberately not label-validated: a byte-for-byte retry of a call
        // that committed before the policy existed must still get its original
        // message back, not a refusal for a write that already happened. What
        // it returns is safe to hand over — the label serializes through
        // `agent_label::serialize_stored`, so a non-conforming one comes back
        // as null.
        if stored_hash != message_idempotency_fingerprint(sender, new) {
            return Err(Error::IdempotencyKeyConflict {
                key: key.to_string(),
                tool: "send_message",
            });
        }

        let msg = sqlx::query_as(&format!(
            "SELECT {MSG_COLS} FROM messages WHERE org_id = $1 AND id = $2"
        ))
        .bind(org)
        .bind(id)
        .fetch_one(self.conn())
        .await?;
        Ok(Some(msg))
    }

    async fn send_message(&mut self, sender: UserId, new: NewMessage) -> Result<Message> {
        let body = new.body.trim();
        if body.is_empty() {
            return Err(Error::Invalid("message body must not be empty".into()));
        }
        if body.len() > MAX_BODY_LEN {
            return Err(Error::Invalid(format!(
                "message body is {} bytes; the limit is {MAX_BODY_LEN}",
                body.len()
            )));
        }
        // Exactly one channel. The schema's `messages_channel_ck` says the same,
        // but a constraint violation tells a caller nothing about what to send
        // instead.
        match (new.repo_id, new.team_id) {
            (Some(_), None) | (None, Some(_)) => {}
            (None, None) => {
                return Err(Error::Invalid(
                    "a message goes to one channel: set repo_id for a repo's channel or \
                     team_id for a team's"
                        .into(),
                ))
            }
            (Some(_), Some(_)) => {
                return Err(Error::Invalid(
                    "a message goes to one channel, a repo or a team, not both".into(),
                ))
            }
        }
        if let Some(key) = new.idempotency_key.as_deref() {
            crate::idempotency::validate(key)?;
        }
        // Readers see the label beside the body (#163). The normalized
        // form is what is stored, but the idempotency fingerprint below still
        // hashes the label exactly as sent — unchanged from before this
        // policy, so keyed messages stored earlier still replay. A byte-for-
        // byte retry therefore matches; a retry that changes only the label's
        // padding is a different call and gets idempotency_key_conflict.
        let sender_label = crate::agent_label::validate(new.sender_label.as_deref())?;

        let org = self.org();

        let msg = if let Some(key) = new.idempotency_key.as_deref() {
            let hash = message_idempotency_fingerprint(sender, &new);
            // A savepoint, not a bare INSERT — see jobs::add_job's identical
            // reasoning: Postgres aborts the whole transaction on a
            // statement error, so recovering from a unique-violation in the
            // same Tx needs a savepoint to roll back to.
            sqlx::query("SAVEPOINT send_message_idempotency")
                .execute(self.conn())
                .await?;
            let inserted = sqlx::query_as(&format!(
                "INSERT INTO messages (org_id, sender_user_id, sender_label, sender_kind, \
                                       team_id, kind, body, repo_id, \
                                       job_id, in_reply_to, idempotency_key, \
                                       idempotency_payload_hash, scope_teams) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,{SCOPE_TEAMS_SQL}) \
                 RETURNING {MSG_COLS}"
            ))
            .bind(org)
            .bind(sender)
            .bind(sender_label)
            .bind(new.sender_kind)
            .bind(new.team_id.map(|t| t.in_org(self.org())).transpose()?)
            .bind(new.kind)
            .bind(body)
            .bind(new.repo_id)
            .bind(new.job_id.as_ref().map(|j| j.0.as_str()))
            .bind(new.in_reply_to)
            .bind(key)
            .bind(&hash)
            .fetch_one(self.conn())
            .await;

            match inserted {
                Ok(msg) => {
                    sqlx::query("RELEASE SAVEPOINT send_message_idempotency")
                        .execute(self.conn())
                        .await?;
                    msg
                }
                // A concurrent caller won the race on this brand-new key
                // between find_replayed_message's read and this insert —
                // converge on its row exactly like jobs::add_job does. See
                // that function's identical comment on why the literal
                // index name below must stay in sync with 0025's
                // `CREATE UNIQUE INDEX messages_org_idempotency_key_idx`.
                Err(sqlx::Error::Database(db))
                    if db.is_unique_violation()
                        && db.constraint() == Some("messages_org_idempotency_key_idx") =>
                {
                    sqlx::query("ROLLBACK TO SAVEPOINT send_message_idempotency")
                        .execute(self.conn())
                        .await?;
                    sqlx::query("RELEASE SAVEPOINT send_message_idempotency")
                        .execute(self.conn())
                        .await?;
                    let winner: (i64, Vec<u8>) = sqlx::query_as(
                        "SELECT id, idempotency_payload_hash FROM messages \
                         WHERE org_id = $1 AND idempotency_key = $2",
                    )
                    .bind(org)
                    .bind(key)
                    .fetch_optional(self.conn())
                    .await?
                    // Unlike its three siblings (add_job's own idempotency
                    // recovery, link_ticket, create_from_ticket), this
                    // specific branch's precondition — the winner's row
                    // vanishing between the violation and this re-query —
                    // cannot currently occur: no code path in this codebase
                    // ever deletes a `users` or `orgs` row, and this insert
                    // has no other foreign key whose loss could produce the
                    // same shape. It stays classified as `RaceLost`
                    // (retriable) for structural consistency with the other
                    // three SAVEPOINT-recovery sites, and in case a future
                    // account- or org-deletion feature changes that. If such
                    // a feature is ever added, re-check this reasoning
                    // rather than assume it still holds — a retry here would
                    // resend the same `sender_user_id`, which could just as
                    // easily fail again on a foreign-key constraint
                    // (surfacing as `Error::Db`, not a second `RaceLost`)
                    // instead of succeeding.
                    .ok_or_else(|| {
                        Error::RaceLost(format!(
                            "send_message lost a unique-violation race for idempotency key {key:?} \
                             but no concurrently-created message was found — this is a transient \
                             server-side race; retry after a short backoff"
                        ))
                    })?;
                    if winner.1 != hash {
                        return Err(Error::IdempotencyKeyConflict {
                            key: key.to_string(),
                            tool: "send_message",
                        });
                    }
                    // See jobs::add_job's identical comment: this is
                    // reachable via a genuine concurrent race through the
                    // production (MCP) path, or via a direct tower-core caller
                    // that skips the find_replayed_message pre-check — this
                    // function cannot tell which.
                    tracing::warn!(
                        org = %org,
                        key,
                        "send_message's idempotency-key insert hit a unique violation and \
                         converged onto an existing message instead of failing; expected \
                         under concurrent replay of the same new key (see design spec §8) \
                         — unexpected otherwise"
                    );
                    sqlx::query_as(&format!(
                        "SELECT {MSG_COLS} FROM messages WHERE org_id = $1 AND id = $2"
                    ))
                    .bind(org)
                    .bind(winner.0)
                    .fetch_one(self.conn())
                    .await?
                }
                Err(error) => return Err(Error::Db(error)),
            }
        } else {
            // Unchanged from before this feature: no key, no idempotency
            // columns written, no savepoint.
            sqlx::query_as(&format!(
                "INSERT INTO messages (org_id, sender_user_id, sender_label, sender_kind, \
                                       team_id, kind, body, repo_id, \
                                       job_id, in_reply_to, scope_teams) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,{SCOPE_TEAMS_SQL}) \
                 RETURNING {MSG_COLS}"
            ))
            .bind(org)
            .bind(sender)
            .bind(sender_label)
            .bind(new.sender_kind)
            .bind(new.team_id.map(|t| t.in_org(self.org())).transpose()?)
            .bind(new.kind)
            .bind(body)
            .bind(new.repo_id)
            .bind(new.job_id.as_ref().map(|j| j.0.as_str()))
            .bind(new.in_reply_to)
            .fetch_one(self.conn())
            .await?
        };

        Ok(msg)
    }

    async fn inbox(&mut self, reader: UserId, q: &InboxQuery) -> Result<Vec<Message>> {
        let org = self.org();
        let limit = q.limit.clamp(1, INBOX_LIMIT_MAX);

        // Two orderings rather than one parameterized `ORDER BY`: the direction
        // of an ORDER BY cannot be bound as a parameter, and building it by
        // string concatenation from client input is how injection happens.
        let order = if q.newest_first { "DESC" } else { "ASC" };
        let (channel, channel_id) = channel_sql(q.channel, 6);

        let msgs = sqlx::query_as(&format!(
            "SELECT {MSG_COLS} FROM messages m \
             WHERE m.org_id = $1 \
               AND {channel} \
               AND (NOT $3 OR ( \
                     m.sender_user_id <> $2 \
                     AND m.id > COALESCE( \
                       (SELECT last_read_id FROM message_cursors \
                        WHERE org_id = $1 AND user_id = $2 AND {}), 0))) \
               AND {} \
             ORDER BY m.id {order} LIMIT $4",
            cursor_channel_sql(q.channel, 6),
            message_visible_sql(5)
        ))
        .bind(org)
        .bind(reader)
        .bind(q.unread_only)
        .bind(limit)
        .bind(team_uuids(q.visible_teams.as_ref()))
        .bind(channel_id)
        .fetch_all(self.conn())
        .await?;

        Ok(msgs)
    }

    async fn ack_messages_for(
        &mut self,
        reader: UserId,
        channel: ChannelRef,
        up_to: i64,
        scope: &TeamScope,
    ) -> Result<i64> {
        let org = self.org();
        let teams = team_uuids(scope.restriction().as_ref());
        let (in_channel, channel_id) = channel_sql(channel, 3);
        if up_to != 0 {
            let readable: bool = sqlx::query_scalar(&format!(
                "SELECT EXISTS (SELECT 1 FROM messages m WHERE m.org_id = $1 \
                   AND m.id = $4 \
                   AND {in_channel} \
                   AND {})",
                message_visible_sql(2)
            ))
            .bind(org)
            .bind(teams)
            .bind(channel_id)
            .bind(up_to)
            .fetch_one(self.conn())
            .await?;
            if !readable {
                return Err(Error::Invalid(format!(
                    "message {up_to} is not one you can read in this channel; read inbox \
                     for this same channel again and ack the highest id it returns (or 0)"
                )));
            }
        }

        // One fixed statement per channel kind: an `ON CONFLICT` target must
        // name the partial unique index's own predicate to infer it, and that
        // cannot be a parameter.
        let upsert = match channel {
            ChannelRef::Repo(_) => {
                "INSERT INTO message_cursors (org_id, user_id, repo_id, last_read_id) \
                 VALUES ($1,$2,$3,$4) \
                 ON CONFLICT (org_id, user_id, repo_id) WHERE repo_id IS NOT NULL DO UPDATE \
                   SET last_read_id = GREATEST(message_cursors.last_read_id, EXCLUDED.last_read_id), \
                       updated_at = now() \
                 RETURNING last_read_id"
            }
            ChannelRef::Team(_) => {
                "INSERT INTO message_cursors (org_id, user_id, team_id, last_read_id) \
                 VALUES ($1,$2,$3,$4) \
                 ON CONFLICT (org_id, user_id, team_id) WHERE team_id IS NOT NULL DO UPDATE \
                   SET last_read_id = GREATEST(message_cursors.last_read_id, EXCLUDED.last_read_id), \
                       updated_at = now() \
                 RETURNING last_read_id"
            }
        };
        let landed: i64 = sqlx::query_scalar(upsert)
            .bind(org)
            .bind(reader)
            .bind(channel_id)
            .bind(up_to)
            .fetch_one(self.conn())
            .await?;

        Ok(landed)
    }

    async fn message_readable(&mut self, id: i64, scope: &TeamScope) -> Result<bool> {
        let org = self.org();
        let teams = team_uuids(scope.restriction().as_ref());
        Ok(sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM messages m WHERE m.org_id = $1 AND m.id = $2 \
               AND {})",
            message_visible_sql(3)
        ))
        .bind(org)
        .bind(id)
        .bind(teams)
        .fetch_one(self.conn())
        .await?)
    }

    async fn reply_parent(&mut self, parent: i64, scope: &TeamScope) -> Result<()> {
        let org = self.org();
        if !scope.is_all() {
            // In id order, so two transactions holding the same pair cannot
            // deadlock each other.
            sqlx::query(
                "SELECT r.id FROM repos r WHERE r.org_id = $1 AND r.id IN ( \
                   SELECT m.repo_id FROM messages m WHERE m.org_id = $1 AND m.id = $2 \
                   UNION \
                   SELECT j.repo_id FROM messages m \
                     JOIN jobs j ON j.org_id = m.org_id AND j.id = m.job_id \
                   WHERE m.org_id = $1 AND m.id = $2) \
                 ORDER BY r.id FOR SHARE OF r",
            )
            .bind(org)
            .bind(parent)
            .execute(self.conn())
            .await?;
        }
        if !self.message_readable(parent, scope).await? {
            return Err(Error::Invalid(format!(
                "in_reply_to {parent} is not a message you can read; pass the id of a \
                 message from your inbox, or omit in_reply_to to start a new thread"
            )));
        }
        Ok(())
    }

    async fn unread_count_for(
        &mut self,
        reader: UserId,
        channel: ChannelRef,
        scope: &TeamScope,
    ) -> Result<i64> {
        let org = self.org();
        let teams = team_uuids(scope.restriction().as_ref());
        let (in_channel, channel_id) = channel_sql(channel, 4);
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM messages m \
             WHERE m.org_id = $1 \
               AND {in_channel} \
               AND m.sender_user_id <> $2 \
               AND m.id > COALESCE( \
                 (SELECT last_read_id FROM message_cursors \
                  WHERE org_id = $1 AND user_id = $2 AND {}), 0) \
               AND {}",
            cursor_channel_sql(channel, 4),
            message_visible_sql(3)
        ))
        .bind(org)
        .bind(reader)
        .bind(teams)
        .bind(channel_id)
        .fetch_one(self.conn())
        .await?;
        Ok(n)
    }
}
