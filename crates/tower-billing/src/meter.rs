//! Recording tool calls, and refusing the one billable call when the platform
//! says to.
//!
//! ## Two halves, two systems
//!
//! The org's *standing* (its budget, its payment standing, this month's meter
//! totals) lives in the platform, which is the system of record for billing. A
//! call's *record* lives here, in the factory's own database, as a row in
//! `usage_outbox`:
//!
//! - **Recording** is local and atomic. [`Meter::charge`] and
//!   [`Meter::charge_completion`] insert the outbox row in the tool's own
//!   transaction, so a failed call rolls its record back with everything else
//!   (never billed) and a successful one is recorded even if the platform is
//!   down at that instant (never lost). A background shipper
//!   ([`crate::outbox`]) delivers the rows later, exactly once from the
//!   platform's point of view because it dedupes on the row's `event_id`.
//! - **Enforcing** reads the platform's answer to "must this service refuse
//!   billable calls for this org now" ([`UsageStatus::billable_blocked`]),
//!   cached by `PlatformClient::usage_status` for `savvagent_resource::USAGE_STATUS_TTL`
//!   (a minute). The platform decides; this crate only obeys, and the
//!   platform's spend lags by whatever is still in the outbox plus one cache
//!   window. That bound is the price of the split and is deliberately
//!   accepted.
//!
//! **A change in the console takes up to that minute to apply, in both
//! directions.** An owner who raises the budget or adds a card can still see
//! completions refused until the cached "blocked" answer expires, and an org
//! that has just been blocked can still complete jobs until the cached "not
//! blocked" one does. The refusal says so, so an agent waits a minute rather
//! than concluding the fix did not work.
//!
//! ## Why charging happens *before* the work
//!
//! A charge is made immediately after the tool opens its transaction, before
//! it does anything. That looks wrong for the rule "a failed call is not
//! billed" — and it is exactly what makes the rule true: the outbox row is in
//! the tool's own transaction, so a tool that fails rolls the meter back with
//! it. Charging afterwards would need a second transaction, which can fail on
//! its own, be retried, or be forgotten. Doing it first also puts the refusal
//! where it costs nothing.
//!
//! ## What enforcement means
//!
//! Every tool call is free except a completed job ([`crate::classify`]), so
//! only [`Meter::charge_completion`] can refuse, and only when the platform's
//! status for the org says `billable_blocked()`: its budget's hard stop has
//! tripped (`budget_exceeded`) or it is not in good standing to be billed
//! (`payment_required`). The refusal is [`BillingError::Blocked`]. Free calls
//! are never refused and never look anything up, so a blocked org can still
//! queue, claim, read, and wait — and find out why it is blocked.
//!
//! **If the platform cannot answer, the call is allowed** (and logged). The
//! alternative is refusing paying customers' work because a lookup failed;
//! the call is still recorded in the outbox, so nothing is under-billed.
//! Authentication already requires the platform, so this only matters in the
//! window where a cached token outlives an outage. The decision is remembered
//! per org for [`FAIL_OPEN_FOR`], so an outage costs one bounded lookup per org
//! per few seconds, not one per call.
//!
//! **The lookup happens before the transaction opens.** `complete_job` calls
//! [`Meter::warm`] before it opens its transaction and hands the answer, a
//! [`Warmed`], to [`Meter::charge_completion`], which never makes a network
//! call itself. A slow platform therefore never holds a pooled database
//! connection, and the answer the charge judges cannot expire between the two.
//! The tracker webhook does the same before its transaction, to decide whether
//! a ticket close is billed (a blocked org's close is applied but recorded
//! unbilled; see [`crate::completion`]). No other path calls `warm`: the free
//! tools have nothing to ask.
//!
//! [`Warmed`] can only be made by [`Meter::warm`], so a charge cannot be handed
//! a made-up "no answer" by accident: the only way to get an allowing status
//! without asking is for the platform to fail to answer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use savvagent_resource::{PlatformClient, UsageStatus};
use savvagent_tenant::ids::{OrgId, UserId};
use savvagent_tenant::Tx;
use serde::Serialize;

use crate::classify;
use crate::completion::{self, CompletionSource};
use crate::error::{BillingError, Result};
use crate::outbox;

