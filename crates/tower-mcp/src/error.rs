//! Turning domain failures into something an agent can act on.
//!
//! The reader of every string in this file is an LLM that has never seen the
//! documentation, is holding a partly-wrong model of the server's state, and
//! must decide within one turn whether to retry, change the arguments, or give
//! up and ask a human. That audience changes what a good error is:
//!
//! - **Say what was wrong, what was valid, and what to call next.** `tower-core`
//!   already writes its messages this way — `RepoUnresolved` lists the
//!   registered slugs, `LeaseHeld` names the holder and the expiry — so this
//!   module's job is to carry them through intact rather than flatten them into
//!   "bad request".
//! - **Put the branch point in machine-readable data, not in prose.** An agent
//!   matching on message text breaks the first time someone rewords one.
//!   `code` is stable; `retriable` answers the only question the agent has to
//!   resolve before its next action.
//! - **Never leak across the tenant boundary.** A message may name the caller's
//!   own repos and jobs. It may never name anything it took another org's data
//!   to know, which is why these are built from `tower-core` errors that were
//!   themselves produced inside a pinned transaction.

use rmcp::model::{ErrorCode, ErrorData};
use tower_core::Error as CoreError;

/// Convert a domain error into the MCP error envelope.
///
/// The JSON-RPC code is coarse on purpose: agents branch on `data.code`, and
/// spreading the distinction across two vocabularies would mean maintaining
/// both. Only the "your arguments are wrong" / "the server broke" split is
/// worth expressing here, because clients treat those differently at the
/// transport level.
pub fn from_core(e: &CoreError) -> ErrorData {
    envelope(Fault::tower_core(e), e.to_string(), e.code(), e.retriable())
}

/// Convert a tenant-substrate failure (`savvagent-tenant`).
pub fn from_tenant(e: &savvagent_tenant::Error) -> ErrorData {
    envelope(
        Fault::of_tenant(e),
        e.to_string(),
        e.code(),
        matches!(Fault::of_tenant(e), Fault::Database),
    )
}

/// Whose fault a failure is, which decides both the JSON-RPC code and whether
/// its text may reach the caller.
///
/// Tenancy errors arrive wrapped (`CoreError::Tenant`), and billing and platform
/// failures arrive from several crates, so a database failure can be layers
/// down. It
/// is the same failure however it got here and must be answered the same way.
#[derive(Clone, Copy)]
enum Fault {
    /// The caller's request was wrong; the message is written to be read.
    Caller,
    /// A lost race: ours, but the message is already written for the caller.
    Race,
    /// A database failure: ours, transient, and the text can carry table
    /// names or constraint names. Retriable.
    Database,
    /// The platform could not answer an identity question. Ours, transient or
    /// not depending on the failure; the text can carry the platform's response
    /// body, so it is redacted like a database error.
    Platform,
    /// Permanent server misconfiguration: a bad encryption key, undecryptable
    /// ciphertext, or tenant isolation not enforced. Ours, redacted like
    /// `Database` (the text can carry key diagnostics), but retrying the same
    /// call can never succeed.
    Misconfigured,
}

impl Fault {
    fn tower_core(e: &CoreError) -> Self {
        match e {
            CoreError::Db(_) => Fault::Database,
            CoreError::RaceLost(_) => Fault::Race,
            CoreError::Tenant(t) => Fault::of_tenant(t),
            CoreError::Platform(_) => Fault::Platform,
            _ => Fault::Caller,
        }
    }

    fn of_tenant(e: &savvagent_tenant::Error) -> Self {
        match e {
            // These two describe the request.
            savvagent_tenant::Error::Invalid(_) | savvagent_tenant::Error::OrgNotFound(_) => {
                Fault::Caller
            }
            savvagent_tenant::Error::Db(_) => Fault::Database,
            savvagent_tenant::Error::Config(_)
            | savvagent_tenant::Error::Crypto(_)
            | savvagent_tenant::Error::IsolationNotEnforced { .. } => Fault::Misconfigured,
        }
    }
}

fn envelope(fault: Fault, text: String, code: &'static str, retriable: bool) -> ErrorData {
    let rpc_code = match fault {
        // A database failure or a lost race is ours, not the caller's: the
        // request was fine; the transaction lost to a concurrent write.
        // Reporting either as an argument error would send an agent into a
        // rewrite loop over a request that was fine to begin with.
        Fault::Database | Fault::Misconfigured | Fault::Race | Fault::Platform => {
            ErrorCode::INTERNAL_ERROR
        }
        Fault::Caller => ErrorCode::INVALID_PARAMS,
    };

    // The internal message of a database error is not for the caller: it can
    // carry table names, constraint names, and fragments of SQL, none of which
    // an agent can act on and some of which describe other tenants' schema
    // surface. Log it, return a generic sentence.
    let message = match fault {
        Fault::Database | Fault::Misconfigured => {
            tracing::error!(error = %text, "server-side failure surfaced to an MCP caller");
            "the server could not complete this call; retry shortly".to_string()
        }
        // Unlike `Database`, a lost race's message is already written to be
        // read by the caller and must reach it unredacted. It is rare enough,
        // and rare-enough-to-be-suspicious if it fires a lot, that it still
        // deserves its own trace naming which call site (named in the message
        // itself) produced it.
        Fault::Race => {
            tracing::warn!(message = %text, "a lost unique-violation race surfaced to an MCP caller");
            text
        }
        Fault::Platform => {
            tracing::error!(error = %text, "platform lookup failed for an MCP caller");
            "the Savvagent platform could not be reached to check this; nothing was changed. \
             Retry shortly."
                .to_string()
        }
        Fault::Caller => text,
    };

    ErrorData::new(
        rpc_code,
        message,
        Some(serde_json::json!({
            "code": code,
            "retriable": retriable,
        })),
    )
}

