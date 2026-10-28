//! Tracker webhook deliveries: repeat detection, per-org keys, and isolation.
//!
//! `tracker_deliveries` is a tenant table whose policy lets an *unpinned*
//! statement reach every org's rows (the retention sweep has no org). So both
//! sides need proving: a pinned transaction sees and touches only its own org's
//! rows, and the org-less sweep reaches every org's where RLS applies.

mod common;

use common::db;
use of_core::tracker_deliveries::{record, sweep};
use of_core::trackers::Provider;
use otto_tenant::ids::OrgId;
use otto_tenant::Db;
use sqlx::PgPool;

async fn record_one(db: &Db, org: OrgId, id: Option<&str>, digest: [u8; 32]) -> bool {
    let mut tx = db.begin(org).await.unwrap();
    let first = record(&mut tx, Provider::Github, id, &digest)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    first
}

async fn count_all(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM tracker_deliveries")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn the_same_body_is_a_repeat_whatever_delivery_id_it_carries(pool: PgPool) {
    let db = db(pool);
    let org = OrgId::new();
    assert!(record_one(&db, org, Some("a"), [1; 32]).await);
    assert!(!record_one(&db, org, Some("a"), [1; 32]).await);
    assert!(!record_one(&db, org, Some("b"), [1; 32]).await);
    assert!(!record_one(&db, org, None, [1; 32]).await);
}

#[sqlx::test]
async fn a_seen_delivery_id_is_a_repeat_whatever_body_it_carries(pool: PgPool) {
    let db = db(pool);
    let org = OrgId::new();
    assert!(record_one(&db, org, Some("a"), [1; 32]).await);
    assert!(!record_one(&db, org, Some("a"), [2; 32]).await);
    // An absent id never clashes with another absent id.
    assert!(record_one(&db, org, None, [3; 32]).await);
    assert!(record_one(&db, org, None, [4; 32]).await);
}

#[sqlx::test]
async fn a_delivery_rolled_back_with_its_transaction_is_not_a_repeat(pool: PgPool) {
    let db = db(pool);
    let org = OrgId::new();
    let mut tx = db.begin(org).await.unwrap();
    assert!(record(&mut tx, Provider::Github, Some("a"), &[1; 32])
        .await
        .unwrap());
    tx.rollback().await.unwrap();
    assert!(record_one(&db, org, Some("a"), [1; 32]).await);
}

/// The keys are per org: another org seeing the same body or id is not a repeat.
#[sqlx::test]
async fn deliveries_do_not_cross_orgs(pool: PgPool) {
    let db = db(pool);
    let (a, b) = (OrgId::new(), OrgId::new());
    assert!(record_one(&db, a, Some("same"), [9; 32]).await);
    assert!(record_one(&db, b, Some("same"), [9; 32]).await);
    assert!(!record_one(&db, b, Some("same"), [9; 32]).await);
}

/// Deliberately unscoped SQL in a pinned transaction: RLS, not the predicate,
/// is what has to hold. The test must `SET LOCAL ROLE otto_app` explicitly,
/// because `#[sqlx::test]` connects as a superuser and bypasses RLS.
#[sqlx::test]
async fn rls_scopes_tracker_deliveries_to_the_pinned_org(pool: PgPool) {
    let db = db(pool);
    let (a, b) = (OrgId::new(), OrgId::new());
    record_one(&db, a, Some("a"), [1; 32]).await;
    record_one(&db, b, Some("b"), [2; 32]).await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(b.to_string())
        .execute(tx.conn())
        .await
        .unwrap();

    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM tracker_deliveries")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    let deleted = sqlx::query("DELETE FROM tracker_deliveries WHERE org_id = $1")
        .bind(a)
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    let forged = sqlx::query(
        "INSERT INTO tracker_deliveries (org_id, provider, digest) \
         VALUES ($1, 'github', decode(repeat('ab', 32), 'hex'))",
    )
    .bind(a)
    .execute(tx.conn())
    .await;
    tx.rollback().await.unwrap();

    assert_eq!(seen, 1, "org B saw org A's delivery");
    assert_eq!(deleted, 0, "org B deleted org A's delivery");
    assert!(forged.is_err(), "org B recorded a delivery for org A");
}

/// The sweep runs on the pool with no org pinned. `#[sqlx::test]` connects as a
/// superuser, which bypasses RLS, so here the pool runs as `otto_app`, a role
/// RLS binds: a sweep that matched zero rows there would fail this test.
#[sqlx::test]
async fn the_sweep_reaches_every_org_where_rls_applies(pool: PgPool) {
    let db = db(pool.clone());
    let (a, b) = (OrgId::new(), OrgId::new());
    record_one(&db, a, Some("old-a"), [1; 32]).await;
    record_one(&db, b, Some("old-b"), [2; 32]).await;
    sqlx::query("UPDATE tracker_deliveries SET received_at = now() - interval '40 days'")
        .execute(&pool)
        .await
        .unwrap();
    record_one(&db, a, Some("fresh"), [3; 32]).await;

    let restricted = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE otto_app").execute(conn).await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let rdb = Db::from_pool(restricted);

    assert_eq!(sweep(&rdb, 30).await.unwrap(), 2);
    // A session left on the throwaway database would hang its teardown.
    rdb.pool().close().await;
    assert_eq!(count_all(&pool).await, 1, "the fresh delivery was kept");
    // A swept delivery is no longer a repeat.
    assert!(record_one(&db, b, Some("old-b"), [2; 32]).await);
}
