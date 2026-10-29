//! Tracker webhook deliveries: repeat detection, per-org keys, and isolation.
//!
//! `tracker_deliveries` is a tenant table whose `SELECT` and `DELETE` policies
//! let an *unpinned* statement reach every org's rows (the retention sweep has
//! no org), while its `INSERT` policy admits a row only for the pinned org. So
//! every side needs proving: a pinned transaction sees and touches only its own
//! org's rows, an unpinned insert is refused, and the org-less sweep reaches
//! every org's where RLS applies.

mod common;

use common::db;
use of_core::tracker_deliveries::{record, sweep, Recorded};
use of_core::trackers::Provider;
use otto_tenant::ids::OrgId;
use otto_tenant::Db;
use sqlx::PgPool;

async fn record_one(db: &Db, org: OrgId, id: Option<&str>, digest: [u8; 32]) -> Recorded {
    let mut tx = db.begin(org).await.unwrap();
    let recorded = record(&mut tx, Provider::Github, id, &digest)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    recorded
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
    assert!(matches!(
        record_one(&db, org, Some("a"), [1; 32]).await,
        Recorded::First
    ));
    assert!(matches!(
        record_one(&db, org, Some("a"), [1; 32]).await,
        Recorded::Repeat { .. }
    ));
    assert!(matches!(
        record_one(&db, org, Some("b"), [1; 32]).await,
        Recorded::Repeat { .. }
    ));
    assert!(matches!(
        record_one(&db, org, None, [1; 32]).await,
        Recorded::Repeat { .. }
    ));
}

/// The delivery id is unsigned, so it is never a key: a seen id carrying a
/// different body is a first delivery, and so is an absent id.
#[sqlx::test]
async fn a_seen_delivery_id_never_makes_a_different_body_a_repeat(pool: PgPool) {
    let db = db(pool);
    let org = OrgId::new();
    for (id, digest) in [
        (Some("a"), [1; 32]),
        (Some("a"), [2; 32]),
        (None, [3; 32]),
        (None, [4; 32]),
    ] {
        assert!(matches!(
            record_one(&db, org, id, digest).await,
            Recorded::First
        ));
    }
}

#[sqlx::test]
async fn a_delivery_rolled_back_with_its_transaction_is_not_a_repeat(pool: PgPool) {
    let db = db(pool);
    let org = OrgId::new();
    let mut tx = db.begin(org).await.unwrap();
    assert!(matches!(
        record(&mut tx, Provider::Github, Some("a"), &[1; 32])
            .await
            .unwrap(),
        Recorded::First
    ));
    tx.rollback().await.unwrap();
    assert!(matches!(
        record_one(&db, org, Some("a"), [1; 32]).await,
        Recorded::First
    ));
}

/// A repeat carries when the original was received, so an operator can tell a
/// redelivery from a replay by comparing it with GitHub's delivery log.
#[sqlx::test]
async fn a_repeat_reports_when_the_original_was_received(pool: PgPool) {
    let db = db(pool.clone());
    let org = OrgId::new();
    record_one(&db, org, Some("a"), [5; 32]).await;
    let stored: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT received_at FROM tracker_deliveries WHERE org_id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        record_one(&db, org, Some("a"), [5; 32]).await,
        Recorded::Repeat {
            received_at: Some(stored)
        }
    );
}

/// A repeat's original time is its own org's: org B, recording a body org A
/// recorded earlier, is told B's own time, never A's.
#[sqlx::test]
async fn a_repeat_reports_only_its_own_orgs_original(pool: PgPool) {
    let db = db(pool.clone());
    let (a, b) = (OrgId::new(), OrgId::new());
    record_one(&db, a, None, [6; 32]).await;
    sqlx::query("UPDATE tracker_deliveries SET received_at = now() - interval '5 days'")
        .execute(&pool)
        .await
        .unwrap();
    record_one(&db, b, None, [6; 32]).await;
    let b_stored: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT received_at FROM tracker_deliveries WHERE org_id = $1")
            .bind(b)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        record_one(&db, b, None, [6; 32]).await,
        Recorded::Repeat {
            received_at: Some(b_stored)
        }
    );
}

