//! What the platform's signed lifecycle webhooks do to this database.
//!
//! The platform owns orgs, teams, and memberships; this database holds rows keyed
//! by their ids with no foreign key to them (they are in another database). So
//! when the platform deletes one, it tells every registered resource server, and
//! this module is the cleanup. Deliveries are at-least-once and unordered
//! (`otto_resource::webhook`), so everything here is idempotent twice over: each
//! delivery is recorded in `platform_events` **in the same transaction as its
//! effects** (a redelivery finds the row and does nothing), and each effect is
//! itself safe to repeat.
//!
//! | Event | Effect |
//! |---|---|
//! | `org.deleted` | Every row this database holds for the org is deleted: repos, jobs, leases, messages, tracker connections, counters, unshipped usage, console sessions, and the org's domain audit trail. Only the dedupe marker survives. |
//! | `team.deleted` | **Scope is kept, not widened.** Repos, jobs, and messages keep their `team_id`. The team's channel (its messages and its members' cursors) stays under the tombstoned id, unreachable over MCP because the slug no longer resolves. See below. |
//! | `member.removed` | The user's live leases are released, their in-progress or active job claims go back to `pending`, their message read cursors are deleted (one per channel they read), and their console sessions in the org are deleted. History they authored (jobs they created, messages they sent) is kept. |
//!
//! # Why clean-up and requests take a lock
//!
//! Authentication refuses a tombstoned org or user ([`revoked`]), but a request
//! that passed that check a moment before the tombstone was written can still
//! be on its way to a transaction. Without the org being gone as a foreign key,
//! nothing would stop it from re-creating rows after the purge. So every tenant
//! transaction is opened with [`begin_live`], which takes the org's lifecycle
//! lock *shared* and re-checks the tombstones while holding it, and `org.deleted`
//! / `member.removed` take the same lock *exclusively* after writing their
//! tombstone. A transaction already in flight finishes first and is cleaned up
//! with everything else; one that starts later sees the tombstone and is
//! refused. No redelivery is needed for either.
//!
//! # Why `team.deleted` keeps the team id
//!
//! A null `team_id` means *org-wide*. The obvious cleanup, "null out the deleted
//! team's references", would therefore publish everything the team had to the
//! whole org, on the strength of a webhook. That is exactly the failure the old
//! schema had (`ON DELETE SET NULL`) and the old code guarded against by
//! refusing the delete. The platform does not ask us any more, so the guard
//! becomes this rule: **the dangling id is the tombstone.** The platform never
//! reuses a team id, so a deleted team's id matches no member's team list and no
//! `VerifiedTeam::verify`; the repo is visible to org owners and admins only
//! until one reassigns it (to a live team, or to org-wide, which is then a
//! deliberate, audited human act) through `PATCH .../repos/{repo}`. Nothing is
//! deleted and nothing is widened. The event is recorded in the audit trail with
//! the number of repos, jobs, and messages left orphaned, so an admin can find
//! them.

use crate::audit::action;
use crate::error::Result;
use otto_resource::webhook::{LifecycleEvent, WebhookEvent};
use otto_tenant::audit::Entry;
use otto_tenant::ids::{OrgId, TeamId, UserId};
use otto_tenant::{Db, Tx};
use serde::Serialize;

/// What handling a delivery did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    /// Effects applied now.
    Applied { detail: serde_json::Value },
    /// This event id was applied by an earlier delivery. The clean-up was
    /// re-run anyway (it is idempotent), to catch work that raced the first run.
    Duplicate,
    /// An event type this version does not know. Acknowledged, not failed:
    /// failing it would only make the platform retry it forever.
    Ignored,
}

/// How long a removed member is refused by [`revoked`]. Past the introspection
/// cache's 60 s the platform itself answers "inactive", so this only has to
/// outlast that; it is short so a member who is re-added is not locked out.
pub const REMOVED_MEMBER_TTL_SECS: i64 = 300;

/// Whether `user`'s token for `org` must be refused regardless of what the
/// platform's cached introspection says: the org was deleted, or the user was
/// just removed from it.
///
/// Called by both HTTP surfaces on every authenticated request. An `Err` is a
/// database failure and must be answered `503`, not treated as "not revoked".
pub async fn revoked(db: &Db, org: OrgId, user: UserId) -> Result<bool> {
    let mut conn = db.pool().acquire().await?;
    revoked_on(&mut conn, org, Some(user)).await
}

/// [`revoked`] on a given connection. With no user, only the org is checked.
async fn revoked_on(
    conn: &mut sqlx::PgConnection,
    org: OrgId,
    user: Option<UserId>,
) -> Result<bool> {
    let hit: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deleted_orgs WHERE org_id = $1) \
             OR EXISTS (SELECT 1 FROM removed_members \
                        WHERE org_id = $1 AND user_id = $2 \
                          AND removed_at > now() - make_interval(secs => $3))",
    )
    .bind(org)
    .bind(user)
    .bind(REMOVED_MEMBER_TTL_SECS as f64)
    .fetch_one(conn)
    .await?;
    Ok(hit)
}