/// Convert a missing-scope failure.
///
/// A missing scope is genuinely actionable: the agent must re-authorize asking
/// for more, and it cannot do that without being told which scope it lacks. (Every
/// other credential failure is answered at the HTTP layer with a `401` before a
/// handler runs, and says as little as possible on purpose.)
pub fn from_scope(e: &crate::auth::MissingScope) -> ErrorData {
    ErrorData::new(
        ErrorCode::INVALID_REQUEST,
        e.to_string(),
        Some(serde_json::json!({
            "code": "insufficient_scope",
            "retriable": false,
        })),
    )
}

/// Convert a failed platform call made on a caller's behalf (a member or team
/// lookup). The caller is refused; the platform's response text, which can carry
/// a status body, stays in the log.
pub fn from_platform(e: &savvagent_resource::Error) -> ErrorData {
    envelope(
        Fault::Platform,
        e.to_string(),
        "platform_unavailable",
        e.is_retriable(),
    )
}

/// Convert a metering failure.
///
/// A billing block is not an argument error and must not read like one: the
/// call was well-formed and the agent should stop rather than rewrite it. The
/// code is the platform's reason (`budget_exceeded` or `payment_required`),
/// and the message says what still works and the URL a human goes to, because
/// an agent that cannot say *what to do about it* just retries.
pub fn from_billing(e: &tower_billing::BillingError) -> ErrorData {
    // Not blocks: a failed lookup or a database fault keeps its own
    // identity (and redaction) rather than being reported as a billing problem.
    match e {
        tower_billing::BillingError::Platform(p) => return from_platform(p),
        tower_billing::BillingError::Tenant(t) => return from_tenant(t),
        tower_billing::BillingError::Db(_) => {
            return envelope(Fault::Database, e.to_string(), e.code(), true)
        }
        tower_billing::BillingError::Blocked { .. } => {}
    }

    ErrorData::new(
        ErrorCode::INVALID_REQUEST,
        e.to_string(),
        Some(serde_json::json!({
            "code": e.code(),
            "retriable": e.retriable(),
        })),
    )
}

