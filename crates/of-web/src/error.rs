//! The console API's error envelope.
//!
//! Every failure leaves here as `{"error": {"code": …, "message": …}}` with a
//! status that matches. `code` is the stable branch point a UI switches on;
//! `message` is what it shows a human. That is the same split `of-core::Error`
//! already makes for agents, and it is deliberately the same envelope shape, so
//! a person debugging the console and a person debugging an agent are reading
//! the same thing.
//!
//! **Two rules about what goes in `message`.**
//!
//! A database error is never one of them. `of_core::Error::Db` carries table
//! and constraint names, which tell an attacker about a schema they cannot
//! otherwise see and tell a user nothing they can act on. It is logged in full
//! and reported as a flat internal error.
//!
//! The same goes for a failed call to the platform: its response text is logged,
//! and the caller is told only that the platform could not be reached.

use axum::response::{IntoResponse, Response};
use axum::Json;
use http::{header, HeaderValue, StatusCode};
use of_core::Error as CoreError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// Seconds until a throttled caller may retry, rendered as `Retry-After`.
    pub retry_after: Option<i64>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    /// No usable credential: none was sent, or the platform does not vouch for
    /// the one that was. The console reads this and sends the user to sign in.
    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "sign in to continue: this needs a console session or a valid otto platform access token",
        )
    }

    /// A signed-in console session asked for an org it is not signed in to. A
    /// session is bound to one org (the token the platform issued it is), so the
    /// answer is to sign in again for the other one, not "not found".
    pub fn org_session_mismatch() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "org_session_mismatch",
            "this session is for a different organization; sign in again to switch",
        )
    }

    /// Signed in, but not allowed to do this.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    /// An unexpected failure. The detail is logged, never sent.
    pub fn internal(context: &str, e: impl std::fmt::Display) -> Self {
        tracing::error!(error = %e, context, "console request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "something went wrong on our side; try again shortly",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(serde_json::json!({
                "error": { "code": self.code, "message": self.message },
            })),
        )
            .into_response();

        if let Some(secs) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&secs.max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }

        response
    }
}

impl ApiError {
    /// The database could not answer. `503`, not `500`: it is a "try again"
    /// condition, and a client that treats it as a server bug or an auth
    /// failure will do the wrong thing. The detail is logged, never sent.
    pub fn unavailable(context: &str, e: impl std::fmt::Display) -> Self {
        tracing::error!(error = %e, context, "database unavailable");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "could not reach the database right now; retry shortly",
        )
    }

    /// The platform could not answer. `503` and never `401`: the caller's
    /// credential may be perfectly good, and answering `401` would send a client
    /// to sign in again against the very thing that is down. The detail is
    /// logged, never sent.
    pub fn platform_unavailable(context: &str, e: impl std::fmt::Display) -> Self {
        tracing::error!(error = %e, context, "the otto platform could not answer");
        let mut api = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "platform_unavailable",
            "the otto platform could not be reached to check this; retry shortly",
        );
        api.retry_after = Some(5);
        api
    }
}

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        use CoreError::*;

        // Tenancy failures carry their own codes and wording and are mapped on
        // their own below.
        let e = match e {
            Tenant(inner) => return ApiError::from(inner),
            // The platform could not answer an identity question this request
            // depended on (is this team real?). Refused, and retriable.
            Platform(inner) => return ApiError::platform_unavailable("platform lookup", inner),
            // The same answer authentication gives a tombstoned org or user;
            // this is that check, re-run under the org's lifecycle lock.
            AccessRevoked => return ApiError::unauthenticated(),
            other => other,
        };

        // `Db` never reaches the caller. Everything else in `of-core::Error`
        // was written to be read by whoever hit it.
        if let Db(inner) = &e {
            return ApiError::internal("of-core", inner);
        }

        let status = match &e {
            JobNotFound(_)
            | NoJobForTicket(_)
            | RepoNotFound(_)
            | RepoUnresolved { .. }
            | TeamNotFound { .. }
            | TeamChannelNotFound { .. } => StatusCode::NOT_FOUND,

            RepoSlugTaken(_)
            | RemoteTaken(..)
            | LeaseHeld { .. }
            | LeaseNotHeld(_)
            | AlreadyClaimed { .. }
            | TicketAlreadyLinked { .. }
            | AmbiguousTicket { .. }
            | IdempotencyKeyConflict { .. } => StatusCode::CONFLICT,

            WrongStatus { .. }
            | DependencyCycle(..)
            | Invalid(_)
            | InvalidAgentLabel { .. }
            | InvalidRepoSlug { .. } => StatusCode::BAD_REQUEST,

            // The withheld error's own status: a cycle is a bad request, the
            // rest are conflicts with a row the caller cannot see.
            Redacted { code, .. } if *code == "dependency_cycle" => StatusCode::BAD_REQUEST,
            Redacted { .. } => StatusCode::CONFLICT,

            // Retriable, not the caller's fault — the same distinction
            // retriable() already draws at the MCP layer. 503, not 500: this
            // is specifically a "try again" condition, and its message
            // (unlike Db's) is already safe to show as-is.
            RaceLost(_) => StatusCode::SERVICE_UNAVAILABLE,

            Db(_) | Platform(_) | Tenant(_) | AccessRevoked => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let mut api = ApiError::new(status, e.code(), e.to_string());
        // Rare, and the only signal an operator gets if it fires more than
        // expected -- or if send_message's currently-unreachable case (see
        // that site's own comment in messages.rs) is ever reached by a
        // future change. Unlike Db's ApiError::internal path, this does not
        // redact the message; it only adds the log side-effect and a small
        // retry hint, since the race this describes is expected to resolve
        // almost immediately.
        if let RaceLost(_) = &e {
            tracing::warn!(
                message = %api.message,
                "a lost unique-violation race surfaced to the console API"
            );
            api.retry_after = Some(1);
        }
        api
    }
}

