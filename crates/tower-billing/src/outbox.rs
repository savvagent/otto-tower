//! The usage outbox: how a metered call reaches the platform.
//!
//! A call is recorded by [`enqueue`], inside the tool's own transaction, as a
//! row in `usage_outbox` with a freshly minted `event_id`. [`run`] is the
//! background task that drains the table: it claims due rows, posts them to the
//! platform with `PlatformClient::ship_usage`, and deletes what the platform has
//! accounted for.
//!
//! ## Why this is exactly-once without being clever
//!
//! The platform dedupes on `(resource server, event_id)`, and `event_id` is fixed
//! when the row is written. So the shipper may retry anything, any number of
//! times, from any number of replicas, and the platform counts each call once.
//! The shipper's only job is to never *lose* a row, which it guarantees by
//! deleting only rows the platform has answered for: every event in a successful
//! response is exactly one of accepted, duplicate, or rejected, and all three are
//! final. A failed or unreadable response deletes nothing.
//!
//! ## Backoff and replicas
//!
//! `next_attempt_at` is both the retry backoff and a claim lease. Claiming a row
//! pushes it into the future (so a second replica skips it, via `FOR UPDATE SKIP
//! LOCKED`); success deletes it; failure re-schedules it with exponential
//! backoff. A replica that dies mid-ship leaves rows that simply become due
//! again when their lease lapses. Running the shipper in every replica is safe.
//!
//! ## Outages
//!
//! While the platform is down rows accumulate and nothing else is affected: the
//! tools keep working and keep recording. When it returns they drain, oldest
//! first.

use std::time::Duration;

use savvagent_resource::{MeterDeclaration, MeterDeclarationReceipt, PlatformClient, UsageEvent};
use savvagent_tenant::ids::UserId;
use savvagent_tenant::{Db, Tx};
use sqlx::FromRow;
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::Result;

/// The platform meter key for a completed job.
pub const METER_JOBS_COMPLETED: &str = "otto-tower/jobs_completed";

/// The meters this server declares to the platform before it ships usage, as
/// `(key, display name, unit)`.
///
/// The platform rejects a metered event for an undeclared meter as
/// `unknown_meter`, so [`run`] declares these first (and again whenever the
/// platform refuses events as `unknown_meter`; those rows stay queued). Every `Metered::meter()` this crate records must be listed here.
///
/// ```
/// let (key, display, unit) = tower_billing::METER_DECLARATIONS[0];
/// assert_eq!((key, display, unit), (tower_billing::METER_JOBS_COMPLETED, "Jobs completed", "job"));
/// ```
pub const METER_DECLARATIONS: &[(&str, &str, &str)] =
    &[(METER_JOBS_COMPLETED, "Jobs completed", "job")];

/// Every meter key that has been renamed, as `(retired, successor)`. Each
/// successor must be in [`METER_DECLARATIONS`] and no retired key may be; a test
/// holds the list to both.
///
/// A rename's migration (`0006_meter_otto_tower.sql` for this one) rewrites the
/// rows queued when it runs, once. That is not enough: an image that predates
/// the rename keeps enqueuing completions under the retired key — after a
/// rollback past the migration, or while it is still running beside a newer
/// one during a deploy — and the platform refuses an undeclared meter as
/// `unknown_meter`, which is final. So the retired key never reaches the
/// platform from this binary: [`ship_once`] maps every row it ships through
/// [`current_meter`], and [`rewrite_retired_meters`] (run by `tower-server` at
/// startup, before the shipper) moves the queued rows themselves, so that what
/// is in the table says what will be billed.
///
/// ```
/// assert_eq!(
///     tower_billing::outbox::current_meter("otto-factory/jobs_completed"),
///     tower_billing::METER_JOBS_COMPLETED
/// );
/// assert_eq!(
///     tower_billing::outbox::current_meter(tower_billing::METER_JOBS_COMPLETED),
///     tower_billing::METER_JOBS_COMPLETED
/// );
/// ```
pub const RETIRED_METERS: &[(&str, &str)] =
    &[("otto-factory/jobs_completed", METER_JOBS_COMPLETED)];