/// The advisory-lock namespace for an org's lifecycle lock; the org id is hashed
/// into the second key. The two-key form never overlaps sqlx's single-key
/// migration lock. A hash collision between two orgs only serializes them.
const LIFECYCLE_LOCK: i32 = 0x6f66_6c63; // "oflc"

/// Open a transaction pinned to `org` that no lifecycle clean-up can interleave
/// with, refusing it if the org was deleted or `user` was just removed.
///
/// Every request-path tenant transaction opens here rather than with
/// `Db::begin` directly: see the module docs. `user` is `None` for work done on
/// nobody's behalf (an inbound tracker webhook, a background write-back), which
/// is refused only for a deleted org. The lock is held until the transaction
/// ends, so keep the transaction short; it is shared, so requests never wait on
/// each other, only on a clean-up in progress.
pub async fn begin_live(db: &Db, org: OrgId, user: Option<UserId>) -> Result<Tx<'static>> {
    let mut tx = db.begin(org).await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1, hashtext($2::text))")
        .bind(LIFECYCLE_LOCK)
        .bind(org)
        .execute(tx.conn())
        .await?;
    if revoked_on(tx.conn(), org, user).await? {
        // Dropping `tx` rolls it back and releases the lock.
        return Err(crate::Error::AccessRevoked);
    }
    Ok(tx)
}

/// Wait for every transaction [`begin_live`] opened for `org` to finish, and
/// keep new ones out until `tx` ends. Called right after the tombstone is
/// written, so whatever starts after this sees it.
async fn exclude_requests(tx: &mut Tx<'_>, org: OrgId) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::text))")
        .bind(LIFECYCLE_LOCK)
        .bind(org)
        .execute(tx.conn())
        .await?;
    Ok(())
}

/// Housekeeping: forget dedupe markers older than `keep_days` and expired
/// removed-member tombstones. Safe at any time, because clean-up is idempotent
/// and a forgotten marker only means a (very) late redelivery writes its audit
/// entry again.
pub async fn sweep(db: &Db, keep_days: i32) -> Result<u64> {
    let events = sqlx::query(
        "DELETE FROM platform_events WHERE received_at < now() - make_interval(days => $1)",
    )
    .bind(keep_days)
    .execute(db.pool())
    .await?
    .rows_affected();
    let tombstones = sqlx::query(
        "DELETE FROM removed_members WHERE removed_at < now() - make_interval(secs => $1)",
    )
    .bind((REMOVED_MEMBER_TTL_SECS * 2) as f64)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(events + tombstones)
}

/// Apply one verified delivery.
pub async fn apply(db: &Db, event: &WebhookEvent) -> Result<Outcome> {
    match &event.event {
        LifecycleEvent::OrgDeleted { org_id } => org_deleted(db, event, (*org_id).into()).await,
        LifecycleEvent::TeamDeleted { org_id, team_id } => {
            team_deleted(db, event, (*org_id).into(), (*team_id).into()).await
        }
        LifecycleEvent::MemberRemoved { org_id, user_id } => {
            member_removed(db, event, (*org_id).into(), (*user_id).into()).await
        }
        LifecycleEvent::Unknown { kind } => {
            tracing::warn!(kind = %kind, event_id = %event.id, "ignoring unknown platform event");
            Ok(Outcome::Ignored)
        }
        // `LifecycleEvent` is #[non_exhaustive].
        _ => Ok(Outcome::Ignored),
    }
}

