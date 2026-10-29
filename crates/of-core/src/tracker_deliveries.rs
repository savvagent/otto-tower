//! Tracker webhook deliveries already applied, so a repeat does nothing.
//!
//! GitHub signs a webhook's body (`X-Hub-Signature-256`) but not when it was
//! sent, so a captured delivery verifies until the webhook secret is rotated.
//! The webhook route records each verified event here **in the transaction
//! that applies it** and stops when the record already exists. A delivery that
//! fails rolls its record back with everything else, so a manual redelivery of
//! it (GitHub does not retry on its own) is not mistaken for a repeat. An event
//! that matched no binding, or was dropped, still commits its record: it was
//! judged once and is not judged again.
//!
//! A delivery is a repeat if this org already recorded the same digest of its
//! signed body: the signature binds the body, so a replay must send the same
//! bytes. The unsigned `X-GitHub-Delivery` is stored for diagnosis only and is
//! never a key, so an unsigned value can never mark a genuine delivery as seen.
//!
//! A repeat reports when the original was received. A redelivery GitHub's log
//! shows and a replay it does not look alike here; the original's time is what
//! lets an operator tell them apart.

use crate::error::Result;
use crate::trackers::Provider;
use chrono::{DateTime, Utc};
use otto_tenant::{Db, Tx};

/// What [`record`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Recorded in the caller's transaction: apply the delivery in it.
    First,
    /// This org already recorded the same body; acknowledge it without effect.
    /// `received_at` is when the original was received. It is `None` only if
    /// the sweep forgot the original between the insert and the read that
    /// follows it.
    Repeat { received_at: Option<DateTime<Utc>> },
}

/// Record a verified delivery in `tx`: [`Recorded::First`], or
/// [`Recorded::Repeat`] with the original's `received_at` if this org already
/// recorded the same `digest` for `provider`. `delivery_id` is stored alongside
/// for diagnosis; it plays no part in deciding a repeat.
///
/// Two concurrent deliveries of the same body serialize on the primary key: the
/// second waits for the first's transaction, and is a repeat if it committed or
/// a first delivery if it rolled back. Commit `tx` only after applying the
/// delivery's effects in it.
pub async fn record(
    tx: &mut Tx<'_>,
    provider: Provider,
    delivery_id: Option<&str>,
    digest: &[u8; 32],
) -> Result<Recorded> {
    let org = tx.org();
    let inserted = sqlx::query(
        "INSERT INTO tracker_deliveries (org_id, provider, digest, delivery_id) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (org_id, provider, digest) DO NOTHING",
    )
    .bind(org)
    .bind(provider)
    .bind(digest.as_slice())
    .bind(delivery_id)
    .execute(tx.conn())
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(Recorded::First);
    }
    // A second statement, not a CTE beside the INSERT: under READ COMMITTED a
    // conflicting INSERT waits for a concurrent first delivery's transaction,
    // but its own statement's snapshot was taken before that commit and cannot
    // see the row. Only a new statement's snapshot does. Between the two
    // statements the sweep can still forget the row, hence the `Option`.
    let received_at = sqlx::query_scalar(
        "SELECT received_at FROM tracker_deliveries \
         WHERE org_id = $1 AND provider = $2 AND digest = $3",
    )
    .bind(org)
    .bind(provider)
    .bind(digest.as_slice())
    .fetch_optional(tx.conn())
    .await?;
    Ok(Recorded::Repeat { received_at })
}

/// Housekeeping: forget deliveries older than `keep_days`, for every org.
///
/// Runs on the pool with no org pinned (background code only), so it carries no
/// `org_id` predicate on purpose: the table's `SELECT` and `DELETE` policies
/// both have an unpinned branch, so an unpinned statement reaches every org's
/// rows (its `INSERT` policy has none: only a pinned transaction records). A forgotten delivery replayed later is applied
/// again. Against an existing job only its revision and `ticket_changed_at`
/// checks guard it, and if the job is gone a replayed `opened` or `labeled`
/// event creates it again.
pub async fn sweep(db: &Db, keep_days: i32) -> Result<u64> {
    let swept = sqlx::query(
        "DELETE FROM tracker_deliveries WHERE received_at < now() - make_interval(days => $1)",
    )
    .bind(keep_days)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(swept)
}
