//! The console's sign-in: OAuth authorization code + PKCE against the platform.
//!
//! ```text
//!  browser                    Savvagent Tower                           Savvagent platform
//!     │ GET /auth/login?org=acme&next=/o/acme/jobs                       │
//!     ├───────────────────────────►│ state, PKCE verifier → sealed cookie│
//!     │◄── 303 + __Host-of_oauth ──┤                                     │
//!     │ GET /oauth/authorize?…code_challenge=S256(verifier)&org_hint=acme│
//!     ├─────────────────────────────────────────────────────────────────►│ (signs in, picks org)
//!     │◄──────────────────────────────── 303 /auth/callback?code&state ──┤
//!     │ GET /auth/callback (cookie)│                                     │
//!     ├───────────────────────────►│ POST /oauth/token (code, verifier) ─►│
//!     │                            │◄─ access + refresh token ────────────┤
//!     │                            │ introspect → user, org; store session│
//!     │◄ 303 next + __Host-of_session                                    │
//! ```
//!
//! The browser only ever holds two opaque cookies; the platform's tokens stay
//! here (`crate::console`). `/auth/login` and `/auth/callback` are `GET`s a
//! browser navigates to, so every failure is a redirect to `/?login_error=<code>`
//! for the console to render, never a JSON body a person would be left staring
//! at. The codes are a fixed set — nothing from the query string is reflected.

use axum::extract::{Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::console;
use crate::cookies::{self, OauthFlow};
use crate::error::{ApiError, ApiResult};
use crate::oauth::TokenError;
use crate::session::{self, role_name};
use crate::state::AppState;

/// What the console asks the platform for: exactly what its routes check
/// (`jobs:read` for the queue, `repos:*` for repos, `trackers` for connections,
/// `org:admin` for the audit trail). The platform grants the subset the person is
/// entitled to — `org:admin` is dropped for a non-admin — and the routes check
/// the grant, so asking for all of it is safe.
pub const CONSOLE_SCOPES: &[&str] = &[
    of_core::scopes::JOBS_READ,
    of_core::scopes::REPOS_READ,
    of_core::scopes::REPOS_WRITE,
    of_core::scopes::TRACKERS,
    of_core::scopes::ORG_ADMIN,
];

fn disabled() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "console_login_disabled",
        "console sign-in is not configured on this deployment (OF_CONSOLE_CLIENT_ID is unset)",
    )
}

/// A `303` with `Set-Cookie`s. Never cached: each of these carries or consumes
/// something single-use.
fn see_other(location: &str, set_cookies: impl IntoIterator<Item = HeaderValue>) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for cookie in set_cookies {
        headers.append(header::SET_COOKIE, cookie);
    }
    response
}

/// Back to the landing page with a reason, and the half-finished login cleared.
fn login_failed(code: &'static str) -> Response {
    see_other(
        &format!("/?login_error={code}"),
        [cookies::clear(cookies::OAUTH_COOKIE)],
    )
}

#[derive(Deserialize)]
pub struct LoginQuery {
    org: Option<String>,
    next: Option<String>,
}

pub async fn login(
    State(state): State<AppState>,
    Query(query): Query<LoginQuery>,
) -> ApiResult<Response> {
    let client_id = state
        .config
        .console_client_id
        .as_deref()
        .ok_or_else(disabled)?;

    let org = query.org.filter(|o| !o.is_empty());
    if let Some(org) = &org {
        if !cookies::is_org_slug(org) {
            return Err(ApiError::bad_request("org is not an organization slug"));
        }
    }
    // An unsafe `next` is dropped rather than refused: the person still gets
    // signed in, and lands on the default page.
    let next = query.next.as_deref().and_then(cookies::safe_next);

    let flow = OauthFlow {
        state: cookies::random_token(),
        verifier: cookies::random_token(),
        next,
        org,
        exp: chrono::Utc::now().timestamp() + cookies::OAUTH_MAX_AGE_SECS,
    };
    let sealed = flow
        .seal(&state.cipher)
        .ok_or_else(|| ApiError::internal("seal the oauth cookie", "seal failed"))?;

    let mut url = url::Url::parse(&format!("{}/oauth/authorize", state.config.platform_url))
        .map_err(|e| ApiError::internal("build the authorize URL", e))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &state.config.console_redirect_uri())
            .append_pair("code_challenge", &pkce_challenge(&flow.verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &flow.state)
            .append_pair("resource", &state.config.resource_uri)
            .append_pair("scope", &CONSOLE_SCOPES.join(" "));
        if let Some(org) = &flow.org {
            q.append_pair("org_hint", org);
        }
    }

    Ok(see_other(
        url.as_str(),
        [cookies::set(
            cookies::OAUTH_COOKIE,
            &sealed,
            cookies::OAUTH_MAX_AGE_SECS,
        )],
    ))
}

