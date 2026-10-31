//! Savvagent Tower's own migrations, in `crates/of-core/migrations`.
//!
//! `0001_baseline.sql` is a squashed, domain-only baseline that replaced the
//! original `0001`-`0034` history at the platform cutover
//! (savvagent/otto-factory#192); migrations are append-only again from there.
//!
//! **Never call `otto_tenant::Db::migrate` on this database.** It applies
//! otto-platform's own migration history, whose version numbers collide with
//! this one's and whose checksums differ, so sqlx would refuse to start at best
//! and interleave two histories in one `_sqlx_migrations` table at worst. The
//! platform's schema lives in the platform's database, not this one.
//!
//! `tests/guards.rs` fails the build if a workspace source file calls the
//! platform's `migrate`.

use otto_tenant::Db;

/// The migrator for this crate's `migrations/` directory, for tests that hand
/// it to `#[sqlx::test(migrator = "of_core::MIGRATOR")]`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Apply every Savvagent Tower migration. Safe to run from several replicas at
/// once: sqlx takes a Postgres advisory lock for the duration, so the losers
/// wait rather than racing each other through the same DDL.
pub async fn migrate(db: &Db) -> crate::Result<()> {
    MIGRATOR
        .run(db.pool())
        .await
        .map_err(|e| sqlx::Error::Migrate(Box::new(e)))?;
    Ok(())
}
