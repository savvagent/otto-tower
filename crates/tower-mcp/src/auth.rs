//! The OAuth resource server: what stands between a bearer token and the queue.
//!
//! The authorization server is the Savvagent platform; it mints tokens and this
//! service never sees how. This module is the *resource* server, and it has
//! exactly three jobs, in this order:
//!
//! 1. **Refuse an unauthenticated request in a way that teaches the client how
//!    to authenticate.** A `401` carrying
//!    `WWW-Authenticate: Bearer resource_metadata="…"` (RFC 9728) is the entire
//!    onboarding story: the user pastes one MCP URL into their agent, the agent
//!    gets this header, follows it to the metadata document, finds the
//!    authorization server (the platform), registers itself, and opens a
//!    browser. Nothing else is configured anywhere. Get this header wrong and the
//!    product's premise — one URL, no install — stops working, in a way that
//!    looks to the user like "the server is broken".
//! 2. **Ask the platform whether the token is good for us** (RFC 7662
//!    introspection, `PlatformClient::introspect`). The platform enforces the
//!    audience: a token minted for any other resource comes back inactive. This
//!    is the confused-deputy defense and it is why this service's canonical URI
//!    is configuration rather than something derived from the request's `Host`
//!    header — a header an attacker controls is not a thing to compare an
//!    audience against.
//! 3. **Attach the principal to the request**, so handlers downstream have an
//!    org and a user without re-deciding authorization per tool.
//!
//! **A platform outage is a `503`, never a `401`.** `401` tells an agent its
//! token is dead and sends it to re-authenticate against the very platform that
//! is down; every connected agent would stampede it at the worst moment. Only
//! the platform *answering* "inactive" is a `401`.
//!
//! **The principal is per request, not per session.** An MCP session spans many
//! HTTP requests, and the token is introspected on every one. The client caches a
//! positive answer for 60 seconds, which is the whole revocation delay: a token
//! revoked, or a member removed, at the platform stops working here within that
//! window rather than whenever the MCP session happens to end.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use chrono::{DateTime, Utc};
use http::{HeaderMap, StatusCode};
use savvagent_resource::{PlatformClient, Role, TokenClaims, TokenKind};
use savvagent_tenant::ids::{OrgId, UserId};
use savvagent_tenant::Db;
use uuid::Uuid;

use crate::legacy::Legacy;

/// Everything the middleware needs, shared by the whole surface.
#[derive(Clone)]
pub struct ResourceServer {
    /// Consulted for tombstones (a deleted org, a just-removed member) that must
    /// refuse a token the platform's cached introspection still vouches for.
    pub db: Db,
    /// The platform: authorization server, identity directory, billing.
    pub platform: Arc<PlatformClient>,
    /// This resource's canonical URI — the audience every token must name.
    ///
    /// Configuration, never derived from the request. A `Host` header is
    /// attacker-controlled, and an audience check against attacker-controlled
    /// input is not a check.
    pub resource_uri: String,
    /// Public base URL of *this* service, for the discovery pointer in a `401`.
    pub public_url: String,
    /// Base URL of the authorization server (the platform), advertised in the
    /// protected-resource metadata so clients know where to register and
    /// authorize.
    pub authorization_server: String,
}

impl ResourceServer {
    pub fn new(
        db: Db,
        platform: Arc<PlatformClient>,
        resource_uri: impl Into<String>,
        public_url: impl Into<String>,
        authorization_server: impl Into<String>,
    ) -> Self {
        Self {
            db,
            platform,
            resource_uri: resource_uri.into(),
            public_url: public_url.into(),
            authorization_server: authorization_server.into(),
        }
    }

    /// Where an unauthenticated client is sent to find out what to do.
    pub fn metadata_url(&self) -> String {
        metadata_url(&self.public_url)
    }

    /// As [`Self::metadata_url`], on the host the request came through: a
    /// legacy host points at its own copy of the document, so a client that
    /// started there finds a `resource` equal to the URL it was given.
    pub fn metadata_url_for(&self, legacy: Option<&Legacy>) -> String {
        legacy.map_or_else(|| self.metadata_url(), Legacy::metadata_url)
    }

    /// The `resource` the metadata document names on this request's host.
    ///
    /// Discovery only. The audience a token is checked against is
    /// [`Self::resource_uri`] on every host, because the platform is asked as
    /// that resource and nothing else; a legacy host changes what is
    /// *advertised*, never what is *accepted*.
    pub fn advertised_resource(&self, legacy: Option<&Legacy>) -> String {
        legacy.map_or_else(|| self.resource_uri.clone(), Legacy::resource_uri)
    }
}