/// RFC 7636 `S256`: base64url, unpadded, of the SHA-256 of the verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> ApiResult<Response> {
    let client_id = state
        .config
        .console_client_id
        .as_deref()
        .ok_or_else(disabled)?;

    // The platform turned the person away (they declined, or are not a member of
    // any org, …). A fixed vocabulary: the query string is attacker-controlled.
    if let Some(error) = query.error.as_deref() {
        return Ok(login_failed(match error {
            "access_denied" => "access_denied",
            "temporarily_unavailable" | "server_error" => "platform_unavailable",
            _ => "failed",
        }));
    }

    // The flow this browser started, and nobody else's: without the cookie, a
    // callback is somebody else's login being fed to this browser.
    let Some(flow) = cookies::read(&headers, cookies::OAUTH_COOKIE)
        .and_then(|v| OauthFlow::open(&state.cipher, v, chrono::Utc::now().timestamp()))
    else {
        return Ok(login_failed("invalid_state"));
    };
    let (Some(code), Some(got_state)) = (query.code.as_deref(), query.state.as_deref()) else {
        return Ok(login_failed("failed"));
    };
    if !bool::from(got_state.as_bytes().ct_eq(flow.state.as_bytes())) {
        return Ok(login_failed("invalid_state"));
    }

    let tokens = match state
        .oauth
        .exchange_code(
            client_id,
            code,
            &state.config.console_redirect_uri(),
            &flow.verifier,
            &state.config.resource_uri,
        )
        .await
    {
        Ok(tokens) => tokens,
        Err(TokenError::InvalidGrant) => return Ok(login_failed("failed")),
        Err(TokenError::Rejected(reason)) => {
            tracing::error!(%reason, "the platform refused the console's code exchange; is OF_CONSOLE_CLIENT_ID registered with this redirect URI?");
            return Ok(login_failed("failed"));
        }
        Err(TokenError::Unavailable(reason)) => {
            tracing::error!(%reason, "the platform could not redeem the console's login code");
            return Ok(login_failed("platform_unavailable"));
        }
    };

    // Who did the platform just sign in? Ask it, rather than reading the token.
    let claims = match state.platform.introspect(&tokens.access_token).await {
        Ok(Some(claims)) => claims,
        Ok(None) => return Ok(login_failed("failed")),
        Err(e) => {
            tracing::error!(error = %e, "could not introspect the token the console just received");
            return Ok(login_failed("platform_unavailable"));
        }
    };
    match of_core::platform_events::revoked(&state.db, claims.org_id.into(), claims.user_id.into())
        .await
    {
        Ok(false) => {}
        Ok(true) => return Ok(login_failed("failed")),
        Err(e) => return Err(ApiError::unavailable("check revocation tombstones", e)),
    }
    let member = match state.platform.member(claims.org_id, claims.user_id).await {
        Ok(Some(member)) if member.org.id == claims.org_id => member,
        Ok(_) => return Ok(login_failed("failed")),
        Err(e) => {
            tracing::error!(error = %e, "could not look up the console's new member");
            return Ok(login_failed("platform_unavailable"));
        }
    };

    let cookie = match console::create(&state, &tokens, &claims).await {
        Ok(cookie) => cookie,
        // The tombstone check above, lost to a clean-up that started since:
        // the same answer it gives.
        Err(e) if e.status == http::StatusCode::UNAUTHORIZED => return Ok(login_failed("failed")),
        Err(e) => return Err(e),
    };

    // `next` was checked when it went into the cookie; it is checked again,
    // because the cookie is the one thing here a browser could have tampered
    // with if the sealing key were ever lost.
    let landing = flow
        .next
        .as_deref()
        .and_then(cookies::safe_next)
        .unwrap_or_else(|| format!("/o/{}", member.org.slug));

    Ok(see_other(
        &landing,
        [
            cookies::set(
                cookies::SESSION_COOKIE,
                &cookie,
                console::REFRESH_TTL_DAYS * 86_400,
            ),
            cookies::clear(cookies::OAUTH_COOKIE),
        ],
    ))
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(cookie) = cookies::session_cookie(&headers) {
        console::logout(&state, cookie).await?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, cookies::clear(cookies::SESSION_COOKIE));
    Ok(response)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    user: UserView,
    org: OrgView,
    role: &'static str,
    scopes: Vec<String>,
    /// Where the platform's own console lives: members, teams, SSO, usage,
    /// tokens, and the org list are managed there.
    platform_url: String,
}

#[derive(Serialize)]
struct UserView {
    id: uuid::Uuid,
    email: Option<String>,
    name: Option<String>,
}

#[derive(Serialize)]
struct OrgView {
    id: uuid::Uuid,
    slug: String,
    name: String,
    plan: String,
}

pub async fn get_session(State(state): State<AppState>, parts: Parts) -> ApiResult<Response> {
    let identity = session::identify(&state, &parts).await?;
    let member = identity.member;
    let view = SessionView {
        user: UserView {
            id: member.user.id,
            email: member.user.email,
            name: member.user.name,
        },
        org: OrgView {
            id: member.org.id,
            slug: member.org.slug,
            name: member.org.name,
            plan: member.org.plan,
        },
        role: role_name(member.role),
        scopes: identity.claims.scopes,
        platform_url: state.config.platform_url.clone(),
    };
    let mut response = Json(view).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B's worked example.
    #[test]
    fn the_pkce_challenge_matches_the_rfc_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn every_console_scope_is_one_the_server_understands() {
        for scope in CONSOLE_SCOPES {
            assert!(of_core::scopes::KNOWN.contains(scope), "{scope}");
        }
    }
}