/// How long the status lookup waits on the platform before allowing the call.
///
/// The lookup runs before the transaction opens, so no connection is held
/// while it waits, but the agent is: the client's own 10 s timeout is far too
/// long to add to a `complete_job`. A cache hit (the normal case) costs
/// nothing and a miss gets this long, after which the call goes ahead and is
/// recorded like any other — the same outcome as the platform being down.
const STATUS_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a failed status lookup is remembered, per org, as "allow".
const FAIL_OPEN_FOR: std::time::Duration = std::time::Duration::from_secs(5);

/// Fraction of the bucket at which a caller starts being warned.
///
/// Eighty percent is early enough that a team has time to do something about it
/// before work starts being refused, and late enough that it is not noise.
pub const WARN_AT: f64 = 0.8;

/// Where an owner fixes a budget or payment block when the deployment names
/// none (`OF_UPGRADE_URL` unset): the platform console's home, which lists the
/// user's orgs (billing lives under each org there).
///
/// ```
/// assert_eq!(tower_billing::default_fix_url("https://otto.example/"), "https://otto.example");
/// ```
pub fn default_fix_url(platform_url: &str) -> String {
    platform_url.trim_end_matches('/').to_string()
}

/// The org's standing as [`Meter::warm`] read it, for
/// [`Meter::charge_completion`] or a tracker close to judge.
///
/// Only [`Meter::warm`] makes one. A lookup that failed is a `Warmed` that
/// allows (fail open); there is no other way to get one that allows without
/// asking.
#[derive(Debug, Clone)]
pub struct Warmed(Option<UsageStatus>);

impl Warmed {
    /// Whether billable work must be refused (or, for a tracker close, left
    /// unbilled) now. `false` when the platform could not answer.
    pub fn billable_blocked(&self) -> bool {
        self.0.as_ref().is_some_and(UsageStatus::billable_blocked)
    }

    /// The platform's reason for a block, when it gave one.
    pub fn blocked_reason(&self) -> Option<&str> {
        self.0
            .as_ref()
            .filter(|s| s.billable_blocked())
            .and_then(|s| s.blocked_reason.as_deref())
    }

    /// Whether the platform answered at all.
    pub fn answered(&self) -> bool {
        self.0.is_some()
    }
}

/// Configuration for the meter.
#[derive(Clone)]
pub struct Meter {
    platform: Arc<PlatformClient>,
    /// Orgs whose last status lookup failed, and until when to skip looking.
    fail_open: Arc<Mutex<HashMap<OrgId, Instant>>>,
    /// Where an owner fixes a block: `OF_UPGRADE_URL`, else the platform
    /// console's home, which lists the user's orgs. Named in the refusal,
    /// because an error that says "raise the budget" without saying where is a
    /// dead end for an agent and an annoyance for a human. Not a per-org link:
    /// the charge knows only the org id, and resolving its slug would put a
    /// platform call inside the transaction.
    pub fix_url: String,
}

impl std::fmt::Debug for Meter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meter")
            .field("fix_url", &self.fix_url)
            .finish_non_exhaustive()
    }
}

impl Meter {
    pub fn new(platform: Arc<PlatformClient>, fix_url: impl Into<String>) -> Self {
        Self {
            platform,
            fail_open: Arc::default(),
            fix_url: fix_url.into(),
        }
    }

    /// Look the org's standing up now, before the transaction that completes
    /// a job opens, and return it for [`Self::charge_completion`] (or a
    /// tracker close) to judge. When the platform could not answer in time the
    /// result allows the completion (fail open). Never fails: see the module
    /// docs.
    pub async fn warm(&self, org: OrgId) -> Warmed {
        Warmed(self.status_for(org).await)
    }

    /// Record one free tool call. Never refuses and never asks the platform:
    /// every tool is free ([`crate::classify`]), and a completed job is
    /// charged through [`Self::charge_completion`] instead.
    ///
    /// The call is recorded in the outbox inside `tx`, so it commits or rolls
    /// back with the tool's own work.
    pub async fn charge(&self, tx: &mut Tx<'_>, user: UserId, tool: &str) -> Result<Charge> {
        let class = classify::classify(tool);
        outbox::enqueue(tx, Some(user), tool, class.is_billable(), None).await?;
        Ok(Charge {
            billable: class.is_billable(),
        })
    }