/// The authenticated caller of a tool call: what the platform asserted about the
/// token, in the shape handlers use.
///
/// Built only from [`TokenClaims`] the platform returned for this service's own
/// audience (see [`Principal::from`]); handlers never construct one from request
/// data.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub token_id: Uuid,
    pub user_id: UserId,
    /// Fixed when the token was issued: a token opens exactly one org.
    pub org_id: OrgId,
    /// The caller's role in that org *now*, as of the last introspection.
    pub role: Role,
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
    pub kind: TokenKind,
    pub expires_at: DateTime<Utc>,
}

impl From<TokenClaims> for Principal {
    fn from(c: TokenClaims) -> Self {
        Self {
            token_id: c.token_id,
            user_id: c.user_id.into(),
            org_id: c.org_id.into(),
            role: c.role,
            client_id: c.client_id,
            scopes: c.scopes,
            kind: c.kind,
            expires_at: c.expires_at,
        }
    }
}

/// A token that lacks a scope a tool needs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("this token lacks the {0} scope")]
pub struct MissingScope(pub String);

impl Principal {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Require a scope, or fail. Used at the top of every tool handler that
    /// reads or mutates anything.
    pub fn require_scope(&self, scope: &str) -> Result<(), MissingScope> {
        if self.has_scope(scope) {
            Ok(())
        } else {
            Err(MissingScope(scope.to_string()))
        }
    }
}

/// Free-standing so the discovery pointer and the challenge that carries it can
/// be tested without a database handle. Neither depends on one, and a unit test
/// that has to build a connection pool to check a header string is a unit test
/// that will not be run.
fn metadata_url(public_url: &str) -> String {
    format!(
        "{}/.well-known/oauth-protected-resource",
        public_url.trim_end_matches('/')
    )
}

/// Extract a bearer token from an `Authorization` header.
///
/// The scheme is compared case-insensitively because RFC 7235 says auth schemes
/// are case-insensitive and real clients send `bearer`. The token itself is
/// trimmed but otherwise untouched — it is compared by hash, so any
/// normalization here could only turn a valid token into an invalid one.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Build the `401` that tells a client where to authenticate.
///
/// `error` and `error_description` follow RFC 6750; `resource_metadata` follows
/// RFC 9728 and is the field MCP clients actually read.
fn challenge(metadata_url: &str, reason: &str) -> Response {
    let header = format!(
        r#"Bearer realm="savvagent-tower", error="invalid_token", error_description="{reason}", resource_metadata="{metadata_url}""#
    );

    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "invalid_token",
            "error_description": reason,
            "resource_metadata": metadata_url,
        })),
    )
        .into_response();

    // A header value that fails to build would silently drop the discovery
    // pointer and leave the client with a bare 401 it cannot act on, so fall
    // back to the pointer alone rather than to nothing.
    let value = http::HeaderValue::from_str(&header)
        .unwrap_or_else(|_| http::HeaderValue::from_static(r#"Bearer realm="savvagent-tower""#));
    response
        .headers_mut()
        .insert(http::header::WWW_AUTHENTICATE, value);
    // The pointer differs on a legacy host, so a cache must not hand one
    // host's challenge to a caller of the other.
    response.headers_mut().insert(
        http::header::VARY,
        http::HeaderValue::from_static(crate::legacy::VARY_BY_HOST),
    );

    response
}

/// Reject the request unless it carries a live token audienced for us.
///
/// On success the [`Principal`] is inserted into the request's extensions,
/// where the MCP transport carries it through to tool handlers as part of
/// `http::request::Parts`.
pub async fn require_bearer(
    State(rs): State<Arc<ResourceServer>>,
    mut req: Request,
    next: Next,
) -> Response {
    // Which discovery document to point at. The legacy marker (set by the
    // outermost layer from configured hosts only) selects the URL; it plays no
    // part in deciding whether the token is good — `introspect` below asks the
    // platform as `rs.resource_uri` whichever host this came through.
    let metadata_url = rs.metadata_url_for(req.extensions().get::<Legacy>());

    let Some(token) = bearer(req.headers()) else {
        return challenge(&metadata_url, "an OAuth 2.1 bearer token is required");
    };

    match rs.platform.introspect(token).await {
        Ok(Some(claims)) => {
            // The platform vouches for this token (possibly from a cache up to
            // 60 s old), but it may have told us since that the org is gone or
            // the user was removed: honour that now rather than at cache expiry.
            // A database failure here is an outage (503), not "not revoked".
            match tower_core::platform_events::revoked(
                &rs.db,
                claims.org_id.into(),
                claims.user_id.into(),
            )
            .await
            {
                Ok(false) => {}
                Ok(true) => {
                    return challenge(
                        &metadata_url,
                        "this token is no longer valid for this resource; sign in again",
                    )
                }
                Err(e) => {
                    tracing::error!(error = %e, "could not check the revocation tombstones");
                    return unavailable();
                }
            }
            req.extensions_mut().insert(Principal::from(claims));
            next.run(req).await
        }

        // The platform answered, and the answer is "no": unknown, expired,
        // revoked, minted for a different resource server, or its user left the
        // org. One answer on purpose — the distinctions are an oracle, and an
        // agent cannot act on them differently anyway. The audience case is the
        // one a client can fix by changing what it asks for, but the platform
        // does not tell us which it was, so the message covers it.
        Ok(None) => challenge(
            &metadata_url,
            "this token is not valid for this resource; obtain a new one, requesting \
             the resource indicator this server advertises",
        ),

        // Anything else means we could not find out. That is not an
        // authentication failure, and answering `401` would be actively harmful
        // (see the module docs). Covers a transport failure, a 5xx, a platform
        // that rejected *our* credential (a deployment fault, and not something
        // the caller can fix by re-authenticating), and a body we cannot read.
        Err(e) => {
            tracing::error!(error = %e, "token introspection failed");
            unavailable()
        }
    }
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(http::header::RETRY_AFTER, "5")],
        Json(serde_json::json!({
            "error": "temporarily_unavailable",
            "error_description":
                "could not verify the token right now; retry shortly. \
                 Your credentials are fine.",
        })),
    )
        .into_response()
}

