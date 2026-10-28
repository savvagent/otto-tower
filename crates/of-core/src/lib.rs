//! `of-core` — the otto-tower domain: repos, jobs, leases, messages, trackers.
//!
//! Identity, tenancy, auth, and plan usage are not here. They live in the otto
//! platform, in another database, reached over HTTP (`otto-resource`); this
//! crate builds the factory's domain on top of `otto-tenant`'s pinned
//! transaction and holds no foreign key to anything the platform owns.
//!
//! This crate owns every factory SQL statement and knows nothing about HTTP,
//! MCP, or authentication. Two rules hold throughout, and both exist
//! to make cross-tenant leakage structurally impossible rather than merely
//! unlikely:
//!
//! 1. **Every tenant-scoped operation takes an [`OrgId`]** — usually by being a
//!    method on [`Tx`], which cannot be constructed without one. There is no
//!    function in this crate that reads a job without naming an org.
//! 2. **Every tenant transaction runs pinned.** [`Db::begin`] issues
//!    `SET LOCAL ROLE otto_app` and `SET LOCAL app.org_id`, so Postgres
//!    row-level security applies even when the connecting user owns the tables.
//!    A query that forgets its `org_id` predicate returns nothing instead of
//!    leaking. See `otto_tenant::isolation` for the deployment shapes that
//!    qualify and the one that does not.
//!
//! Because [`Tx`] and [`Db`] are defined in `otto-tenant`, Rust's orphan rules
//! forbid inherent `impl Tx<'_> { ... }` blocks here. The factory's methods are
//! therefore extension traits ([`jobs::JobsExt`], [`repos::ReposExt`],
//! [`leases::LeasesExt`], [`messages::MessagesExt`]); import the trait alongside
//! `Tx` to call its methods.
//!
//! [`OrgId`]: otto_tenant::ids::OrgId
//! [`Tx`]: otto_tenant::Tx
//! [`Db`]: otto_tenant::Db

pub mod agent_label;
pub mod audit;
pub mod console_sessions;
pub mod error;
pub mod idempotency;
pub mod ids;
pub mod jobs;
pub mod leases;
pub mod messages;
pub mod migrate;
pub mod platform_events;
pub mod repo_slug;
pub mod repos;
pub mod scopes;
pub mod teams;
pub mod ticket_ref;
pub mod trackers;
pub mod watch;

pub use error::{Error, Result};
pub use ids::{JobId, RepoId};
pub use migrate::{migrate, MIGRATOR};
