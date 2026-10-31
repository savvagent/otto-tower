//! Who is calling the console API.
//!
//! # Two credentials, one identity
//!
//! The console API accepts either of two credentials, and both end in the same
//! call: the platform introspecting a platform-issued access token.
//!
//! ```text
//!   Authorization: Bearer sav_at_…  (or sav_pat_…)   scripts, CI, agents
//!       └─ PlatformClient::introspect
//!   Cookie: __Host-of_session=…                         the console in a browser
//!       └─ the session row ── held token pair ── refreshed if expiring
//!            └─ PlatformClient::introspect              (see `crate::console`)
//!
//!            └─ active? which user, org, role, scopes?
//!                 └─ the `{org}` in the path must be the token's org
//! ```
//!
//! [`authenticate`] is the one place a credential becomes an identity.
//! **A bearer token, when present, decides the request**, valid or not: it is never
//! "fixed" by falling back to a cookie, so a script's behaviour does not depend on
//! what a browser left lying around. Only a request with no `Authorization`
//! bearer looks at the cookie. Everything downstream of [`OrgCtx`] needs to know
//! nothing about which it was, except one thing: a cookie session is bound to one
//! org, and asking it for another is not "no such org" but a prompt to sign in to
//! that one — see [`ApiError::org_session_mismatch`].
//!
//! The cookie is an ambient credential, so cookie-authenticated writes are
//! guarded against cross-site requests ([`crate::csrf`]); a bearer is not ambient
//! and is exempt.
//!
//! # What an extractor guarantees
//!
//! [`OrgCtx`] resolves the caller, the org in the path, and their role before a
//! handler body runs, and a handler that needs more than membership says so in
//! one line. A handler that forgets is a handler that serves another tenant's
//! data, and a type is a better place for that than a review checklist.
//!
//! - **An org that is not the token's is `404`.** A token opens exactly one org,
//!   fixed when it was issued. Answering `403` on a real slug and `404` on a fake
//!   one turns any token into a directory of who uses the product. (A *cookie*
//!   session asking for another org is `401 org_session_mismatch` instead: the
//!   person is signed in, just not to that org, and the console's answer is to sign
//!   in again with an `org_hint`. It leaks nothing a login would not.)
//! - **A platform outage is `503`, never `401`.** Same reasoning as the MCP
//!   surface: `401` sends a client off to sign in again against the thing that is
//!   down.
//! - **Scopes are checked as the MCP tools check them.** A token that could not
//!   `repos:write` over MCP cannot over the console.

use axum::extract::{FromRef, FromRequestParts};
use http::request::Parts;
use savvagent_resource::{MemberInfo, Role, TokenClaims};
use savvagent_tenant::ids::{OrgId, UserId};

use crate::console;
use crate::cookies;
use crate::error::ApiError;
use crate::state::AppState;

/// Which credential authenticated a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Bearer,
    /// The console's session cookie.
    Cookie,
}

/// The bearer token on the request, if any.
pub fn bearer_token(parts: &Parts) -> Option<&str> {
    bearer_token_in(&parts.headers)
}

pub fn bearer_token_in(headers: &http::HeaderMap) -> Option<&str> {
    let raw = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Resolve the request's credential to the platform's claims about it.
///
/// The single place a credential becomes an identity. Everything else in the
/// console API takes the result.
pub async fn authenticate(
    state: &AppState,
    parts: &Parts,
) -> Result<(TokenClaims, Source), ApiError> {
    if let Some(token) = bearer_token(parts) {
        return introspect(state, token)
            .await?
            .map(|claims| (claims, Source::Bearer))
            .ok_or_else(ApiError::unauthenticated);
    }

    let cookie = cookies::session_cookie(&parts.headers).ok_or_else(ApiError::unauthenticated)?;
    let (token, org) = console::access_token(state, cookie).await?;
    match introspect(state, &token).await? {
        Some(claims) => Ok((claims, Source::Cookie)),
        None => {
            // The platform no longer honours the token it issued this session
            // (revoked, or the member left). Nothing will revive it.
            console::drop_session(state, org, cookie).await;
            Err(ApiError::unauthenticated())
        }
    }
}

async fn introspect(state: &AppState, token: &str) -> Result<Option<TokenClaims>, ApiError> {
    match state.platform.introspect(token).await {
        Ok(claims) => Ok(claims),
        Err(e) => Err(ApiError::platform_unavailable(
            "introspect a console token",
            e,
        )),
    }
}

/// The caller. Only an id: the user is the platform's record.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub id: UserId,
}

/// The org a request acts in.
#[derive(Debug, Clone)]
pub struct OrgRef {
    pub id: OrgId,
    pub slug: String,
}

/// The caller, acting in the one org their token opens.
#[derive(Debug, Clone)]
pub struct OrgCtx {
    pub user: Caller,
    pub org: OrgRef,
    /// The caller's role in the org as of the platform's last answer.
    pub role: Role,
    pub scopes: Vec<String>,
}