/// `GET /.well-known/oauth-protected-resource` (RFC 9728).
///
/// Served from here, beside the `/mcp` endpoint it describes, because the `401`
/// above points at it. `authorization_servers` is the platform: this service
/// issues no tokens and runs no authorization endpoints of its own.
///
/// The scopes come from [`tower_core::scopes::KNOWN`] — what this binary
/// understands — which is also what the operator registers with the platform
/// (`savvagent-platform-server resource register --scopes …`), so what is advertised
/// is what the authorization server will issue.
///
/// On a legacy host the document names that host's own resource URI
/// ([`ResourceServer::advertised_resource`]). RFC 9728 §3.3 has the client
/// compare `resource` with the URL it started from, so naming the canonical URI
/// there would fail every fresh sign-in on the old address until the sunset.
pub async fn protected_resource_metadata(
    State(rs): State<Arc<ResourceServer>>,
    legacy: Option<Extension<Legacy>>,
) -> Response {
    let resource = rs.advertised_resource(legacy.as_ref().map(|Extension(l)| l));
    let vary = [(http::header::VARY, crate::legacy::VARY_BY_HOST)];
    (
        vary,
        Json(serde_json::json!({
            "resource": resource,
            "resource_name": tower_core::scopes::RESOURCE_NAME,
            "authorization_servers": [rs.authorization_server.trim_end_matches('/')],
            "scopes_supported": tower_core::scopes::KNOWN,
            "bearer_methods_supported": ["header"],
        })),
    )
        .into_response()
}

/// The authenticated caller of a tool call.
///
/// Handlers take `Extension(parts): Extension<http::request::Parts>` — the MCP
/// transport's only channel from HTTP into a tool — and pull the principal back
/// out with this.
pub fn principal_from(parts: &http::request::Parts) -> Option<Principal> {
    parts.extensions.get::<Principal>().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(http::header::AUTHORIZATION, value.parse().unwrap());
        h
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        for prefix in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            assert_eq!(
                bearer(&headers(&format!("{prefix} sav_at_abc"))),
                Some("sav_at_abc"),
                "{prefix} should be accepted"
            );
        }
    }

    #[test]
    fn other_schemes_and_malformed_headers_carry_no_token() {
        for bad in [
            "Basic dXNlcjpwdw==",
            "sav_at_abc",
            "Bearer",
            "Bearer   ",
            "",
        ] {
            assert_eq!(bearer(&headers(bad)), None, "{bad:?} should yield no token");
        }
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    /// The header a client cannot follow is the header that breaks onboarding.
    /// Every field an MCP client reads has to be present and well-formed.
    #[test]
    fn the_challenge_points_at_the_metadata_document() {
        let response = challenge(
            &metadata_url("https://mcp.example.com"),
            "an OAuth 2.1 bearer token is required",
        );

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let header = response
            .headers()
            .get(http::header::WWW_AUTHENTICATE)
            .expect("no WWW-Authenticate header")
            .to_str()
            .unwrap();

        assert!(header.starts_with("Bearer "));
        assert!(header.contains(r#"error="invalid_token""#));
        assert!(header.contains(
            r#"resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource""#
        ));
        assert_eq!(
            response.headers()[http::header::VARY],
            "X-Forwarded-Host, Host"
        );
    }

    /// A trailing slash on the configured public URL must not produce a
    /// double-slashed metadata URL — some clients normalize that away and some
    /// fetch it verbatim and 404.
    #[test]
    fn the_metadata_url_survives_a_trailing_slash() {
        assert_eq!(
            metadata_url("https://mcp.example.com/"),
            metadata_url("https://mcp.example.com"),
            "a configured trailing slash must not produce a double-slashed URL — \
             some clients normalize that away and some fetch it verbatim and 404"
        );
    }
}
