//! Rows an older image enqueues under the old key after this has run are
//! handled by the usage shipper (`tower_billing::outbox::RETIRED_METERS`), and
//! tested there.
//!
//! `0006_meter_otto_tower.sql` rewrites queued `usage_outbox` rows from the old
//! meter key to the new one, for every org, under both shapes a migration can
//! run in: a role that bypasses RLS (this deployment's superuser) and a role
//! subject to RLS with no `app.org_id` set (`otto_app`, standing in for the
//! fallback). `usage_outbox`'s policy lets an org-less session see every org,
//! so a bare UPDATE would also pass the second test; the per-org loop is the
//! repo's rule for any rewrite of tenant data, kept so the migration does not
//! depend on that policy branch surviving.

mod common;

use common::db;
use sqlx::PgPool;
use uuid::Uuid;

const MIGRATION: &str = include_str!("../migrations/0006_meter_otto_tower.sql");
const OLD: &str = "otto-factory/jobs_completed";
const NEW: &str = "otto-tower/jobs_completed";

async fn seed(pool: &PgPool, orgs: &[Uuid]) {
    for org in orgs {
        for (tool, meter, qty) in [
            ("complete_job", Some(OLD), Some(1_i64)),
            ("complete_job", Some(OLD), Some(1)),
            ("complete_job", Some(NEW), Some(1)),
            ("list_jobs", None, None),
        ] {
            sqlx::query(
                "INSERT INTO usage_outbox (org_id, tool, billable, meter, quantity) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(org)
            .bind(tool)
            .bind(meter.is_some())
            .bind(meter)
            .bind(qty)
            .execute(pool)
            .await
            .unwrap();
        }
    }
}

async fn counts(pool: &PgPool, org: Uuid) -> (i64, i64, i64) {
    let n = |m: Option<&'static str>| {
        let q = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM usage_outbox WHERE org_id = $1 \
             AND meter IS NOT DISTINCT FROM $2",
        )
        .bind(org)
        .bind(m);
        q.fetch_one(pool)
    };
    (
        n(Some(OLD)).await.unwrap(),
        n(Some(NEW)).await.unwrap(),
        n(None).await.unwrap(),
    )
}

#[sqlx::test]
async fn rewrites_every_orgs_queued_rows_when_rls_is_bypassed(pool: PgPool) {
    let orgs = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    seed(&pool, &orgs).await;

    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();

    for org in orgs {
        assert_eq!(counts(&pool, org).await, (0, 3, 1), "org {org}");
    }
}

#[sqlx::test]
async fn rewrites_every_orgs_queued_rows_when_rls_applies_with_no_org_set(pool: PgPool) {
    let orgs = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    seed(&pool, &orgs).await;

    // Drop to the non-owner role RLS applies to, with no app.org_id, exactly as a
    // migration on a non-bypassing deployment would run. Each org's rows must
    // still be rewritten while the loop pins app.org_id to that org.
    let db = db(pool.clone());
    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::raw_sql(MIGRATION).execute(tx.conn()).await.unwrap();
    tx.commit().await.unwrap();

    for org in orgs {
        assert_eq!(counts(&pool, org).await, (0, 3, 1), "org {org}");
    }
}

#[sqlx::test]
async fn leaves_rejected_history_and_is_idempotent(pool: PgPool) {
    let org = Uuid::new_v4();
    seed(&pool, &[org]).await;
    sqlx::query(
        "INSERT INTO usage_outbox_rejected (event_id, org_id, tool, billable, occurred_at, reason, meter, quantity) \
         VALUES (gen_random_uuid(), $1, 'complete_job', true, now(), 'x', $2, 1)",
    )
    .bind(org)
    .bind(OLD)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();

    assert_eq!(counts(&pool, org).await, (0, 3, 1));
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_outbox_rejected WHERE org_id = $1 AND meter = $2",
    )
    .bind(org)
    .bind(OLD)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, 1);
}
