//! otto-tower server binary. Assembles every HTTP surface on one port.

use anyhow::{Context, Result};
use of_billing::outbox::ShipperConfig;
use of_core::watch::Watcher;
use of_server::{platform_client, router_with_shipper, Config, LogFormat};
use otto_tenant::Db;
use tokio::net::TcpListener;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Loads `.env` if there is one. `dotenvy` never overwrites a variable that
    // is already set, so a file that accidentally ships inside an image cannot
    // override the deployment's real configuration.
    let dotenv = dotenvy::dotenv();

    let config = Config::from_env().context(
        "configuration is incomplete. Copy .env.example to .env for local runs, \
         or set the variables named above in the deployment",
    )?;

    init_tracing(config.log_format)?;
    config.warn_retired();
    config.warn_past_sunset(chrono::Utc::now());
    match dotenv {
        Ok(path) => tracing::debug!(path = %path.display(), "loaded .env"),
        Err(_) => tracing::debug!("no .env file; using the process environment"),
    }

    // Validate the key before anything else can be built with it. `Cipher`
    // construction is infallible from here on, and a bad key is a startup
    // error naming the variable rather than a panic the first time something
    // needs to encrypt a secret hours later.
    otto_tenant::crypto::Cipher::from_base64_key(&config.encryption_key)
        .context("OF_ENCRYPTION_KEY is not a valid 32-byte base64 key")?;

    let db = Db::connect(&config.database_url)
        .await
        .context("could not connect to DATABASE_URL")?;

    if config.run_migrations {
        // sqlx holds a Postgres advisory lock for the whole run, so several
        // replicas starting together is safe: the losers block until the winner
        // is done rather than racing each other through the same DDL.
        tracing::info!("applying migrations");
        of_core::migrate(&db).await.context("migrations failed")?;
    } else {
        tracing::warn!("OF_RUN_MIGRATIONS is off; assuming the schema is already current");
    }

    // Before the shipper declares the meters and starts draining: move rows an
    // older image enqueued under a meter key that has since been renamed (a
    // rollback past `0006_meter_otto_tower.sql`, then this roll forward) to the
    // key they will be billed under. The shipper maps such keys on every batch
    // as well (rows an older replica writes after this point), so this keeps
    // the table honest rather than billing right. Idempotent, and a no-op on
    // every ordinary start. Run whether or not this process applied the
    // migrations, since the rows are not a schema matter.
    let moved = of_billing::outbox::rewrite_retired_meters(&db)
        .await
        .context("could not move queued usage off a retired meter key")?;
    if moved > 0 {
        tracing::warn!(
            rows = moved,
            "moved queued usage rows from a retired meter key to its successor \
             (an older image ran since the rename)"
        );
    }

    // Prove tenant isolation before binding a port, never after. Row-level
    // security is the one guard the *environment* can switch off — the same
    // migrations isolate perfectly under one database role and not at all under
    // another, and no amount of reading this repository tells you which one a
    // deployment connects as. Discovering that from a customer is not a
    // recoverable failure, so it is a startup error naming the remediation.
    let isolation = db
        .verify_tenant_isolation()
        .await
        .context("refusing to serve: tenant isolation is not enforced by this database")?;
    tracing::info!("{}", isolation.summary());

    // The platform is the authorization server and the identity directory, and
    // every request this service authenticates goes through it. Reachable or
    // not, the process still starts (a platform outage must not stop a restart
    // that would otherwise serve cached tokens and reads), but a *wrong
    // credential* is a deployment fault worth shouting about now rather than at
    // the first request.
    let platform = platform_client(&config)?;
    check_platform(&platform, &config).await;

    let watcher = Watcher::spawn(db.pool().clone())
        .await
        .context("could not start the change listener")?;

    if !config.static_dir.is_dir() {
        // Loud on purpose. A missing bundle serves a
        // 404 on every console page while the API works perfectly, which looks
        // like a routing bug for as long as it takes someone to notice that
        // `npm run build` was never run.
        tracing::warn!(
            dir = %config.static_dir.display(),
            "no console bundle at OF_STATIC_DIR; the API will work and every console page will 404. \
             Run `npm run build` in web/, or set OF_STATIC_DIR"
        );
    }

    // Delivers the usage outbox to the platform. See `of_billing::outbox`: rows
    // are written in the tools' own transactions and shipped here, so the
    // platform being down delays billing and never loses it.
    let (stop_shipper, shipper_shutdown) = tokio::sync::watch::channel(false);
    let shipper_status = of_billing::outbox::ShipperStatus::default();
    let shipper = tokio::spawn(of_billing::outbox::run_with_status(
        db.clone(),
        platform.clone(),
        ShipperConfig::default(),
        shipper_shutdown,
        shipper_status.clone(),
    ));

    // Hourly housekeeping: forget old platform-event and tracker-delivery dedupe
    // markers (30 days each) and expired removed-member tombstones.
    let sweeper = tokio::spawn(sweep_loop(db.clone(), stop_shipper.subscribe()));

    let app = router_with_shipper(db, watcher.clone(), platform, &config, shipper_status)?;

    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("could not bind {}", config.bind))?;

    tracing::info!(
        bind = %config.bind,
        public_url = %config.public_url,
        resource_uri = %config.resource_uri,
        platform_url = %config.platform_url,
        "of-server listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    // The shipper makes a last attempt on the way out so a clean shutdown does
    // not strand rows that were one request from delivered. Anything it cannot
    // deliver stays in the outbox for the next process.
    tracing::info!("flushing usage");
    stop_shipper.send(true).ok();
    if let Err(e) = shipper.await {
        tracing::warn!(error = %e, "the usage shipper did not stop cleanly");
    }
    sweeper.abort();

    // After the server, deliberately. `Watcher::spawn` takes a connection out of
    // the pool for `LISTEN` and detaches it, so dropping the pool does not
    // reclaim it; `shutdown` stops the task and waits for the connection to go.
    // Without this the process holds a Postgres session open past the point it
    // stopped serving anything.
    tracing::info!("draining");
    watcher.shutdown().await;
    tracing::info!("stopped");

    Ok(())
}