/// Tenant-substrate failures (`otto-tenant`).
///
/// `IsolationNotEnforced` is a startup assertion, so arriving here at all would
/// mean a server that promised to refuse to serve is serving. It is logged in
/// full and answered vaguely: its message names database roles and tables,
/// which is infrastructure detail no HTTP client should be handed.
/// `Config`/`Crypto` carry the same shape of risk -- key-material and
/// ciphertext diagnostics, never an HTTP client's business.
impl From<otto_tenant::Error> for ApiError {
    fn from(e: otto_tenant::Error) -> Self {
        use otto_tenant::Error::*;

        match &e {
            Db(inner) => ApiError::internal("otto-tenant", inner),
            IsolationNotEnforced { .. } | Config(_) | Crypto(_) => {
                ApiError::internal("otto-tenant", &e)
            }
            Invalid(_) => ApiError::new(StatusCode::BAD_REQUEST, e.code(), e.to_string()),
            OrgNotFound(_) => ApiError::new(StatusCode::NOT_FOUND, e.code(), e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one regression this file exists to prevent. A `Db` error carries
    /// constraint and column names; if it ever reaches a response body, an
    /// attacker gets a free schema dump and the user gets nothing useful.
    #[test]
    fn database_errors_never_reach_the_caller() {
        let leaky = CoreError::Db(sqlx::Error::Protocol(
            "duplicate key value violates unique constraint \"repos_org_id_slug_key\"".into(),
        ));
        let api = ApiError::from(leaky);

        assert_eq!(api.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(api.code, "internal_error");
        assert!(
            !api.message.contains("repos_org_id"),
            "the schema leaked into the response: {}",
            api.message
        );
    }

    /// Unlike `Db`, `RaceLost`'s message is already written to be read by
    /// whoever hit it — it must reach the caller intact, not the redacted
    /// generic string `internal()` produces, and its status is 503 (retry),
    /// not 500, matching the distinction `retriable()` already draws.
    ///
    /// It also carries a `retry_after` hint, unlike every other non-429
    /// error this module produces: an agent told "retriable" with nothing
    /// else to go on could hot-loop, and the race this describes is
    /// expected to resolve almost immediately, so `Some(1)` (second) is
    /// enough.
    #[test]
    fn race_lost_maps_to_service_unavailable_with_its_own_message() {
        let e = CoreError::RaceLost("add_job lost a race".into());
        let api = ApiError::from(e);

        assert_eq!(api.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(api.code, "race_lost");
        assert_eq!(api.message, "add_job lost a race");
        assert_eq!(api.retry_after, Some(1));
    }

    /// A `remote_taken` withheld from a team-limited caller is still the
    /// conflict it was, under the same code, so a client branching on either
    /// takes the same branch whether or not the owning repo was named.
    #[test]
    fn a_redacted_remote_taken_is_still_a_conflict_with_its_code() {
        let scope = of_core::teams::TeamScope::Teams(Default::default());
        let e = scope.redact_foreign_ids(CoreError::RemoteTaken(
            "github.com/acme/secret".into(),
            "secret-repo".into(),
        ));
        let api = ApiError::from(e);

        assert_eq!(api.status, StatusCode::CONFLICT);
        assert_eq!(api.code, "remote_taken");
        assert!(!api.message.contains("secret-repo"), "{}", api.message);
    }

    /// An issue reference naming no job is the same 404 as an unknown job id;
    /// one naming several is a conflict the caller resolves by picking an id.
    #[test]
    fn ticket_references_map_to_not_found_and_conflict() {
        let api = ApiError::from(CoreError::NoJobForTicket("#928".into()));
        assert_eq!(api.status, StatusCode::NOT_FOUND);
        assert_eq!(api.code, "job_not_found");

        let api = ApiError::from(CoreError::AmbiguousTicket {
            ticket: "#928".into(),
            jobs: vec!["job-2".into(), "job-1".into()],
            more: false,
        });
        assert_eq!(api.status, StatusCode::CONFLICT);
        assert_eq!(api.code, "ambiguous_ticket_ref");
    }

    /// A team the platform does not know is a plain 404 (the repo was never
    /// created), and a platform that cannot answer is a retriable 503, never a
    /// 401 and never a silent success.
    #[test]
    fn an_unknown_team_is_not_found_and_an_unreachable_platform_is_unavailable() {
        let api = ApiError::from(CoreError::TeamNotFound {
            team: "0192f0c8-0000-7000-8000-000000000000".into(),
        });
        assert_eq!(api.status, StatusCode::NOT_FOUND);
        assert_eq!(api.code, "team_not_found");

        let api = ApiError::from(CoreError::Platform(otto_resource::Error::Status {
            status: 503,
            body: "db-7.internal exploded".into(),
        }));
        assert_eq!(api.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(api.code, "platform_unavailable");
        assert_eq!(api.retry_after, Some(5));
        assert!(
            !api.message.contains("db-7"),
            "the platform's response leaked: {}",
            api.message
        );
    }

    #[test]
    fn a_throttled_response_carries_retry_after() {
        let mut api = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "slow down");
        api.retry_after = Some(60);
        let response = api.into_response();
        assert_eq!(
            response.headers().get(header::RETRY_AFTER).unwrap(),
            "60",
            "a 429 without Retry-After leaves a client guessing"
        );
    }
}
