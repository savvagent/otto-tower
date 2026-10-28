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

use crate::error::Result;
use crate::trackers::Provider;
use otto_tenant::{Db, Tx};

/// Record a verified delivery in `tx`. `false` if this org already recorded the
/// same `digest` for `provider`. `delivery_id` is stored alongside for
/// diagnosis; it plays no part in deciding a repeat.
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
) -> Result<bool> {
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