/// How long platform-event dedupe markers are kept.
const PLATFORM_EVENT_RETENTION_DAYS: i32 = 30;

/// How long tracker webhook deliveries are remembered as applied. A signed
/// GitHub delivery replayed after this is applied again, guarded only by the
/// job's own revision and `ticket_changed_at` checks; GitHub itself redelivers
/// only within 3 days.
const TRACKER_DELIVERY_RETENTION_DAYS: i32 = 30;

/// Run [`of_core::platform_events::sweep`] and
/// [`of_core::tracker_deliveries::sweep`] hourly until shutdown. Each runs
/// whether or not the other failed.
async fn sweep_loop(db: Db, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        match of_core::platform_events::sweep(&db, PLATFORM_EVENT_RETENTION_DAYS).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept old platform-event records"),
            Err(e) => tracing::warn!(error = %e, "platform-event sweep failed"),
        }
        match of_core::tracker_deliveries::sweep(&db, TRACKER_DELIVERY_RETENTION_DAYS).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept old tracker-delivery records"),
            Err(e) => tracing::warn!(error = %e, "tracker-delivery sweep failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// Ask the platform something harmless, to learn whether our credential works.
///
/// Non-fatal on purpose. The usage-status lookup for an org that cannot exist
/// answers `404` to a good credential and `401` to a bad one, and touches no
/// data. A transport failure only means the platform is not up yet.
async fn check_platform(platform: &otto_resource::PlatformClient, config: &Config) {
    match platform.usage_status(uuid::Uuid::nil()).await {
        Ok(_) | Err(otto_resource::Error::NotFound) => {
            tracing::info!(platform = %config.platform_url, "the otto platform accepted this service's credential")
        }
        Err(otto_resource::Error::Unauthorized) => tracing::error!(
            platform = %config.platform_url,
            resource_uri = %config.resource_uri,
            "the otto platform REJECTED this service's credential: no token can be \
             validated and no usage can be shipped. Check OF_INTROSPECTION_SECRET and that \
             OF_RESOURCE_URI is the resource_uri this service was registered with"
        ),
        Err(e) => tracing::warn!(
            platform = %config.platform_url,
            error = %e,
            "could not reach the otto platform at startup; continuing"
        ),
    }
}

fn init_tracing(format: LogFormat) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);

    match format {
        LogFormat::Json => registry
            .with(tracing_subscriber::fmt::layer().json())
            .init(),
        LogFormat::Text => registry.with(tracing_subscriber::fmt::layer()).init(),
    }

    Ok(())
}

/// Resolves on the first `SIGTERM` or `SIGINT`.
///
/// `SIGTERM` is the one that matters — it is what a container runtime sends
/// before it waits its grace period and then sends `SIGKILL`. A server that
/// only handles `SIGINT` looks fine in a terminal and is hard-killed on every
/// single deploy, dropping whatever was in flight.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install the SIGINT handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT; shutting down"),
        _ = terminate => tracing::info!("SIGTERM; shutting down"),
    }
}
