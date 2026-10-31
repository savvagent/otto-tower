//! `tower-billing` — metering, buckets, and the free/billable split.
//!
//! The billable unit is the MCP tool call, but **not every call is billable**,
//! and that classification ([`classify`]) is load-bearing rather than a detail:
//! `watch` is a continuous long poll, and billing it flat would charge an idle
//! agent tens of thousands of operations a month for waiting quietly, which is
//! precisely the behaviour the server asks of it.
//!
//! The split of responsibility with the platform is deliberate. The platform
//! owns plans, standing, and the monthly totals; this crate owns policy — which
//! tools cost anything ([`classify`]), when a call is refused ([`meter`]), what
//! a completed job writes ([`completion`]) — and
//! the *record* of every call, kept in the factory's own database as a
//! transactional outbox ([`outbox`]) that a background task ships to the
//! platform. That leaves the interesting decisions unit-testable with no
//! database at all, which is why the threshold arithmetic in [`meter`] and the
//! price list in [`classify`] have tests that run in microseconds.
//!
//! The one thing that must not be refactored apart: the outbox row is written
//! **in the same transaction as the tool's own work**. A failed call is not
//! billed and a successful one is never lost, and both of those follow from that
//! single fact rather than from any retry logic.

pub mod classify;
pub mod completion;
pub mod error;
pub mod meter;
pub mod outbox;

pub use classify::{classify, Class};
pub use completion::{
    record_job_completed, CompletionSource, TOOL_COMPLETE_JOB, TOOL_INBOUND_TICKET_CLOSE,
};
pub use error::{BillingError, Result};
pub use meter::{default_fix_url, BudgetReport, Charge, Meter, MeterStatus, Status, Warmed};
pub use outbox::{Metered, METER_DECLARATIONS, METER_JOBS_COMPLETED, RETIRED_METERS};
