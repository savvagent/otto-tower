//! Billing errors.

pub type Result<T> = std::result::Result<T, BillingError>;

#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    /// The platform says this org's billable calls must be refused now: its
    /// budget's hard stop has tripped, or it is not in good standing to be
    /// billed. Only a completed job is billable, so only `complete_job` sees
    /// this.
    ///
    /// Written for an LLM caller that has to decide what to do next, so it says
    /// what happened, what still works, and where a human fixes it. An agent
    /// that reads only "blocked" will retry; one that reads this will stop and
    /// say something useful to the person it is working for. `reason` is the
    /// platform's own word for it, kept raw; [`BillingError::code`] maps it to
    /// a stable code.
    #[error(
        "{what}, so this job was not marked completed. The job and your claim on it are \
         unchanged: keep the claim alive with renew_claim while an owner fixes billing and \
         then call complete_job again, or hand the job back to the queue with repend_job. \
         Every other Savvagent Tower tool is free and keeps working: you can still queue, \
         claim, and inspect jobs. To complete jobs again, an owner of this organization \
         needs to {fix} in the otto console at {fix_url}. Changes made in the otto console \
         can take up to a minute to reach Savvagent Tower.",
        what = blocked_what(reason.as_deref()),
        fix = blocked_fix(reason.as_deref()),
    )]
    Blocked {
        reason: Option<String>,
        fix_url: String,
    },

    /// The platform could not report the org's standing (`whoami`, `usage`).
    /// Never raised by the status check itself, which fails open — see
    /// [`crate::Meter`].
    #[error("the Savvagent platform could not be reached to read this organization's usage ({0})")]
    Platform(#[from] savvagent_resource::Error),

    #[error(transparent)]
    Tenant(#[from] savvagent_tenant::Error),

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl BillingError {
    /// Stable machine-readable code, for the same reason `tower_core::Error` has
    /// one: agents branch on this, humans read the message.
    pub fn code(&self) -> &'static str {
        match self {
            BillingError::Blocked { reason, .. } => blocked_code(reason.as_deref()),
            BillingError::Platform(_) => "platform_unavailable",
            BillingError::Tenant(e) => e.code(),
            BillingError::Db(_) => "internal_error",
        }
    }

    /// Retrying a blocked org does not help until an owner raises the budget,
    /// fixes the payment method, or the month rolls over, and an agent that
    /// backs off and retries is an agent burning its own time.
    pub fn retriable(&self) -> bool {
        match self {
            BillingError::Blocked { .. } => false,
            BillingError::Platform(e) => e.is_retriable(),
            BillingError::Tenant(e) => matches!(e, savvagent_tenant::Error::Db(_)),
            BillingError::Db(_) => true,
        }
    }
}

/// The stable code for a block. Only the platform's two known reasons are
/// passed through; anything else — no reason at all (a tripped budget from a
/// platform that predates `blocked_reason`) or one this build does not know —
/// is `budget_exceeded`, so an agent always gets a code it can branch on
/// rather than a string nobody documented.
pub(crate) fn blocked_code(reason: Option<&str>) -> &'static str {
    match reason {
        Some(savvagent_resource::PAYMENT_REQUIRED) => savvagent_resource::PAYMENT_REQUIRED,
        _ => savvagent_resource::BUDGET_EXCEEDED,
    }
}

fn blocked_what(reason: Option<&str>) -> &'static str {
    match blocked_code(reason) {
        savvagent_resource::PAYMENT_REQUIRED => {
            "this organization has no payment method in good standing and has used \
             past what its plan includes this month"
        }
        _ => "this organization has reached its Savvagent Tower budget for this month",
    }
}

fn blocked_fix(reason: Option<&str>) -> &'static str {
    match blocked_code(reason) {
        savvagent_resource::PAYMENT_REQUIRED => "add or fix a card",
        _ => "raise the budget",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(reason: Option<&str>) -> BillingError {
        BillingError::Blocked {
            reason: reason.map(str::to_string),
            fix_url: "https://otto.example".into(),
        }
    }

    /// Agents branch on the code, so it is one of exactly two values whatever
    /// the platform sends.
    #[test]
    fn a_block_is_coded_by_the_platforms_reason() {
        assert_eq!(blocked(Some("budget_exceeded")).code(), "budget_exceeded");
        assert_eq!(blocked(Some("payment_required")).code(), "payment_required");
        assert_eq!(blocked(None).code(), "budget_exceeded");
        assert_eq!(
            blocked(Some("a_reason_from_the_future")).code(),
            "budget_exceeded"
        );
    }

    /// The message promises a console change applies within a minute. That is
    /// only true while the platform client caches the status for no longer.
    #[test]
    fn the_message_promises_no_less_than_the_status_cache_delivers() {
        assert!(savvagent_resource::USAGE_STATUS_TTL <= std::time::Duration::from_secs(60));
    }

    #[test]
    fn a_block_is_never_retriable() {
        assert!(!blocked(Some("budget_exceeded")).retriable());
        assert!(!blocked(Some("payment_required")).retriable());
        assert!(!blocked(None).retriable());
    }

    /// The message says what happened, what still works, who fixes it, and
    /// where.
    #[test]
    fn a_block_says_what_to_do_about_it() {
        let budget = blocked(Some("budget_exceeded")).to_string();
        assert!(budget.contains("budget"), "{budget}");
        assert!(budget.contains("raise the budget"), "{budget}");
        assert!(budget.contains("free"), "{budget}");
        assert!(budget.contains("owner"), "{budget}");
        assert!(budget.contains("https://otto.example"), "{budget}");

        assert!(budget.contains("renew_claim"), "{budget}");
        assert!(budget.contains("repend_job"), "{budget}");
        assert!(budget.contains("up to a minute"), "{budget}");

        let payment = blocked(Some("payment_required")).to_string();
        assert!(payment.contains("add or fix a card"), "{payment}");
        assert!(payment.contains("https://otto.example"), "{payment}");
    }
}