    /// Charge the job `complete_job` is about to complete, or refuse it.
    ///
    /// `warmed` is what [`Self::warm`] returned for this org before `tx` was
    /// opened; this never asks the platform itself, so nothing here waits on
    /// the network while a connection is held. Refused with
    /// [`BillingError::Blocked`] when `warmed` is `billable_blocked()`, and
    /// then nothing is written. Otherwise the completion is recorded, billed,
    /// through [`crate::completion::record_job_completed`], inside `tx` (a
    /// lookup that failed allows it: fail open, see the module docs).
    pub async fn charge_completion(
        &self,
        tx: &mut Tx<'_>,
        user: UserId,
        warmed: &Warmed,
    ) -> Result<Charge> {
        if warmed.billable_blocked() {
            return Err(BillingError::Blocked {
                reason: warmed.blocked_reason().map(str::to_string),
                fix_url: self.fix_url.clone(),
            });
        }
        completion::record_job_completed(tx, Some(user), CompletionSource::CompleteJob, true)
            .await?;
        Ok(Charge { billable: true })
    }

    /// Record an idempotent replay of `tool` — always as `Free`, regardless
    /// of `tool`'s own classification, and with no status lookup.
    ///
    /// "Both classes are recorded regardless" still has to hold for a replay: it
    /// is a real call that reached the server and was served, just not new work.
    /// Charging it again would double-bill the caller for one logical
    /// job/message; not recording it at all would leave a gap in the history a
    /// future repricing decision needs. Never refused — a replay creates
    /// nothing, so refusing it would refuse a call that is free to serve.
    pub async fn record_replay(&self, tx: &mut Tx<'_>, user: UserId, tool: &str) -> Result<()> {
        outbox::enqueue(tx, Some(user), tool, false, None).await?;
        Ok(())
    }

    /// The platform's usage status for `org`, or `None` if it could not be read
    /// in time (the caller then allows the call). Bounded by
    /// [`STATUS_LOOKUP_TIMEOUT`], and a failure is remembered for
    /// [`FAIL_OPEN_FOR`] so an outage does not add that wait to every call.
    async fn status_for(&self, org: OrgId) -> Option<UsageStatus> {
        if let Some(until) = self
            .fail_open
            .lock()
            .ok()
            .and_then(|m| m.get(&org).copied())
        {
            if until > Instant::now() {
                return None;
            }
        }
        let failure = match tokio::time::timeout(
            STATUS_LOOKUP_TIMEOUT,
            self.platform.usage_status(org.as_uuid()),
        )
        .await
        {
            Ok(Ok(status)) => {
                if let Ok(mut m) = self.fail_open.lock() {
                    m.remove(&org);
                }
                return Some(status);
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => "timed out".to_string(),
        };
        tracing::warn!(
            org = %org,
            reason = %failure,
            "could not read usage from the platform in time; allowing billable calls for {FAIL_OPEN_FOR:?} (they are still recorded)"
        );
        if let Ok(mut m) = self.fail_open.lock() {
            m.insert(org, Instant::now() + FAIL_OPEN_FOR);
        }
        None
    }

    /// Report an org's standing without charging for anything.
    ///
    /// Used by the `usage` and `whoami` tools, which are themselves free — a
    /// caller must never have to spend an operation to find out how many it has
    /// left. Unlike the status check this surfaces a platform failure, since an
    /// answer was asked for and a made-up one would be worse than an error.
    pub async fn report(&self, org: OrgId) -> Result<Status> {
        let usage = self.platform.usage_status(org.as_uuid()).await?;
        Ok(Status::new(usage))
    }
}

/// `free` -> `Free`.
fn plan_name(plan: &str) -> String {
    let mut chars = plan.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Where an org stands: this month's metered usage, its budget, and any block.
///
/// `meters`, `budget` and `blocked_reason` are the current picture. The older
/// bucket fields (`included_ops` through `enforced`) are kept so existing
/// readers keep working.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub plan: String,
    /// This month's quantity for each meter this server bills on, such as
    /// `otto-tower/jobs_completed`. A meter with no usage yet shows zero.
    pub meters: Vec<MeterStatus>,
    /// The monthly budget the org set for this service and where its spend
    /// stands, or null when it has set none.
    pub budget: Option<BudgetReport>,
    /// Why billable calls (`complete_job`) are being refused right now, such
    /// as `budget_exceeded` or `payment_required`, or null when they are not.
    pub blocked_reason: Option<String>,
    /// Deprecated: the legacy bucket. Billable operations included this month,
    /// after any negotiated override.
    pub included_ops: i64,
    /// Deprecated: see `meters`.
    pub billable_used: i64,
    /// Deprecated. Never negative: an org past its bucket has none left, not a
    /// debt.
    pub remaining: i64,
    /// Deprecated. Every recorded call this month, billable or not.
    pub total_calls: i64,
    pub period_start: chrono::NaiveDate,
    /// Deprecated. True past [`WARN_AT`] of the bucket.
    pub warning: bool,
    /// Deprecated. Whether exceeding the legacy bucket stops billable work
    /// rather than metering overage.
    pub hard_stop: bool,
    /// Deprecated. Now always `true`: the platform's blocked reason is always
    /// enforced, and is no longer switchable.
    pub enforced: bool,
}

/// One meter's usage this month.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MeterStatus {
    /// The meter's key, `<service>/<meter>`.
    pub meter: String,
    pub display_name: String,
    pub unit: String,
    /// The sum of the quantities billed on this meter this month.
    pub quantity: u64,
}

