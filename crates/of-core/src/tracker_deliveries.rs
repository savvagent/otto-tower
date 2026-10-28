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
//! A delivery is a repeat if this org already recorded the same digest or the
//! same delivery id. The digest (of the event name and the signed body) is
//! what stops a replay: the signature binds the body, so a replay must send the
//! same bytes. `X-GitHub-Delivery` is not signed, so it only catches GitHub's
//! own redeliveries, which reuse it.

use crate::error::Result;
use crate::trackers::Provider;
use otto_tenant::{Db, Tx};

/// Record a verified delivery in `tx`. `false` if this org already recorded the
/// same `digest` or the same `delivery_id` for `provider`.
///
/// Two concurrent deliveries of the same event serialize on the unique key: the
/// second waits for the first's transaction, and is a repeat if it committed or
/// a first delivery if it rolled back. Commit `tx` only after applying the
/// delivery's effects in it.
pub async fn record(
    tx: &mut Tx<'_>,
    provider: Provider,
    delivery_id: Option<&str>,
    digest: &[u8; 32],
) -> Result<bool> {
    let org = tx.org();
    // No conflict target: a clash on either the primary key (the digest) or the
    // partial unique index (the delivery id) makes this a repeat. Every unique
    // constraint on this table is a dedupe key; a new one would become one too.
    let inserted = sqlx::query(
        "INSERT INTO tracker_deliveries (org_id, provider, digest, delivery_id) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(org)
    .bind(provider)
    .bind(digest.as_slice())
    .bind(delivery_id)
    .execute(tx.conn())
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

/// Housekeeping: forget deliveries older than `keep_days`, for every org.
///
/// Runs on the pool with no org pinned (background code only), so it carries no
/// `org_id` predicate on purpose: the table's policy lets an unpinned statement
/// reach every org's rows. A forgotten delivery replayed later is applied
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