impl OrgCtx {
    /// Open a transaction pinned to this org, refused if the org was deleted or
    /// the caller removed since authentication (see
    /// `tower_core::platform_events::begin_live`). Every handler opens its
    /// transaction here, never with `Db::begin` directly.
    pub async fn begin(
        &self,
        db: &savvagent_tenant::Db,
    ) -> Result<savvagent_tenant::Tx<'static>, ApiError> {
        Ok(tower_core::platform_events::begin_live(db, self.org.id, Some(self.user.id)).await?)
    }

    /// Which team-scoped rows this caller may see: everything for an owner or
    /// admin, otherwise org-wide rows plus those of the teams the platform says
    /// they belong to. Fails closed: a platform that cannot answer is a `503`,
    /// never "all teams". Ask before opening a transaction, so a slow platform
    /// holds no pooled connection.
    pub async fn team_scope(
        &self,
        platform: &savvagent_resource::PlatformClient,
    ) -> Result<tower_core::teams::TeamScope, ApiError> {
        Ok(
            tower_core::teams::TeamScope::for_member(
                platform,
                self.org.id,
                self.user.id,
                self.role,
            )
            .await?,
        )
    }

    /// Refuse unless the caller administers this org.
    ///
    /// The message names the role they have, because "you need to be an admin"
    /// without saying what you are leaves a person guessing whether they are
    /// looking at the right org.
    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.role.can_administer() {
            return Ok(());
        }
        Err(ApiError::forbidden(format!(
            "this needs an owner or admin of {}; you are a {}",
            self.org.slug,
            role_name(self.role)
        )))
    }

    /// Refuse unless the token carries `scope`. Named in the message, because
    /// it is the one thing the caller can act on: ask for a token that has it.
    pub fn require_scope(&self, scope: &str) -> Result<(), ApiError> {
        if self.scopes.iter().any(|s| s == scope) {
            return Ok(());
        }
        Err(ApiError::forbidden(format!(
            "this token lacks the {scope} scope"
        )))
    }
}

pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::Owner => "owner",
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

/// Who is calling, as the platform sees them right now.
#[derive(Debug, Clone)]
pub struct Identity {
    pub claims: TokenClaims,
    /// The caller's user, the org their credential opens, and their role in it,
    /// from a fresh lookup rather than the (up to 60 s old) introspection claims:
    /// a demoted admin loses admin routes at once.
    pub member: MemberInfo,
    pub source: Source,
}

/// Authenticate, refuse tombstoned callers, and ask the platform who they are.
///
/// Everything an `{org}`-less route needs ([`Identity`]), and the first half of
/// [`OrgCtx`].
pub async fn identify(state: &AppState, parts: &Parts) -> Result<Identity, ApiError> {
    let (claims, source) = authenticate(state, parts).await?;

    // A deleted org or a just-removed member is refused now, though the
    // platform's cached introspection may still vouch for the token.
    match tower_core::platform_events::revoked(
        &state.db,
        claims.org_id.into(),
        claims.user_id.into(),
    )
    .await
    {
        Ok(false) => {}
        Ok(true) => return Err(ApiError::unauthenticated()),
        Err(e) => return Err(ApiError::unavailable("check revocation tombstones", e)),
    }

    let member = match state
        .platform
        .member(claims.org_id, claims.user_id)
        .await
        .map_err(|e| ApiError::platform_unavailable("look up the token's org", e))?
    {
        Some(member) if member.org.id == claims.org_id => member,
        // The platform no longer sees this user in this org: the credential is
        // dead, whatever the introspection cache still says.
        _ => return Err(ApiError::unauthenticated()),
    };

    Ok(Identity {
        claims,
        member,
        source,
    })
}

impl<S> FromRequestParts<S> for OrgCtx
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let state = AppState::from_ref(state);

        let wanted = org_param(parts, &state).await.ok_or_else(|| {
            // A route mounted without an `{org}` segment reaching this
            // extractor is a wiring bug, not a caller error.
            ApiError::internal("org path parameter", "route has no {org} segment")
        })?;

        let Identity {
            claims,
            member,
            source,
        } = identify(&state, parts).await?;
        let (slug, role) = (member.org.slug, member.role);

        // The path may name the org by id or by slug. The platform is asked for
        // the slug rather than trusting anything the client sent.
        let by_id = wanted
            .parse::<uuid::Uuid>()
            .is_ok_and(|id| id == claims.org_id);
        if !(by_id || wanted.eq_ignore_ascii_case(&slug)) {
            return Err(match source {
                // **An org that is not the token's is reported as not found, not
                // as forbidden** — see the module docs.
                Source::Bearer => {
                    ApiError::not_found(format!("no org {wanted:?} that this token opens"))
                }
                // A signed-in person asking for another of their orgs: say so,
                // so the console can send them through login for that one.
                Source::Cookie => ApiError::org_session_mismatch(),
            });
        }

        Ok(OrgCtx {
            user: Caller {
                id: claims.user_id.into(),
            },
            org: OrgRef {
                id: claims.org_id.into(),
                slug,
            },
            role,
            scopes: claims.scopes,
        })
    }
}

/// Read the `{org}` path segment.
///
/// `RawPathParams` rather than `Path<T>`: it clones the parameters out of the
/// request extensions rather than deserializing them into a type, so a handler
/// that also takes `Path<…>` for its *own* segments still sees everything.
async fn org_param(parts: &mut Parts, state: &AppState) -> Option<String> {
    let params = axum::extract::RawPathParams::from_request_parts(parts, state)
        .await
        .ok()?;
    params
        .iter()
        .find(|(key, _)| *key == "org")
        .map(|(_, value)| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts_with(auth: Option<&str>) -> Parts {
        let mut request = http::Request::builder();
        if let Some(auth) = auth {
            request = request.header(http::header::AUTHORIZATION, auth);
        }
        request.body(()).unwrap().into_parts().0
    }

    #[test]
    fn the_bearer_token_is_found() {
        for prefix in ["Bearer", "bearer", "BEARER"] {
            assert_eq!(
                bearer_token(&parts_with(Some(&format!("{prefix} sav_at_abc")))),
                Some("sav_at_abc")
            );
        }
    }

    #[test]
    fn other_schemes_and_empty_values_are_not_tokens() {
        for bad in [
            None,
            Some("Basic dXNlcjpwdw=="),
            Some("sav_at_abc"),
            Some("Bearer"),
            Some("Bearer   "),
        ] {
            assert_eq!(bearer_token(&parts_with(bad)), None, "{bad:?}");
        }
    }
}