/// The key a meter is billed under today: its successor if it was renamed,
/// itself otherwise.
pub fn current_meter(key: &str) -> &str {
    RETIRED_METERS
        .iter()
        .find(|(retired, _)| *retired == key)
        .map_or(key, |(_, successor)| successor)
}

/// A value meter and quantity carried on a usage row, beside the per-call record.
///
/// Built only by its constructors, so a row can carry only a meter this crate
/// declares ([`METER_DECLARATIONS`]) and a positive quantity (the schema also
/// refuses anything else).
///
/// ```
/// let m = tower_billing::Metered::jobs_completed();
/// assert_eq!((m.meter(), m.quantity()), (tower_billing::METER_JOBS_COMPLETED, 1));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metered {
    meter: &'static str,
    quantity: i64,
}

impl Metered {
    /// One completed job.
    pub const fn jobs_completed() -> Self {
        Self {
            meter: METER_JOBS_COMPLETED,
            quantity: 1,
        }
    }

    /// The meter's key, `<service>/<meter>`.
    pub const fn meter(&self) -> &'static str {
        self.meter
    }

    /// The quantity, always positive.
    pub const fn quantity(&self) -> i64 {
        self.quantity
    }
}

/// Record one metered call in the caller's transaction.
///
/// Returns the call's `event_id`. The row commits or rolls back with `tx`, which
/// is what makes "a failed call is not billed" true. `metered` adds a value
/// meter and quantity to the row; `None` records a plain call.
///
/// A completed job is never recorded with this directly: it goes through
/// [`crate::completion::record_job_completed`], which owns what a completion
/// writes.
pub async fn enqueue(
    tx: &mut Tx<'_>,
    user: Option<UserId>,
    tool: &str,
    billable: bool,
    metered: Option<&Metered>,
) -> Result<Uuid> {
    let org = tx.org();
    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable, meter, quantity) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING event_id",
    )
    .bind(org)
    .bind(user)
    .bind(tool)
    .bind(billable)
    .bind(metered.map(|m| m.meter))
    .bind(metered.map(|m| m.quantity))
    .fetch_one(tx.conn())
    .await?;
    Ok(event_id)
}

/// Shipper tuning. [`Default`] is right for production; tests shorten it.
#[derive(Debug, Clone)]
pub struct ShipperConfig {
    /// Rows per request. Never more than the platform accepts per batch.
    pub batch: usize,
    /// How long to sleep when there is nothing to ship.
    pub poll: Duration,
    /// How long a claim holds a row before another replica may take it.
    pub claim_lease: Duration,
    /// First retry delay for a failed row; doubles per attempt.
    pub base_backoff: Duration,
    /// Ceiling for the per-row retry delay.
    pub max_backoff: Duration,
    /// Ceiling for the delay between meter declaration attempts, which start at
    /// `base_backoff` and double.
    pub declare_max_backoff: Duration,
}

impl Default for ShipperConfig {
    fn default() -> Self {
        Self {
            batch: savvagent_resource::MAX_USAGE_BATCH,
            poll: Duration::from_secs(5),
            claim_lease: Duration::from_secs(120),
            base_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(15 * 60),
            declare_max_backoff: Duration::from_secs(60),
        }
    }
}