/// Record the delivery; `false` if an earlier one already did.
async fn first_delivery(
    tx: &mut otto_tenant::Tx<'_>,
    event: &WebhookEvent,
    kind: &str,
) -> Result<bool> {
    let org = tx.org();
    let inserted = sqlx::query(
        "INSERT INTO platform_events (event_id, org_id, kind) VALUES ($1, $2, $3) \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event.id)
    .bind(org)
    .bind(kind)
    .execute(tx.conn())
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

async fn org_deleted(db: &Db, event: &WebhookEvent, org: OrgId) -> Result<Outcome> {
    // Tombstone first, on its own, so authentication refuses this org from now
    // on -- before, not as part of, the purge.
    sqlx::query("INSERT INTO deleted_orgs (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING")
        .bind(org)
        .execute(db.pool())
        .await?;

    // From here until commit no request transaction for this org is open, and
    // any that opens afterwards finds the tombstone.
    let mut tx = db.begin(org).await?;
    exclude_requests(&mut tx, org).await?;

    // The org's audit trail is append-only to a pinned transaction (there is no
    // UPDATE policy and DELETE needs `current_org() IS NULL`), so it is deleted
    // on the pool, unpinned -- under the lock above, so no request can add to
    // it afterwards. Idempotent, and a failure here aborts before the dedupe
    // marker exists, so the platform's retry runs it again.
    let audit = sqlx::query("DELETE FROM audit_events WHERE org_id = $1")
        .bind(org)
        .execute(db.pool())
        .await?
        .rows_affected();

    let first = first_delivery(&mut tx, event, "org.deleted").await?;

    // Children before parents: messages reference jobs and repos, jobs and
    // tracker bindings reference repos, bindings and the connection index
    // reference connections. Every statement names the org explicitly (guard 1)
    // in addition to the pinned policy (guard 2), and the one table with no
    // policy, `tracker_connection_index`, relies on that predicate alone.
    let mut deleted = serde_json::Map::new();
    for table in [
        "messages",
        "message_cursors",
        "tracker_bindings",
        "tracker_connection_index",
        "tracker_connections",
        "repo_leases",
        "job_dependencies",
        "jobs",
        "repo_remotes",
        "repos",
        "org_counters",
        "usage_outbox",
        "console_sessions",
        "tracker_deliveries",
    ] {
        let n = sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(org)
            .execute(tx.conn())
            .await?
            .rows_affected();
        deleted.insert(table.to_string(), n.into());
    }
    deleted.insert("audit_events".into(), audit.into());
    tx.commit().await?;

    tracing::info!(org = %org, event_id = %event.id, first, ?deleted, "purged a deleted org's data");
    Ok(if first {
        Outcome::Applied {
            detail: deleted.into(),
        }
    } else {
        Outcome::Duplicate
    })
}

async fn team_deleted(db: &Db, event: &WebhookEvent, org: OrgId, team: TeamId) -> Result<Outcome> {
    let mut tx = db.begin(org).await?;
    let first = first_delivery(&mut tx, event, "team.deleted").await?;

    let mut counts = serde_json::Map::new();
    for table in ["repos", "jobs", "messages"] {
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE org_id = $1 AND team_id = $2"
        ))
        .bind(org)
        .bind(team)
        .fetch_one(tx.conn())
        .await?;
        counts.insert(table.to_string(), n.into());
    }
    let detail: serde_json::Value = counts.into();

    // Deliberately no UPDATE: the dangling id is the tombstone (module docs).
    if first {
        tx.audit(
            Entry::new(action::TEAM_SCOPE_ORPHANED)
                .actor_label("platform")
                .target("team", team.to_string())
                .detail(detail.clone()),
        )
        .await?;
    }
    tx.commit().await?;

    tracing::info!(org = %org, team = %team, ?detail, "team deleted; its scoped rows stay team-scoped");
    Ok(if first {
        Outcome::Applied { detail }
    } else {
        Outcome::Duplicate
    })
}

async fn member_removed(
    db: &Db,
    event: &WebhookEvent,
    org: OrgId,
    user: UserId,
) -> Result<Outcome> {
    // Tombstone first (see `revoked`): from here on this user's cached token is
    // refused, so nothing new is leased or claimed in their name while we clean up.
    sqlx::query(
        "INSERT INTO removed_members (org_id, user_id) VALUES ($1, $2) \
         ON CONFLICT (org_id, user_id) DO UPDATE SET removed_at = now()",
    )
    .bind(org)
    .bind(user)
    .execute(db.pool())
    .await?;

    let mut tx = db.begin(org).await?;
    exclude_requests(&mut tx, org).await?;
    let first = first_delivery(&mut tx, event, "member.removed").await?;

    let leases = sqlx::query(
        "UPDATE repo_leases SET released_at = now() \
         WHERE org_id = $1 AND holder_user_id = $2 AND released_at IS NULL",
    )
    .bind(org)
    .bind(user)
    .execute(tx.conn())
    .await?
    .rows_affected();

    // The same involuntary re-pend an expired claim gets (see `claim_jobs`):
    // back to `pending`, cancellation bookkeeping cleared so it does not
    // misattribute to whoever claims the job next. Not counted as an attempt.
    let claims = sqlx::query(
        "UPDATE jobs SET status = 'pending', claimed_by = NULL, claimed_by_label = NULL, \
                started_at = NULL, claim_expires_at = NULL, \
                cancel_requested_at = NULL, cancel_requested_by = NULL, cancel_reason = NULL \
         WHERE org_id = $1 AND claimed_by = $2 AND status IN ('in-progress', 'active')",
    )
    .bind(org)
    .bind(user)
    .execute(tx.conn())
    .await?
    .rows_affected();

    let cursors = sqlx::query("DELETE FROM message_cursors WHERE org_id = $1 AND user_id = $2")
        .bind(org)
        .bind(user)
        .execute(tx.conn())
        .await?
        .rows_affected();

    // Their console logins go with them. The tombstone above already refuses the
    // user's tokens; this removes the held token pair itself.
    let sessions = crate::console_sessions::delete_for_user(&mut tx, user).await?;

    let detail = serde_json::json!({
        "leases_released": leases,
        "claims_released": claims,
        "cursors_dropped": cursors,
        "sessions_dropped": sessions,
    });
    if first {
        tx.audit(
            Entry::new(action::MEMBER_RELEASED)
                .actor_label("platform")
                .target("user", user.to_string())
                .detail(detail.clone()),
        )
        .await?;
    }
    tx.commit().await?;

    tracing::info!(org = %org, user = %user, ?detail, "member removed; released what they held");
    Ok(if first {
        Outcome::Applied { detail }
    } else {
        Outcome::Duplicate
    })
}