/// The keys are per org: another org seeing the same body or id is not a repeat.
#[sqlx::test]
async fn deliveries_do_not_cross_orgs(pool: PgPool) {
    let db = db(pool);
    let (a, b) = (OrgId::new(), OrgId::new());
    assert!(matches!(
        record_one(&db, a, Some("same"), [9; 32]).await,
        Recorded::First
    ));
    assert!(matches!(
        record_one(&db, b, Some("same"), [9; 32]).await,
        Recorded::First
    ));
    assert!(matches!(
        record_one(&db, b, Some("same"), [9; 32]).await,
        Recorded::Repeat { .. }
    ));
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
    assert!(matches!(
        record_one(&db, b, Some("old-b"), [2; 32]).await,
        Recorded::First
    ));
}

/// Only the org-less sweep needs the `IS NULL` branch, and it never inserts: an
/// unpinned insert, which no code path makes, is refused rather than admitted
/// for any org. The unpinned `DELETE` keeps its `WHERE` on purpose: a DELETE
/// with no `WHERE` reads no rows, so the `SELECT` policy would not bind it and
/// the assertion would not pin that policy's `IS NULL` branch.
#[sqlx::test]
async fn rls_scopes_an_unpinned_tracker_delivery_insert_to_nothing(pool: PgPool) {
    let db = db(pool.clone());
    let (a, b) = (OrgId::new(), OrgId::new());
    record_one(&db, a, Some("a"), [1; 32]).await;
    record_one(&db, b, Some("b"), [2; 32]).await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    let forged = sqlx::query(
        "INSERT INTO tracker_deliveries (org_id, provider, digest) \
         VALUES ($1, 'github', decode(repeat('cd', 32), 'hex'))",
    )
    .bind(a)
    .execute(tx.conn())
    .await;
    // 42501 is what a policy's `WITH CHECK` raises, so this is the policy
    // refusing, not some other failure passing for it.
    let code = forged
        .expect_err("an unpinned insert recorded a delivery")
        .as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()));
    assert_eq!(code.as_deref(), Some("42501"));
    tx.rollback().await.unwrap();

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    let swept = sqlx::query("DELETE FROM tracker_deliveries WHERE received_at <= now()")
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    tx.rollback().await.unwrap();
    assert_eq!(swept, 2, "an unpinned delete must reach every org's rows");

    let mut policies: Vec<(String, String)> = sqlx::query_as(
        "SELECT p.polname::text, p.polcmd::text FROM pg_policy p \
         JOIN pg_class c ON c.oid = p.polrelid \
         WHERE c.relname = 'tracker_deliveries'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    policies.sort();
    assert_eq!(
        policies,
        vec![
            ("tracker_deliveries_insert".to_owned(), "a".to_owned()),
            ("tracker_deliveries_sweep".to_owned(), "d".to_owned()),
            (
                "tracker_deliveries_tenant_isolation".to_owned(),
                "r".to_owned()
            ),
        ]
    );
}

/// `verify_tenant_isolation` discovers tenant tables by a policy named
/// `<table>_tenant_isolation`; splitting the policy by command must keep that
/// name on one of them, or the startup check silently stops vouching for it.
#[sqlx::test]
async fn the_startup_check_still_vouches_for_tracker_deliveries(pool: PgPool) {
    let db = db(pool);
    let report = db.verify_tenant_isolation().await.unwrap();
    let table = report
        .tables
        .iter()
        .find(|t| t.name == "tracker_deliveries")
        .unwrap_or_else(|| {
            panic!(
                "tracker_deliveries missing from {:?}",
                report.tables.iter().map(|t| &t.name).collect::<Vec<_>>()
            )
        });
    assert!(table.rls_forced);
}