/// Whether the shipper has declared its meters, readable from outside the task.
///
/// The shipper holds back all usage while the declaration is refused, and its
/// only other signal is a log line. Clone one into [`run_with_status`] and read
/// it from wherever the deployment looks (`/readyz`). Cloning shares the state.
#[derive(Debug, Clone, Default)]
pub struct ShipperStatus {
    declared: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ShipperStatus {
    /// `false` until the first successful declaration of this process.
    pub fn meters_declared(&self) -> bool {
        self.declared.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn set_declared(&self, declared: bool) {
        self.declared
            .store(declared, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ShipReport {
    pub claimed: usize,
    pub accepted: u32,
    pub duplicates: u32,
    /// Events the platform refused for good and that moved to the dead letter.
    pub rejected: usize,
    /// Events refused as `unknown_meter` and kept queued with backoff, never
    /// dead-lettered: the caller re-declares the meters and they are retried.
    pub unknown_meter: usize,
}

/// Whether a rejection reason is the platform's `unknown_meter` code. Reasons
/// read code first, then a colon and detail (`unknown_meter: x`).
fn is_unknown_meter(reason: &str) -> bool {
    reason.split(':').next().map(str::trim) == Some("unknown_meter")
}

#[derive(Debug, FromRow)]
struct Row {
    id: i64,
    event_id: Uuid,
    org_id: Uuid,
    user_id: Option<Uuid>,
    tool: String,
    billable: bool,
    occurred_at: chrono::DateTime<chrono::Utc>,
    meter: Option<String>,
    quantity: Option<i64>,
}

/// Claim up to a batch of due rows, ship them, and delete what the platform
/// answered for.
///
/// On any shipping error nothing is deleted: the rows are rescheduled with
/// backoff and the error is returned for the caller's loop to pace itself on.
///
/// An `unknown_meter` refusal is never final: the platform lost the
/// declaration, not the event, so the row stays queued with capped backoff and
/// is counted in [`ShipReport::unknown_meter`] for [`run`] to re-declare.
pub async fn ship_once(
    db: &Db,
    platform: &PlatformClient,
    cfg: &ShipperConfig,
) -> std::result::Result<ShipReport, ShipError> {
    ship_batch(db, platform, cfg).await
}

/// The body of [`ship_once`].
async fn ship_batch(
    db: &Db,
    platform: &PlatformClient,
    cfg: &ShipperConfig,
) -> std::result::Result<ShipReport, ShipError> {
    // Claim, on the pool, unpinned: the shipper serves every org. The table's
    // policy lets an unpinned statement see all orgs' rows and no pinned one see
    // another's (see the baseline migration).
    let rows: Vec<Row> = sqlx::query_as(
        "UPDATE usage_outbox SET attempts = attempts + 1, \
                next_attempt_at = now() + make_interval(secs => $2) \
         WHERE id IN ( \
             SELECT id FROM usage_outbox WHERE next_attempt_at <= now() \
             ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED) \
         RETURNING id, event_id, org_id, user_id, tool, billable, occurred_at, \
                   meter, quantity",
    )
    .bind(cfg.batch as i64)
    .bind(cfg.claim_lease.as_secs_f64())
    .fetch_all(db.pool())
    .await?;

    if rows.is_empty() {
        return Ok(ShipReport::default());
    }

    let mut ordered = rows;
    ordered.sort_by_key(|r| r.id);
    let ids: Vec<i64> = ordered.iter().map(|r| r.id).collect();
    let events: Vec<UsageEvent> = ordered
        .iter()
        .map(|r| {
            // The schema guarantees a positive quantity; a row that says
            // otherwise is corruption, surfaced rather than shipped.
            let quantity = r
                .quantity
                .map(|q| {
                    u64::try_from(q).map_err(|_| {
                        sqlx::Error::Decode(
                            format!("usage row {} has a negative quantity {q}", r.id).into(),
                        )
                    })
                })
                .transpose()?;
            Ok(UsageEvent {
                event_id: r.event_id,
                org_id: r.org_id,
                user_id: r.user_id,
                tool: r.tool.clone(),
                billable: r.billable,
                occurred_at: r.occurred_at,
                // The platform takes the service from the calling resource server.
                service: None,
                // A row an older image enqueued under a retired key is billed
                // under its successor (see `RETIRED_METERS`).
                meter: r.meter.as_deref().map(|m| current_meter(m).to_string()),
                quantity,
            })
        })
        .collect::<std::result::Result<_, sqlx::Error>>()?;

    match platform.ship_usage(&events).await {
        Ok(receipt) => {
            // Every event must be in exactly one of the three outcomes. If the
            // counts do not add up the response is not to be trusted to have
            // accounted for the whole batch, so nothing is deleted: the rows
            // are retried (the platform dedupes) and the mismatch is loud.
            let answered =
                receipt.accepted as usize + receipt.duplicates as usize + receipt.rejected.len();
            if answered != ids.len() {
                let msg = format!(
                    "the platform accounted for {answered} of {} usage events (accepted {}, duplicate {}, rejected {})",
                    ids.len(),
                    receipt.accepted,
                    receipt.duplicates,
                    receipt.rejected.len()
                );
                tracing::error!("{msg}; keeping every row and retrying");
                let err = savvagent_resource::Error::Decode(msg);
                reschedule(db, &ids, &err, cfg).await;
                return Err(ShipError::Platform(err));
            }

            // Accepted and duplicate are final. Rejected is final too, but it is
            // money nobody was told about, so it is kept in a dead-letter table
            // with the platform's reason, in the same transaction that removes it
            // from the outbox -- never dropped silently.
            let (held, rejected): (Vec<_>, Vec<_>) = receipt
                .rejected
                .iter()
                .partition(|r| is_unknown_meter(&r.reason));
            let held_events: Vec<Uuid> = held.iter().map(|r| r.event_id).collect();
            let mut tx = db.pool().begin().await?;
            for r in &rejected {
                tracing::error!(
                    event_id = %r.event_id,
                    reason = %r.reason,
                    "the platform refused a usage event; moving it to usage_outbox_rejected"
                );
                sqlx::query(
                    "INSERT INTO usage_outbox_rejected \
                       (event_id, org_id, user_id, tool, billable, occurred_at, reason, \
                        meter, quantity) \
                     SELECT event_id, org_id, user_id, tool, billable, occurred_at, $2, \
                            meter, quantity \
                     FROM usage_outbox WHERE event_id = $1",
                )
                .bind(r.event_id)
                .bind(&r.reason)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query("DELETE FROM usage_outbox WHERE id = ANY($1) AND NOT event_id = ANY($2)")
                .bind(&ids)
                .bind(&held_events)
                .execute(&mut *tx)
                .await?;
            if !held_events.is_empty() {
                sqlx::query(
                    "UPDATE usage_outbox SET last_error = $2, \
                            next_attempt_at = now() + make_interval(secs => \
                                LEAST($3::float8 * power(2, LEAST(attempts - 1, 20)), $4::float8)) \
                     WHERE event_id = ANY($1)",
                )
                .bind(&held_events)
                .bind("unknown_meter: the platform has no declaration for this meter")
                .bind(cfg.base_backoff.as_secs_f64())
                .bind(cfg.max_backoff.as_secs_f64())
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Ok(ShipReport {
                claimed: ids.len(),
                accepted: receipt.accepted,
                duplicates: receipt.duplicates,
                rejected: rejected.len(),
                unknown_meter: held_events.len(),
            })
        }
        Err(e) => {
            reschedule(db, &ids, &e, cfg).await;
            Err(ShipError::Platform(e))
        }
    }
}

/// Put exactly these rows back with exponential backoff and the failure
/// recorded. `attempts` was already bumped by the claim, so the delay doubles
/// per consecutive failure.
async fn reschedule(db: &Db, ids: &[i64], e: &savvagent_resource::Error, cfg: &ShipperConfig) {
    let rescheduled = sqlx::query(
        "UPDATE usage_outbox SET last_error = $2, \
                next_attempt_at = now() + make_interval(secs => \
                    LEAST($3::float8 * power(2, LEAST(attempts - 1, 20)), $4::float8)) \
         WHERE id = ANY($1)",
    )
    .bind(ids)
    .bind(truncate(&e.to_string(), 500))
    .bind(cfg.base_backoff.as_secs_f64())
    .bind(cfg.max_backoff.as_secs_f64())
    .execute(db.pool())
    .await;
    if let Err(re) = rescheduled {
        // The claim lease still holds the rows back, so this only delays the
        // retry; it cannot lose anything.
        tracing::warn!(error = %re, "could not record a usage shipping failure");
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Why a shipping pass failed.
#[derive(Debug, thiserror::Error)]
pub enum ShipError {
    #[error("the platform did not accept the batch: {0}")]
    Platform(#[from] savvagent_resource::Error),
    #[error("outbox database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Move every queued row still on a [`RETIRED_METERS`] key to its successor, for
/// every org, in one transaction. Returns how many rows moved. Idempotent: with
/// nothing on a retired key it reads the outbox and writes nothing.
///
/// [`ship_once`] already ships such a row under its successor, so this is not
/// what keeps billing right; it keeps the table honest about what will be
/// billed, and it runs at startup so a rollback's leftovers are visible once,
/// in one log line, rather than row by row.
///
/// It rewrites tenant data with no org of its own, so it follows the one
/// pattern that is safe under every deployment shape (CLAUDE.md, "A migration
/// that touches a tenant table's data"): for each org that has such rows it
/// pins `app.org_id` to that org **and** names it in the `UPDATE`'s own
/// predicate. Finding the orgs is an org-less read, which
/// `usage_outbox_tenant_isolation`'s `current_org() IS NULL` branch allows —
/// the branch [`ship_once`]'s claim uses too.
pub async fn rewrite_retired_meters(db: &Db) -> std::result::Result<u64, sqlx::Error> {
    let mut tx = db.pool().begin().await?;
    let moved = rewrite_retired_meters_in(&mut tx).await?;
    tx.commit().await?;
    Ok(moved)
}

/// [`rewrite_retired_meters`] on a connection the caller holds, inside whatever
/// transaction it has open — which is how a test runs it as a role RLS applies
/// to. `app.org_id` is set transaction-locally and restored to unset before
/// this returns, so nothing after it in the same transaction inherits an org.
pub async fn rewrite_retired_meters_in(
    conn: &mut sqlx::PgConnection,
) -> std::result::Result<u64, sqlx::Error> {
    let mut moved = 0;
    for (retired, successor) in RETIRED_METERS {
        // Unpinned for the discovery read, or on a shape RLS binds it would
        // see only the org the previous key's loop left pinned.
        unpin(conn).await?;
        let orgs: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT org_id FROM usage_outbox WHERE meter = $1 ORDER BY org_id",
        )
        .bind(retired)
        .fetch_all(&mut *conn)
        .await?;
        for org in orgs {
            sqlx::query("SELECT set_config('app.org_id', $1, true)")
                .bind(org.to_string())
                .execute(&mut *conn)
                .await?;
            moved +=
                sqlx::query("UPDATE usage_outbox SET meter = $3 WHERE org_id = $1 AND meter = $2")
                    .bind(org)
                    .bind(retired)
                    .bind(successor)
                    .execute(&mut *conn)
                    .await?
                    .rows_affected();
        }
    }
    unpin(conn).await?;
    Ok(moved)
}

/// Restore `app.org_id` to unset for the rest of the transaction.
async fn unpin(conn: &mut sqlx::PgConnection) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('app.org_id', '', true)")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Rows waiting to be shipped (due or backing off).
pub async fn pending(db: &Db) -> std::result::Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM usage_outbox")
        .fetch_one(db.pool())
        .await
}

/// How often a stuck declaration is logged.
const DECLARE_LOG_EVERY: Duration = Duration::from_secs(60);

/// Why a declaration attempt did not finish the job, for the log.
fn declaration_failure(
    result: std::result::Result<MeterDeclarationReceipt, savvagent_resource::Error>,
) -> Option<String> {
    match result {
        Ok(receipt) if receipt.is_complete() => None,
        Ok(receipt) => Some(
            receipt
                .rejected
                .iter()
                .map(|r| format!("{} refused: {}", r.key, r.reason))
                .collect::<Vec<_>>()
                .join("; "),
        ),
        Err(e) => Some(e.to_string()),
    }
}

/// Declare [`METER_DECLARATIONS`] with the platform, retrying with exponential
/// backoff until it is complete. Returns `false` if `shutdown` came first.
///
/// Done means `Ok` with a receipt that [`is_complete`](MeterDeclarationReceipt::is_complete);
/// a transport error, a 5xx, a `409 service_not_set`, or a meter in `rejected`
/// all retry. Nothing ships meanwhile and the outbox keeps every row, so a
/// platform that cannot take the declaration delays billing and loses none. Once
/// a minute, and on the first failure, it logs at `error` with the platform's
/// reason and the number of rows waiting.
async fn declare_meters(
    db: &Db,
    platform: &PlatformClient,
    cfg: &ShipperConfig,
    shutdown: &mut watch::Receiver<bool>,
    status: &ShipperStatus,
) -> bool {
    status.set_declared(false);
    let declarations: Vec<MeterDeclaration> = METER_DECLARATIONS
        .iter()
        .map(|(key, display, unit)| MeterDeclaration::new(*key, *display, *unit))
        .collect();
    let mut attempt: u32 = 0;
    let mut last_logged: Option<tokio::time::Instant> = None;
    loop {
        if *shutdown.borrow() {
            return false;
        }
        let result = tokio::select! {
            r = platform.declare_meters(&declarations) => r,
            _ = shutdown.changed() => return false,
        };
        let Some(reason) = declaration_failure(result) else {
            tracing::info!(meters = declarations.len(), "declared usage meters");
            status.set_declared(true);
            return true;
        };

        if last_logged.is_none_or(|t| t.elapsed() >= DECLARE_LOG_EVERY) {
            last_logged = Some(tokio::time::Instant::now());
            let waiting = match pending(db).await {
                Ok(n) => n.to_string(),
                Err(e) => format!("unknown ({e})"),
            };
            tracing::error!(
                %reason,
                rows_waiting = %waiting,
                "could not declare the usage meters; no usage ships until they are declared \
                 (rows stay queued and nothing is lost). Retrying"
            );
        }
        let wait = cfg
            .base_backoff
            .saturating_mul(1u32 << attempt.min(20))
            .min(cfg.declare_max_backoff);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => return false,
        }
    }
}

/// Declare the meters, then drain the outbox until `shutdown` flips to `true`.
///
/// Nothing ships until [`METER_DECLARATIONS`] are declared (see
/// [`declare_meters`]): a metered event for an undeclared meter would be
/// rejected and dead-lettered. If shutdown arrives while declaring, the task
/// returns at once without a final pass, for the same reason; the rows stay
/// queued for the next process.
///
/// Once shipping, a final pass runs on the way out so a clean shutdown does
/// not strand rows that were one request from delivered.
pub async fn run(
    db: Db,
    platform: std::sync::Arc<PlatformClient>,
    cfg: ShipperConfig,
    shutdown: watch::Receiver<bool>,
) {
    run_with_status(db, platform, cfg, shutdown, ShipperStatus::default()).await
}

/// [`run`], reporting whether the meters are declared through `status`.
pub async fn run_with_status(
    db: Db,
    platform: std::sync::Arc<PlatformClient>,
    cfg: ShipperConfig,
    mut shutdown: watch::Receiver<bool>,
    status: ShipperStatus,
) {
    tracing::info!("usage shipper started");
    if !declare_meters(&db, &platform, &cfg, &mut shutdown, &status).await {
        tracing::warn!("usage shipper stopped before the meters were declared; rows stay queued");
        return;
    }
    let mut failures: u32 = 0;
    loop {
        let wait = match ship_batch(&db, &platform, &cfg).await {
            Ok(report) => {
                failures = 0;
                if report.claimed > 0 {
                    tracing::debug!(?report, "shipped usage");
                }
                if report.unknown_meter > 0 {
                    // The platform lost or never kept the declaration. Declare
                    // again; those rows stay queued with capped backoff and are
                    // retried when due, however many passes that takes, so a
                    // platform that keeps refusing grows the queue but loses
                    // no billed usage. Logged at error, every pass, so a
                    // persistent refusal is visible (no row text).
                    tracing::error!(
                        rows = report.unknown_meter,
                        "the platform refused usage events as unknown_meter; declaring the usage \
                         meters again. The events stay queued and are retried"
                    );
                    if !declare_meters(&db, &platform, &cfg, &mut shutdown, &status).await {
                        tracing::warn!("usage shipper stopped while re-declaring the meters");
                        return;
                    }
                    continue;
                }
                // A full batch means there is probably more waiting.
                if report.claimed >= cfg.batch {
                    Duration::ZERO
                } else {
                    cfg.poll
                }
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                let wait = cfg
                    .poll
                    .saturating_mul(1u32 << failures.min(6))
                    .min(Duration::from_secs(60));
                match &e {
                    ShipError::Platform(savvagent_resource::Error::Unauthorized) => {
                        tracing::error!(
                            "the platform rejected this resource server's credential; usage is \
                         accumulating in the outbox. Check OF_INTROSPECTION_SECRET"
                        )
                    }
                    other => {
                        tracing::warn!(error = %other, retry_in = ?wait, "usage shipping failed")
                    }
                }
                wait
            }
        };

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {
                if let Err(e) = ship_batch(&db, &platform, &cfg).await {
                    tracing::warn!(error = %e, "final usage flush failed; rows stay in the outbox");
                }
                tracing::info!("usage shipper stopped");
                return;
            }
        }
    }
}