/// The error a tool returns when it cannot find an authenticated caller.
///
/// This should be unreachable in production — the middleware refuses the
/// request with a `401` long before a handler runs — so reaching it means the
/// server was assembled without [`crate::auth::require_bearer`]. Say that,
/// loudly, rather than reporting it as the caller's problem: an agent debugging
/// its own arguments against a misconfigured server will never get anywhere.
pub fn unauthenticated() -> ErrorData {
    ErrorData::internal_error(
        "this request carried no authenticated principal, which means the MCP surface \
         was mounted without its resource-server middleware; this is a server \
         misconfiguration, not a problem with your call",
        Some(serde_json::json!({ "code": "unauthenticated", "retriable": false })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_core::ids::JobId;

    /// The two fields an agent branches on have to be present on every error,
    /// or the "check `retriable` before retrying" contract is a lie.
    #[test]
    fn every_error_carries_a_code_and_a_retriable_flag() {
        let errors = [
            CoreError::JobNotFound(JobId::from("job-1")),
            CoreError::Invalid("nope".into()),
            CoreError::LeaseHeld {
                resource: "main".into(),
                holder: "agent-a".into(),
                expires_at: chrono::Utc::now(),
            },
            CoreError::RaceLost("boom".into()),
        ];

        for e in &errors {
            let data = from_core(e).data.expect("no data payload");
            assert_eq!(data["code"], e.code());
            assert_eq!(data["retriable"], e.retriable());
        }
    }

    /// `tower-core` writes errors that name the alternatives. Flattening that away
    /// here would undo the whole point of writing them that way.
    #[test]
    fn the_actionable_detail_survives_conversion() {
        let e = CoreError::RepoUnresolved {
            attempted: "git@github.com:acme/nope.git".into(),
            known: "api, web".into(),
        };
        let converted = from_core(&e);
        assert!(converted.message.contains("api, web"));
        assert!(converted.message.contains("register_repo"));
    }

    /// A database error's own text can carry schema detail and is useless to an
    /// agent besides. It must not reach the wire.
    #[test]
    fn database_internals_do_not_reach_the_caller() {
        let e = CoreError::Db(sqlx::Error::RowNotFound);
        let converted = from_core(&e);
        assert_eq!(converted.code, ErrorCode::INTERNAL_ERROR);
        assert!(!converted.message.to_lowercase().contains("row"));
        assert_eq!(converted.data.unwrap()["retriable"], true);
    }

    /// A lost race is the server's problem, not the caller's — it must map
    /// to INTERNAL_ERROR at the JSON-RPC level exactly like a raw database
    /// failure does, not INVALID_PARAMS. Unlike `Db`, though, its message is
    /// already written to be read by the caller and must survive
    /// conversion unredacted — the opposite of
    /// `database_internals_do_not_reach_the_caller`, above.
    #[test]
    fn race_lost_maps_to_internal_error() {
        let e = CoreError::RaceLost("add_job lost a race".into());
        let converted = from_core(&e);
        assert_eq!(converted.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(converted.data.unwrap()["retriable"], true);
        assert!(converted.message.contains("lost a race"));
    }

    /// A refusal an agent cannot fix by trying again has to say so, and say
    /// where a human can fix it, or the agent retries until something gives up.
    #[test]
    fn a_billing_block_is_actionable_and_not_retriable() {
        for (reason, code) in [
            (Some("budget_exceeded"), "budget_exceeded"),
            (Some("payment_required"), "payment_required"),
            (None, "budget_exceeded"),
        ] {
            let e = from_billing(&tower_billing::BillingError::Blocked {
                reason: reason.map(str::to_string),
                fix_url: "https://otto.example.test".into(),
            });

            assert_eq!(e.code, ErrorCode::INVALID_REQUEST);
            let data = e.data.as_ref().unwrap();
            assert_eq!(data["code"], code);
            assert_eq!(data["retriable"], false);
            assert!(e.message.contains("https://otto.example.test"));
            assert!(
                e.message.contains("free and keeps working"),
                "the caller needs to know what it can still do: {}",
                e.message
            );
        }
    }

    /// A database failure that reaches us through billing is still a database
    /// failure, and must not be reported as a billing block.
    #[test]
    fn a_database_failure_under_billing_keeps_its_own_identity() {
        let e = from_billing(&tower_billing::BillingError::Tenant(
            savvagent_tenant::Error::Db(sqlx::Error::RowNotFound),
        ));
        assert_eq!(e.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(e.data.unwrap()["retriable"], true);
    }

    /// A platform that cannot be reached is a refusal the caller can retry, and
    /// its response body (which could be anything) never reaches the caller.
    #[test]
    fn a_platform_failure_is_retriable_and_redacted() {
        let e = from_platform(&savvagent_resource::Error::Status {
            status: 503,
            body: "internal host db-7.private exploded".into(),
        });
        assert_eq!(e.code, ErrorCode::INTERNAL_ERROR);
        let data = e.data.as_ref().unwrap();
        assert_eq!(data["code"], "platform_unavailable");
        assert_eq!(data["retriable"], true);
        assert!(!e.message.contains("db-7"));

        // A rejected credential is ours to fix, and retrying cannot help.
        let e = from_platform(&savvagent_resource::Error::Unauthorized);
        assert_eq!(e.data.unwrap()["retriable"], false);
    }

    /// A permanent misconfiguration (bad key, undecryptable ciphertext,
    /// isolation not enforced) is the server's fault and redacted, but an agent
    /// that retries it will fail identically forever, so it is not retriable.
    /// Only a real database error is.
    #[test]
    fn misconfiguration_is_redacted_and_not_retriable_but_db_errors_are() {
        for e in [
            savvagent_tenant::Error::Config("OF_ENCRYPTION_KEY is not valid base64".into()),
            savvagent_tenant::Error::Crypto("bad tag".into()),
            savvagent_tenant::Error::IsolationNotEnforced {
                problems: "role otto_app".into(),
            },
        ] {
            let converted = from_tenant(&e);
            assert_eq!(converted.code, ErrorCode::INTERNAL_ERROR);
            assert_eq!(converted.data.as_ref().unwrap()["retriable"], false, "{e}");
            assert!(!converted.message.contains("OF_ENCRYPTION_KEY"));
            assert!(!converted.message.contains("otto_app"));
        }
        let db = from_tenant(&savvagent_tenant::Error::Db(sqlx::Error::PoolTimedOut));
        assert_eq!(db.data.unwrap()["retriable"], true);
    }

    /// A missing scope is named, because naming it is the only way the agent
    /// can fix it.
    #[test]
    fn a_missing_scope_is_named() {
        let scope = from_scope(&crate::auth::MissingScope("jobs:write".into()));
        assert!(scope.message.contains("jobs:write"));
        assert_eq!(scope.data.unwrap()["code"], "insufficient_scope");
    }
}
