//! The usage outbox against a real Postgres and a mock platform.
//!
//! The properties that matter: a call's record commits or rolls back with the
//! call; an outage loses nothing; and everything recorded reaches the platform
//! exactly once, however many times or from however many shippers it is tried.

use std::time::Duration;

use savvagent_tenant::ids::{OrgId, UserId};
use savvagent_tenant::Db;
use sqlx::PgPool;
use tower_billing::outbox::{self, ShipperConfig};
use tower_billing::{BillingError, Meter, Metered, METER_JOBS_COMPLETED};
use tower_testkit::{tripped_budget, MeterMode, MockPlatform};

fn cfg() -> ShipperConfig {
    ShipperConfig {
        // Short lease and backoff so a test can wait out a retry.
        claim_lease: Duration::from_millis(50),
        base_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(200),
        declare_max_backoff: Duration::from_millis(200),
        poll: Duration::from_millis(10),
        ..ShipperConfig::default()
    }
}

async fn record(db: &Db, org: OrgId, user: UserId, meter: &Meter, tool: &str) {
    let mut tx = db.begin(org).await.unwrap();
    meter.charge(&mut tx, user, tool).await.unwrap();
    tx.commit().await.unwrap();
}

/// The billable twin of [`record`]: a completed job, the one metered call.
async fn record_metered(db: &Db, org: OrgId, user: UserId, meter: &Meter) {
    let status = meter.warm(org).await;
    let mut tx = db.begin(org).await.unwrap();
    meter
        .charge_completion(&mut tx, user, &status)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_failed_call_is_not_recorded(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = Meter::new(platform.client(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());

    let status = meter.warm(org).await;
    let mut tx = db.begin(org).await.unwrap();
    meter
        .charge_completion(&mut tx, user, &status)
        .await
        .unwrap();
    // The tool fails after charging: its transaction rolls back, meter and all.
    tx.rollback().await.unwrap();

    assert_eq!(outbox::pending(&db).await.unwrap(), 0);
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_recorded_call_ships_once_and_leaves_the_outbox(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = Meter::new(platform.client(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());

    record_metered(&db, org, user, &meter).await;
    record(&db, org, user, &meter, "list_jobs").await;
    assert_eq!(outbox::pending(&db).await.unwrap(), 2);

    let report = outbox::ship_once(&db, &platform.client(), &cfg())
        .await
        .unwrap();
    assert_eq!((report.claimed, report.accepted), (2, 2));

    assert_eq!(outbox::pending(&db).await.unwrap(), 0);
    let counted = platform.counted_usage();
    assert_eq!(counted.len(), 2);
    let billable: Vec<_> = counted
        .iter()
        .map(|e| (e.tool.as_str(), e.billable))
        .collect();
    assert!(billable.contains(&("complete_job", true)));
    assert!(
        billable.contains(&("list_jobs", false)),
        "free calls are recorded too"
    );
    assert!(counted
        .iter()
        .all(|e| e.org_id == org.as_uuid() && e.user_id == Some(user.as_uuid())));

    // Nothing left to ship.
    let report = outbox::ship_once(&db, &platform.client(), &cfg())
        .await
        .unwrap();
    assert_eq!(report.claimed, 0);
    assert_eq!(platform.usage_calls(), 1);
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn an_outage_loses_nothing_and_the_backlog_ships_later_exactly_once(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let client = platform.client();
    let meter = Meter::new(client.clone(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());

    platform.set_down(true);
    // The tools keep working and keep recording while the platform is away.
    for _ in 0..5 {
        record_metered(&db, org, user, &meter).await;
    }
    for _ in 0..3 {
        assert!(outbox::ship_once(&db, &client, &cfg()).await.is_err());
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert_eq!(
        outbox::pending(&db).await.unwrap(),
        5,
        "an outage must not drop rows"
    );
    assert!(platform.counted_usage().is_empty());
    let (attempts, last_error): (i32, Option<String>) =
        sqlx::query_as("SELECT max(attempts), max(last_error) FROM usage_outbox")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(attempts >= 1);
    assert!(
        last_error.is_some(),
        "the failure is recorded for an operator"
    );

    // The platform comes back; the backlog drains.
    platform.set_down(false);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let report = outbox::ship_once(&db, &client, &cfg()).await.unwrap();
    assert_eq!((report.claimed, report.accepted), (5, 5));
    assert_eq!(outbox::pending(&db).await.unwrap(), 0);
    assert_eq!(platform.counted_usage().len(), 5);
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_partial_failure_retries_and_still_counts_each_call_once(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let client = platform.client();
    let meter = Meter::new(client.clone(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    for _ in 0..4 {
        record(&db, org, user, &meter, "claim_jobs").await;
    }

    // The first POST fails, the retry succeeds.
    platform.fail_next_usage_posts(1);
    assert!(outbox::ship_once(&db, &client, &cfg()).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    outbox::ship_once(&db, &client, &cfg()).await.unwrap();
    assert_eq!(platform.counted_usage().len(), 4);

    // A lost acknowledgement: the platform counted the batch but we never heard,
    // so the same rows go again. The platform calls them duplicates.
    let counted = platform.counted_usage();
    sqlx::query(
        "INSERT INTO usage_outbox (event_id, org_id, user_id, tool, billable) \
         SELECT $1, $2, $3, $4, true",
    )
    .bind(counted[0].event_id)
    .bind(org)
    .bind(user)
    .bind("claim_jobs")
    .execute(db.pool())
    .await
    .unwrap();
    let report = outbox::ship_once(&db, &client, &cfg()).await.unwrap();
    assert_eq!((report.accepted, report.duplicates), (0, 1));
    assert_eq!(
        platform.counted_usage().len(),
        4,
        "a resend was counted twice"
    );
    assert_eq!(outbox::pending(&db).await.unwrap(), 0);
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn concurrent_shippers_never_double_count(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let client = platform.client();
    let meter = Meter::new(client.clone(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    for _ in 0..40 {
        record_metered(&db, org, user, &meter).await;
    }

    let slow = ShipperConfig {
        batch: 10,
        claim_lease: Duration::from_secs(30),
        ..cfg()
    };
    let (a, b) = tokio::join!(
        outbox::ship_once(&db, &client, &slow),
        outbox::ship_once(&db, &client, &slow)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(
        a.claimed + b.claimed,
        20,
        "two shippers claimed the same rows"
    );
    assert_eq!(a.duplicates + b.duplicates, 0);

    while outbox::pending(&db).await.unwrap() > 0 {
        outbox::ship_once(&db, &client, &slow).await.unwrap();
    }
    assert_eq!(platform.counted_usage().len(), 40);
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_rejected_event_is_kept_in_a_dead_letter_table_not_dropped(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let client = platform.client();
    let meter = Meter::new(client.clone(), "https://x.test/billing");
    let (gone, user) = (OrgId::new(), UserId::new());
    platform.reject_usage_for(gone.as_uuid());
    record_metered(&db, gone, user, &meter).await;

    let report = outbox::ship_once(&db, &client, &cfg()).await.unwrap();
    assert_eq!(report.rejected, 1);
    assert_eq!(
        outbox::pending(&db).await.unwrap(),
        0,
        "not retried forever"
    );

    let (tool, billable, reason): (String, bool, String) =
        sqlx::query_as("SELECT tool, billable, reason FROM usage_outbox_rejected")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!((tool.as_str(), billable), ("complete_job", true));
    assert!(
        reason.contains("does not exist"),
        "the platform's reason is kept: {reason}"
    );
}

/// A receipt that does not account for every event cannot be trusted to have
/// covered the batch: keep everything and ask again.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_receipt_that_does_not_add_up_deletes_nothing(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let client = platform.client();
    let meter = Meter::new(client.clone(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    for _ in 0..3 {
        record_metered(&db, org, user, &meter).await;
    }

    platform.short_receipts(1);
    assert!(outbox::ship_once(&db, &client, &cfg()).await.is_err());
    assert_eq!(
        outbox::pending(&db).await.unwrap(),
        3,
        "rows were deleted on an unreliable receipt"
    );

    tokio::time::sleep(Duration::from_millis(250)).await;
    let report = outbox::ship_once(&db, &client, &cfg()).await.unwrap();
    assert_eq!(
        (report.claimed, report.duplicates),
        (3, 3),
        "the retry is all duplicates"
    );
    assert_eq!(
        platform.counted_usage().len(),
        3,
        "and nothing was counted twice"
    );
    assert_eq!(outbox::pending(&db).await.unwrap(), 0);
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn the_background_task_drains_the_outbox_and_stops_on_shutdown(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let client = platform.client();
    let meter = Meter::new(client.clone(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    platform.set_down(true);
    for _ in 0..3 {
        record_metered(&db, org, user, &meter).await;
    }

    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(outbox::run(db.clone(), client, cfg(), rx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(outbox::pending(&db).await.unwrap(), 3);

    platform.set_down(false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while outbox::pending(&db).await.unwrap() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the shipper never recovered"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(platform.counted_usage().len(), 3);

    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

// ------------------------------------------------------------- enforcement

const FIX_URL: &str = "https://otto.test";

fn meter(platform: &MockPlatform) -> Meter {
    Meter::new(platform.client(), FIX_URL)
}

/// The tools recorded in the outbox, oldest first.
async fn recorded_tools(db: &Db) -> Vec<String> {
    sqlx::query_scalar("SELECT tool FROM usage_outbox ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

/// Try one completed job the way `complete_job` does (warm, then charge in
/// the transaction), committing whatever it wrote; the error if it was
/// refused.
async fn try_complete(db: &Db, org: OrgId, user: UserId, meter: &Meter) -> Option<BillingError> {
    let status = meter.warm(org).await;
    let mut tx = db.begin(org).await.unwrap();
    let res = meter.charge_completion(&mut tx, user, &status).await;
    tx.commit().await.unwrap();
    res.err()
}

/// The platform's blocked reason is the whole rule: a completed job is refused
/// with that reason as its code, nothing is written for it, and a free call in
/// the same transaction still goes through.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_budget_block_refuses_a_completed_job_and_records_nothing(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = meter(&platform);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_blocked_reason(org.as_uuid(), Some(savvagent_resource::BUDGET_EXCEEDED));

    let status = meter.warm(org).await;
    let mut tx = db.begin(org).await.unwrap();
    let err = meter
        .charge_completion(&mut tx, user, &status)
        .await
        .unwrap_err();
    assert!(matches!(err, BillingError::Blocked { .. }), "{err:?}");
    assert_eq!(err.code(), "budget_exceeded");
    assert!(!err.retriable());
    assert!(err.to_string().contains(FIX_URL), "{err}");
    // Free calls are never refused.
    meter.charge(&mut tx, user, "list_jobs").await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(recorded_tools(&db).await, vec!["list_jobs".to_string()]);
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_payment_block_refuses_a_completed_job_as_payment_required(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = meter(&platform);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_blocked_reason(org.as_uuid(), Some(savvagent_resource::PAYMENT_REQUIRED));

    let err = try_complete(&db, org, user, &meter).await.expect("refused");
    assert_eq!(err.code(), "payment_required");
    assert!(!err.retriable());
    assert!(recorded_tools(&db).await.is_empty());
}

/// A platform that reports the budget's hard stop but no `blocked_reason` (one
/// that predates the field) still blocks, and the refusal has to carry a code
/// an agent can branch on: `budget_exceeded`, which is what tripped.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_tripped_budget_with_no_reason_refuses_as_budget_exceeded(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = meter(&platform);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_budget(org.as_uuid(), Some(tripped_budget()));

    let err = try_complete(&db, org, user, &meter).await.expect("refused");
    assert_eq!(err.code(), "budget_exceeded");
    assert!(recorded_tools(&db).await.is_empty());
}

/// The legacy family-wide bucket no longer decides anything: an org past it on
/// a hard-stop plan, with no blocked reason, completes jobs. (The old rule
/// refused here; now the platform says when to refuse.)
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_spent_legacy_bucket_no_longer_refuses(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = meter(&platform);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_usage(org.as_uuid(), 9_999, 500, true);
    // A budget under its limit does not block either.
    platform.set_budget(
        org.as_uuid(),
        Some(savvagent_resource::BudgetStatus {
            spend_cents: Some(10),
            percent_used: Some(1),
            thresholds_reached: vec![],
            blocked: false,
            reason: None,
            ..tripped_budget()
        }),
    );

    assert!(try_complete(&db, org, user, &meter).await.is_none());
    assert_eq!(recorded_tools(&db).await, vec!["complete_job".to_string()]);
}

/// Free calls are never refused, so they never ask: a free call that waited on
/// the platform would make every coordination call as slow as its worst
/// lookup, for an answer it ignores.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn free_calls_never_ask_the_platform(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = meter(&platform);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_blocked_reason(org.as_uuid(), Some(savvagent_resource::BUDGET_EXCEEDED));

    record(&db, org, user, &meter, "add_job").await;
    record(&db, org, user, &meter, "claim_jobs").await;
    let mut tx = db.begin(org).await.unwrap();
    meter.record_replay(&mut tx, user, "add_job").await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(platform.status_calls(), 0);
    assert_eq!(outbox::pending(&db).await.unwrap(), 3);
}

/// Refusing paying customers' work because a lookup failed is worse than
/// letting it through: the call is still recorded, so nothing is under-billed.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_platform_outage_does_not_block_work_it_only_defers_the_bill(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = meter(&platform);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_blocked_reason(org.as_uuid(), Some(savvagent_resource::BUDGET_EXCEEDED));
    platform.set_down(true);

    assert!(try_complete(&db, org, user, &meter).await.is_none());

    let (billable, meter_key): (bool, Option<String>) =
        sqlx::query_as("SELECT billable, meter FROM usage_outbox")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(billable);
    assert_eq!(meter_key.as_deref(), Some(METER_JOBS_COMPLETED));
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_replay_is_recorded_as_free_and_never_refused(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = meter(&platform);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_blocked_reason(org.as_uuid(), Some(savvagent_resource::BUDGET_EXCEEDED));

    let mut tx = db.begin(org).await.unwrap();
    meter.record_replay(&mut tx, user, "add_job").await.unwrap();
    tx.commit().await.unwrap();

    let billable: bool = sqlx::query_scalar("SELECT billable FROM usage_outbox")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!billable);
}

#[tokio::test]
async fn the_report_surfaces_a_platform_failure_instead_of_inventing_numbers() {
    let platform = MockPlatform::start().await;
    let meter = meter(&platform);
    let org = OrgId::new();
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_usage(org.as_uuid(), 450, 500, true);

    let status = meter.report(org).await.unwrap();
    assert_eq!(
        (status.billable_used, status.remaining, status.warning),
        (450, 50, true)
    );
    assert_eq!(status.plan, "Free");
    assert!(
        status.enforced,
        "the platform's blocked reason is always enforced"
    );

    // A different org the platform has never heard of: an error, not zeros.
    assert!(meter.report(OrgId::new()).await.is_err());
}

/// An outage adds one bounded lookup, not one per call.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn an_outage_is_remembered_so_it_does_not_slow_every_call(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let mut c = savvagent_resource::ClientConfig::new(
        &platform.url,
        tower_testkit::RESOURCE_URI,
        tower_testkit::SECRET,
    );
    c.usage_status_ttl = Duration::from_millis(1);
    let meter = Meter::new(platform.client_with(c), FIX_URL);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_status_down(true);

    for _ in 0..5 {
        assert!(try_complete(&db, org, user, &meter).await.is_none());
    }
    assert_eq!(
        platform.status_calls(),
        1,
        "the first failure is remembered, so the next four never ask"
    );
    platform.set_status_down(false);
    platform.set_blocked_reason(org.as_uuid(), Some(savvagent_resource::BUDGET_EXCEEDED));
    // Still inside the remembered window: allowed without asking.
    assert!(try_complete(&db, org, user, &meter).await.is_none());
    assert_eq!(
        platform.status_calls(),
        1,
        "the failed lookup was not remembered"
    );
    assert_eq!(outbox::pending(&db).await.unwrap(), 6);
}

/// `warm` makes the one network call before a transaction exists, and the
/// charge inside the transaction judges what it returned without asking
/// again — even when the client's cache has already expired, so no lookup can
/// happen while a pooled connection is held.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn the_charge_judges_the_warmed_status_and_never_asks_itself(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let mut c = savvagent_resource::ClientConfig::new(
        &platform.url,
        tower_testkit::RESOURCE_URI,
        tower_testkit::SECRET,
    );
    // Expired before the charge runs: an in-transaction lookup would show.
    c.usage_status_ttl = Duration::from_millis(1);
    let meter = Meter::new(platform.client_with(c), FIX_URL);
    let (org, user) = (OrgId::new(), UserId::new());
    platform.add_org(org.as_uuid(), "acme", "Acme");

    let status = meter.warm(org).await;
    assert!(status.answered());
    assert_eq!(platform.status_calls(), 1);
    tokio::time::sleep(Duration::from_millis(5)).await;
    // Blocked after the warm: the charge judges the answer it was handed.
    platform.set_blocked_reason(org.as_uuid(), Some(savvagent_resource::BUDGET_EXCEEDED));

    let mut tx = db.begin(org).await.unwrap();
    meter
        .charge_completion(&mut tx, user, &status)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        platform.status_calls(),
        1,
        "the charge must not look the org up inside the transaction"
    );

    // And a blocked status handed in is refused, still without asking.
    let blocked = meter.warm(org).await;
    let mut tx = db.begin(org).await.unwrap();
    let err = meter
        .charge_completion(&mut tx, user, &blocked)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "budget_exceeded");
    assert_eq!(platform.status_calls(), 2);
}

// ------------------------------------------------------------------- meters

async fn enqueue_metered(db: &Db, org: OrgId, user: UserId, metered: Option<&Metered>) {
    let mut tx = db.begin(org).await.unwrap();
    outbox::enqueue(&mut tx, Some(user), "complete_job", true, metered)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_metered_row_ships_its_meter_and_quantity(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let (org, user) = (OrgId::new(), UserId::new());
    enqueue_metered(&db, org, user, Some(&Metered::jobs_completed())).await;
    enqueue_metered(&db, org, user, None).await;

    outbox::ship_once(&db, &platform.client(), &cfg())
        .await
        .unwrap();

    let counted = platform.counted_usage();
    assert_eq!(counted.len(), 2);
    let metered: Vec<_> = counted.iter().filter(|e| e.meter.is_some()).collect();
    assert_eq!(metered.len(), 1);
    assert_eq!(
        metered[0].meter.as_deref(),
        Some("otto-tower/jobs_completed")
    );
    assert_eq!(metered[0].quantity, Some(1));
    let plain = counted.iter().find(|e| e.meter.is_none()).unwrap();
    assert_eq!(plain.quantity, None, "an unmetered row carries no quantity");
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_rejected_metered_row_keeps_its_meter_and_quantity(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let (gone, user) = (OrgId::new(), UserId::new());
    platform.reject_usage_for(gone.as_uuid());
    enqueue_metered(&db, gone, user, Some(&Metered::jobs_completed())).await;

    let report = outbox::ship_once(&db, &platform.client(), &cfg())
        .await
        .unwrap();
    assert_eq!(report.rejected, 1);

    let (meter, quantity): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT meter, quantity FROM usage_outbox_rejected")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(meter.as_deref(), Some(METER_JOBS_COMPLETED));
    assert_eq!(quantity, Some(1));
}

/// A meter without a quantity (or the reverse) would ship a half-formed event;
/// a zero or negative quantity would not be a count. The schema refuses both.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn the_schema_refuses_a_half_metered_or_non_positive_row(pool: PgPool) {
    for (meter, quantity) in [
        (Some("otto-tower/jobs_completed"), None),
        (None, Some(1_i64)),
        (Some("otto-tower/jobs_completed"), Some(0)),
        (Some("otto-tower/jobs_completed"), Some(-1)),
    ] {
        for table in ["usage_outbox", "usage_outbox_rejected"] {
            let sql = if table == "usage_outbox" {
                "INSERT INTO usage_outbox (org_id, tool, billable, meter, quantity) \
                 VALUES ($1, 't', true, $2, $3)"
            } else {
                "INSERT INTO usage_outbox_rejected \
                   (event_id, org_id, tool, billable, occurred_at, reason, meter, quantity) \
                 VALUES (gen_random_uuid(), $1, 't', true, now(), 'r', $2, $3)"
            };
            let res = sqlx::query(sql)
                .bind(OrgId::new().as_uuid())
                .bind(meter)
                .bind(quantity)
                .execute(&pool)
                .await;
            assert!(res.is_err(), "{table} accepted {meter:?}/{quantity:?}");
        }
    }
}

// ------------------------------------------------------------------ declaration

/// Every meter this crate can record must be declared before shipping, or the
/// platform dead-letters its rows as `unknown_meter`. Add each new `Metered`
/// constructor here.
#[test]
fn every_recordable_meter_is_declared() {
    let recordable = [Metered::jobs_completed()];
    for m in recordable {
        assert!(
            tower_billing::METER_DECLARATIONS
                .iter()
                .any(|(key, _, _)| *key == m.meter()),
            "{} is recorded but not in METER_DECLARATIONS",
            m.meter()
        );
    }
}

// ------------------------------------------------------------ retired meters

const RETIRED: &str = "otto-factory/jobs_completed";

/// A retired key's successor must be declared, or the rewrite only moves rows
/// from one dead letter to another; and a key that is still declared was never
/// retired. `0006_meter_otto_tower.sql`'s rename must be on the list.
#[test]
fn every_retired_meter_moves_to_a_declared_one() {
    let declared = |key: &str| {
        tower_billing::METER_DECLARATIONS
            .iter()
            .any(|(declared, _, _)| *declared == key)
    };
    for (old, new) in tower_billing::RETIRED_METERS {
        assert!(declared(new), "{old} is mapped to undeclared {new}");
        assert!(!declared(old), "{old} is retired but still declared");
    }
    assert!(tower_billing::RETIRED_METERS.contains(&(RETIRED, METER_JOBS_COMPLETED)));
}

/// What an image that predates the rename writes: a completion under the old
/// key. Raw SQL, because this binary can no longer produce one.
async fn enqueue_retired(pool: &PgPool, org: OrgId) {
    sqlx::query(
        "INSERT INTO usage_outbox (org_id, tool, billable, meter, quantity) \
         VALUES ($1, 'complete_job', true, $2, 1)",
    )
    .bind(org.as_uuid())
    .bind(RETIRED)
    .execute(pool)
    .await
    .unwrap();
}

async fn meters(pool: &PgPool, org: OrgId) -> Vec<Option<String>> {
    sqlx::query_scalar("SELECT meter FROM usage_outbox WHERE org_id = $1 ORDER BY id")
        .bind(org.as_uuid())
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Two versions running at once: a replica of the older image keeps writing
/// the old key after this one's startup rewrite has run. The shipper still
/// bills it under the successor, never under a meter the platform refuses.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_row_enqueued_under_a_retired_key_after_startup_ships_under_its_successor(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool.clone());
    let org = OrgId::new();

    assert_eq!(outbox::rewrite_retired_meters(&db).await.unwrap(), 0);
    enqueue_retired(&pool, org).await;

    let report = outbox::ship_once(&db, &platform.client(), &cfg())
        .await
        .unwrap();
    assert_eq!(
        (report.claimed, report.accepted, report.rejected),
        (1, 1, 0)
    );
    let counted = platform.counted_usage();
    assert_eq!(counted.len(), 1);
    assert_eq!(counted[0].meter.as_deref(), Some(METER_JOBS_COMPLETED));
    assert_eq!(counted[0].quantity, Some(1));
    assert_eq!(outbox::pending(&db).await.unwrap(), 0);
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn the_startup_rewrite_moves_every_orgs_rows_when_rls_is_bypassed(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let (a, b, user) = (OrgId::new(), OrgId::new(), UserId::new());
    for org in [a, b] {
        enqueue_retired(&pool, org).await;
        enqueue_retired(&pool, org).await;
        enqueue_metered(&db, org, user, None).await;
    }

    assert_eq!(outbox::rewrite_retired_meters(&db).await.unwrap(), 4);
    for org in [a, b] {
        let want = vec![
            Some(METER_JOBS_COMPLETED.to_string()),
            Some(METER_JOBS_COMPLETED.to_string()),
            None,
        ];
        assert_eq!(meters(&pool, org).await, want, "org {org:?}");
    }
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn the_startup_rewrite_moves_every_orgs_rows_when_rls_applies(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let (a, b) = (OrgId::new(), OrgId::new());
    for org in [a, b] {
        enqueue_retired(&pool, org).await;
    }

    // As the non-owner role RLS binds, with no app.org_id: the discovery read
    // goes through the policy's org-less branch, and each UPDATE through the
    // org the loop pins.
    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    let moved = outbox::rewrite_retired_meters_in(tx.conn()).await.unwrap();
    // Nothing later in the same transaction inherits the last org pinned.
    let left: String = sqlx::query_scalar("SELECT current_setting('app.org_id', true)")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(moved, 2);
    assert_eq!(left, "");
    for org in [a, b] {
        assert_eq!(
            meters(&pool, org).await,
            vec![Some(METER_JOBS_COMPLETED.to_string())]
        );
    }
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn the_startup_rewrite_is_idempotent_and_leaves_rejected_history(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let org = OrgId::new();
    enqueue_retired(&pool, org).await;
    sqlx::query(
        "INSERT INTO usage_outbox_rejected (event_id, org_id, tool, billable, occurred_at, reason, meter, quantity) \
         VALUES (gen_random_uuid(), $1, 'complete_job', true, now(), 'x', $2, 1)",
    )
    .bind(org.as_uuid())
    .bind(RETIRED)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(outbox::rewrite_retired_meters(&db).await.unwrap(), 1);
    assert_eq!(outbox::rewrite_retired_meters(&db).await.unwrap(), 0);

    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_outbox_rejected WHERE org_id = $1 AND meter = $2",
    )
    .bind(org.as_uuid())
    .bind(RETIRED)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, 1);
}

/// Poll `f` until it holds or five seconds pass.
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn spawn_shipper(
    db: &Db,
    platform: &MockPlatform,
    cfg: ShipperConfig,
) -> (
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(outbox::run(db.clone(), platform.client(), cfg, rx));
    (stop, task)
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn the_shipper_declares_the_meter_before_its_first_usage_post(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = Meter::new(platform.client(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    record_metered(&db, org, user, &meter).await;

    let (stop, task) = spawn_shipper(&db, &platform, cfg());
    eventually("the row to ship", || !platform.counted_usage().is_empty()).await;

    let declared = platform.declared_meters();
    assert_eq!(declared.len(), 1, "{declared:?}");
    assert_eq!(declared[0].key, "otto-tower/jobs_completed");
    assert_eq!(declared[0].display_name, "Jobs completed");
    assert_eq!(declared[0].unit, "job");
    assert_eq!(
        platform.usage_posts_before_declaration(),
        0,
        "usage was posted before the meter was declared"
    );

    // A second batch ships without declaring again: once per process.
    record_metered(&db, org, user, &meter).await;
    eventually("the second row to ship", || {
        platform.counted_usage().len() == 2
    })
    .await;
    assert_eq!(platform.meter_calls(), 1, "declared more than once");

    stop.send(true).unwrap();
    task.await.unwrap();
}

/// In `mode` nothing ships and the row stays queued; once the platform
/// recovers the shipper declares and drains.
async fn a_failed_declaration_holds_shipping_back(pool: PgPool, mode: MeterMode) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = Meter::new(platform.client(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    record_metered(&db, org, user, &meter).await;
    record(&db, org, user, &meter, "list_jobs").await;
    platform.set_meter_mode(mode);

    let (stop, task) = spawn_shipper(&db, &platform, cfg());
    eventually("a second declaration attempt", || {
        platform.meter_calls() >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(platform.usage_calls(), 0, "{mode:?}: usage was posted");
    assert!(platform.declared_meters().is_empty(), "{mode:?}");
    assert_eq!(outbox::pending(&db).await.unwrap(), 2, "{mode:?}");

    platform.set_meter_mode(MeterMode::Accept);
    eventually("the outbox to drain", || {
        platform.counted_usage().len() == 2
    })
    .await;
    assert_eq!(platform.declared_meters().len(), 1);
    assert_eq!(platform.usage_posts_before_declaration(), 0);
    // The shipper deletes after the platform counts; wait for the delete.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while outbox::pending(&db).await.unwrap() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{mode:?}: rows linger"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    stop.send(true).unwrap();
    task.await.unwrap();
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn service_not_set_holds_shipping_back(pool: PgPool) {
    a_failed_declaration_holds_shipping_back(pool, MeterMode::ServiceNotSet).await;
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn an_outage_holds_shipping_back(pool: PgPool) {
    a_failed_declaration_holds_shipping_back(pool, MeterMode::Outage).await;
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_rejected_meter_holds_shipping_back(pool: PgPool) {
    a_failed_declaration_holds_shipping_back(pool, MeterMode::Reject).await;
}

#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn shutdown_while_waiting_to_declare_returns_promptly(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = Meter::new(platform.client(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    record_metered(&db, org, user, &meter).await;
    platform.set_meter_mode(MeterMode::Outage);

    // A retry delay far longer than the test: only shutdown can end the wait.
    let slow = ShipperConfig {
        base_backoff: Duration::from_secs(300),
        ..cfg()
    };
    let (stop, task) = spawn_shipper(&db, &platform, slow);
    eventually("the first declaration attempt", || {
        platform.meter_calls() >= 1
    })
    .await;

    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the shipper did not stop during the declaration wait")
        .unwrap();
    assert_eq!(platform.usage_calls(), 0);
    assert_eq!(outbox::pending(&db).await.unwrap(), 1);
}

/// The platform losing the declaration mid-run must not dead-letter billed
/// work: the shipper declares again and the rows ship.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn an_unknown_meter_makes_the_shipper_redeclare_and_ship_the_rows(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = Meter::new(platform.client(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    record_metered(&db, org, user, &meter).await;

    let (stop, task) = spawn_shipper(&db, &platform, cfg());
    eventually("the first row to ship", || {
        platform.counted_usage().len() == 1
    })
    .await;
    assert_eq!(platform.meter_calls(), 1);

    platform.forget_meters();
    record_metered(&db, org, user, &meter).await;
    record_metered(&db, org, user, &meter).await;
    eventually("the rows to ship after re-declaring", || {
        platform.counted_usage().len() == 3
    })
    .await;

    assert_eq!(platform.declared_meters().len(), 1);
    assert!(platform.meter_calls() >= 2, "never declared again");
    let rejected: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_outbox_rejected")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rejected, 0, "an unknown_meter row was dead-lettered");

    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(outbox::pending(&db).await.unwrap(), 0);
}

/// A platform that keeps refusing a meter it was just told about never costs
/// billed usage: the shipper keeps re-declaring, and the rows are neither
/// dead-lettered nor dropped. They stay queued for when the platform recovers.
#[sqlx::test(migrator = "tower_core::MIGRATOR")]
async fn a_platform_that_always_refuses_is_redeclared_forever_and_the_rows_stay_queued(
    pool: PgPool,
) {
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool);
    let meter = Meter::new(platform.client(), "https://x.test/billing");
    let (org, user) = (OrgId::new(), UserId::new());
    platform.refuse_every_meter();
    record_metered(&db, org, user, &meter).await;

    let (stop, task) = spawn_shipper(&db, &platform, cfg());
    // Well past the old bound of three re-declares.
    eventually("many re-declarations", || platform.meter_calls() >= 7).await;

    let rejected: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_outbox_rejected")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rejected, 0, "an unknown_meter row was dead-lettered");
    assert_eq!(outbox::pending(&db).await.unwrap(), 1, "the row was lost");
    assert!(platform.counted_usage().is_empty());

    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(outbox::pending(&db).await.unwrap(), 1);
}