/// The org's monthly budget for this service, in US cents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetReport {
    pub monthly_budget_cents: i64,
    /// Spend so far this month, or null when the platform could not compute it.
    pub spend_cents: Option<i64>,
    /// Whole percent of the budget spent (can exceed 100), or null when spend
    /// is unknown.
    pub percent_used: Option<i64>,
    /// The alert thresholds the platform reports as reached this month.
    pub thresholds_reached: Vec<u8>,
    /// Whether the org opted in to refusing billable calls once spend reaches
    /// the budget.
    pub hard_stop: bool,
    /// Whether billable calls are being refused now because of the budget.
    pub blocked: bool,
    /// Why `blocked` is true (`budget_exceeded`), or why spend is unknown
    /// (`spend_unavailable`, never blocked); null otherwise.
    pub reason: Option<String>,
}

impl Status {
    fn new(usage: UsageStatus) -> Self {
        // Report the code a refusal would carry: a block the platform gave no
        // reason for is still `budget_exceeded`, never null.
        let blocked_reason = if usage.billable_blocked() {
            Some(crate::error::blocked_code(usage.blocked_reason.as_deref()).to_string())
        } else {
            usage.blocked_reason.clone()
        };
        Self {
            plan: plan_name(&usage.plan),
            meters: usage
                .meters
                .into_iter()
                .map(|m| MeterStatus {
                    meter: m.meter,
                    display_name: m.display_name,
                    unit: m.unit,
                    quantity: m.quantity,
                })
                .collect(),
            budget: usage.budget.map(|b| BudgetReport {
                monthly_budget_cents: b.monthly_budget_cents,
                spend_cents: b.spend_cents,
                percent_used: b.percent_used,
                thresholds_reached: b.thresholds_reached,
                hard_stop: b.hard_stop,
                blocked: b.blocked,
                reason: b.reason,
            }),
            blocked_reason,
            included_ops: usage.included_ops,
            billable_used: usage.billable_count,
            remaining: remaining(usage.billable_count, usage.included_ops),
            total_calls: usage.total_count,
            period_start: usage.period_start,
            warning: warning(usage.billable_count, usage.included_ops),
            hard_stop: usage.hard_stop,
            enforced: true,
        }
    }
}

/// The outcome of charging one call.
#[derive(Debug, Clone, PartialEq)]
pub struct Charge {
    pub billable: bool,
}

/// Operations left in the bucket, floored at zero.
fn remaining(used: i64, included: i64) -> i64 {
    included.saturating_sub(used).max(0)
}

/// Whether an org has crossed the warning threshold.
///
/// A bucket of zero counts as exhausted rather than as a division by zero — an
/// org with no included operations is over its limit from the first call.
fn warning(used: i64, included: i64) -> bool {
    if included <= 0 {
        return true;
    }
    used as f64 >= included as f64 * WARN_AT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_never_goes_negative() {
        assert_eq!(remaining(0, 500), 500);
        assert_eq!(remaining(499, 500), 1);
        assert_eq!(remaining(500, 500), 0);
        assert_eq!(
            remaining(900, 500),
            0,
            "an org past its bucket has none left, not a debt"
        );
    }

    /// The boundary customers actually notice: a plan sold as 500 operations
    /// has to deliver 500, not 499.
    #[test]
    fn the_warning_starts_at_four_fifths() {
        assert!(!warning(399, 500));
        assert!(warning(400, 500));
        assert!(warning(500, 500));
    }

    #[test]
    fn an_empty_bucket_is_always_over() {
        assert!(warning(0, 0));
        assert_eq!(remaining(0, 0), 0);
    }

    #[test]
    fn plans_are_shown_capitalised() {
        assert_eq!(plan_name("free"), "Free");
        assert_eq!(plan_name("enterprise"), "Enterprise");
        assert_eq!(plan_name(""), "");
    }
}
