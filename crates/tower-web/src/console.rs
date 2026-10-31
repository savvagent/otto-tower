//! A console login session, from creation to refresh to logout.
//!
//! `/auth/callback` ends in [`create`]; every cookie-authenticated request goes
//! through [`access_token`], which hands back a platform access token that is good
//! for the request (refreshing first if it is about to expire) for
//! [`crate::session::authenticate`] to introspect like any other.
//!
//! # Refresh is single-flight, and survives a cancelled request
//!
//! The platform **rotates the refresh token on every use, and presenting a
//! consumed one revokes the whole token family**. Two requests that both notice
//! an expiring access token and both refresh would therefore sign the user out,
//! which is the opposite of what a refresh is for. So [`access_token`] does the
//! refresh holding `SELECT … FOR UPDATE` on the session's row
//! (`tower_core::console_sessions::lock`): a concurrent request blocks on the lock,
//! and when it gets it, re-reads the row, finds a fresh token, and refreshes
//! nothing. That holds across replicas, not just across tasks.
//!
//! The refresh runs in its own spawned task. Once the platform has rotated the
//! pair, persisting the new one is not optional — the old refresh token is spent —
//! and an `axum` handler future is dropped when its client disconnects. A task
//! that outlives its request finishes the write.
//!
//! What a refresh can answer:
//!
//! | Platform says | Session | Request |
//! |---|---|---|
//! | new pair | rotated in place | proceeds |
//! | `invalid_grant` | deleted | `401` — the person signs in again |
//! | unreachable / 5xx | untouched | `503` — never `401`, which would send them to sign in against the platform that is down |

use chrono::{Duration, Utc};
use savvagent_resource::TokenClaims;
use savvagent_tenant::ids::OrgId;
use tower_core::console_sessions::{self, NewSession};

use crate::cookies;
use crate::error::ApiError;
use crate::oauth::{TokenError, TokenSet};
use crate::state::AppState;

/// How long the platform's refresh tokens live, and so how long a session that
/// is never refreshed lasts. Each rotation issues a fresh one, which restarts it.
pub const REFRESH_TTL_DAYS: i64 = 30;

/// Used when the platform's token response omits `expires_in`.
const DEFAULT_ACCESS_TTL_SECS: i64 = 3600;

fn access_expiry(tokens: &TokenSet) -> chrono::DateTime<Utc> {
    let secs = tokens.expires_in.unwrap_or(DEFAULT_ACCESS_TTL_SECS).max(1);
    Utc::now() + Duration::seconds(secs)
}

/// Store a new session for a freshly redeemed token pair and return the cookie
/// value that opens it.
pub async fn create(
    state: &AppState,
    tokens: &TokenSet,
    claims: &TokenClaims,
) -> Result<String, ApiError> {
    let cookie = cookies::random_token();
    store(state, &cookie, tokens, claims).await?;

    // Opportunistic purge: logins are rare, so this is rarely more than a
    // cheap indexed delete. Never fails a login.
    match console_sessions::purge_expired(&state.db).await {
        Ok(0) => {}
        Ok(n) => tracing::debug!(purged = n, "purged expired console sessions"),
        Err(e) => tracing::warn!(error = %e, "could not purge expired console sessions"),
    }
    Ok(cookie)
}

async fn store(
    state: &AppState,
    cookie: &str,
    tokens: &TokenSet,
    claims: &TokenClaims,
) -> Result<(), ApiError> {
    console_sessions::create(
        &state.db,
        &state.cipher,
        &NewSession {
            id_hash: console_sessions::hash_cookie(cookie),
            user_id: claims.user_id.into(),
            org_id: claims.org_id.into(),
            access_token: &tokens.access_token,
            refresh_token: &tokens.refresh_token,
            access_expires_at: access_expiry(tokens),
            scopes: &claims.scopes,
            expires_at: Utc::now() + Duration::days(REFRESH_TTL_DAYS),
        },
    )
    .await
    .map_err(|e| match e {
        // The org was deleted or the member removed while they were signing in.
        tower_core::Error::AccessRevoked => ApiError::from(e),
        e => ApiError::unavailable("store a console session", e),
    })
}

/// A platform access token for the session behind `cookie` (and the org the
/// session is bound to), valid for this request. `401` when there is no such session (or it has expired, or the
/// platform no longer honours it).
pub async fn access_token(state: &AppState, cookie: &str) -> Result<(String, OrgId), ApiError> {
    let id_hash = console_sessions::hash_cookie(cookie);
    let session = console_sessions::find(&state.db, &id_hash)
        .await
        .map_err(|e| ApiError::unavailable("look up a console session", e))?
        .ok_or_else(ApiError::unauthenticated)?;

    let org = session.org_id;
    if !session.needs_refresh(Utc::now()) {
        return Ok((open_access(state, &session, &id_hash).await?, org));
    }

    // Detached: see the module docs.
    let task_state = state.clone();
    let token = tokio::spawn(async move { refresh(task_state, org, id_hash).await })
        .await
        .map_err(|e| ApiError::internal("console session refresh task", e))??;
    Ok((token, org))
}

/// Open a session's access token. A token that cannot be opened (the
/// encryption key was rotated) is a dead session, not a server error.
async fn open_access(
    state: &AppState,
    session: &console_sessions::Session,
    id_hash: &[u8; 32],
) -> Result<String, ApiError> {
    match session.access_token(&state.cipher) {
        Ok(token) => Ok(token),
        Err(e) => {
            tracing::error!(error = %e, "a console session's token cannot be opened; dropping it");
            let _ = console_sessions::delete(&state.db, session.org_id, id_hash).await;
            Err(ApiError::unauthenticated())
        }
    }
}

async fn refresh(state: AppState, org: OrgId, id_hash: [u8; 32]) -> Result<String, ApiError> {
    let Some((mut tx, session)) = console_sessions::lock(&state.db, org, &id_hash)
        .await
        .map_err(|e| ApiError::unavailable("lock a console session", e))?
    else {
        // Logged out, purged, or its member removed since it was found.
        return Err(ApiError::unauthenticated());
    };

    // The single-flight recheck: a request that was waiting on the lock finds
    // the winner's fresh token here and refreshes nothing.
    if !session.needs_refresh(Utc::now()) {
        let token = open_access(&state, &session, &id_hash).await;
        tx.commit()
            .await
            .map_err(|e| ApiError::unavailable("release a console session", e))?;
        return token;
    }

    let Some(client_id) = state.config.console_client_id.as_deref() else {
        // Login was switched off after this session was made. Nothing to
        // refresh with, and nothing to delete: switching it back on revives it.
        return Err(ApiError::unauthenticated());
    };
    let refresh_token = match session.refresh_token(&state.cipher) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "a console session's refresh token cannot be opened; dropping it");
            console_sessions::delete_locked(&mut tx, &id_hash)
                .await
                .map_err(|e| ApiError::unavailable("drop a console session", e))?;
            tx.commit()
                .await
                .map_err(|e| ApiError::unavailable("drop a console session", e))?;
            return Err(ApiError::unauthenticated());
        }
    };

    match state
        .oauth
        .refresh(client_id, &refresh_token, &state.config.resource_uri)
        .await
    {
        Ok(tokens) => {
            console_sessions::rotate(
                &mut tx,
                &state.cipher,
                &id_hash,
                &tokens.access_token,
                &tokens.refresh_token,
                access_expiry(&tokens),
                Utc::now() + Duration::days(REFRESH_TTL_DAYS),
            )
            .await
            .map_err(|e| ApiError::unavailable("store a refreshed console session", e))?;
            tx.commit()
                .await
                .map_err(|e| ApiError::unavailable("store a refreshed console session", e))?;
            Ok(tokens.access_token)
        }
        Err(TokenError::InvalidGrant) => {
            tracing::info!(org = %org, "console session refresh refused as invalid_grant; dropping the session");
            console_sessions::delete_locked(&mut tx, &id_hash)
                .await
                .map_err(|e| ApiError::unavailable("drop a console session", e))?;
            tx.commit()
                .await
                .map_err(|e| ApiError::unavailable("drop a console session", e))?;
            Err(ApiError::unauthenticated())
        }
        Err(e) => {
            // Untouched: the refresh token is still unspent as far as we know.
            let _ = tx.rollback().await;
            Err(ApiError::platform_unavailable(
                "refresh a console session",
                e,
            ))
        }
    }
}

/// Forget a session whose token the platform no longer honours. Best effort.
pub async fn drop_session(state: &AppState, org: OrgId, cookie: &str) {
    let id_hash = console_sessions::hash_cookie(cookie);
    if let Err(e) = console_sessions::delete(&state.db, org, &id_hash).await {
        tracing::warn!(error = %e, "could not drop a dead console session");
    }
}

/// Sign out: revoke the refresh token at the platform (best effort — a logout
/// must work while the platform is down), then delete the row.
///
/// Takes the row lock first, so a logout cannot interleave with a refresh and
/// revoke a token the refresh has just spent.
pub async fn logout(state: &AppState, cookie: &str) -> Result<(), ApiError> {
    let id_hash = console_sessions::hash_cookie(cookie);
    let Some(session) = console_sessions::find(&state.db, &id_hash)
        .await
        .map_err(|e| ApiError::unavailable("look up a console session", e))?
    else {
        return Ok(());
    };
    let Some((mut tx, session)) = console_sessions::lock(&state.db, session.org_id, &id_hash)
        .await
        .map_err(|e| ApiError::unavailable("lock a console session", e))?
    else {
        return Ok(());
    };

    if let (Some(client_id), Ok(refresh_token)) = (
        state.config.console_client_id.as_deref(),
        session.refresh_token(&state.cipher),
    ) {
        if let Err(e) = state.oauth.revoke(client_id, &refresh_token).await {
            tracing::warn!(error = %e, "could not revoke a console session's refresh token at the platform");
        }
    }

    console_sessions::delete_locked(&mut tx, &id_hash)
        .await
        .map_err(|e| ApiError::unavailable("delete a console session", e))?;
    tx.commit()
        .await
        .map_err(|e| ApiError::unavailable("delete a console session", e))?;
    Ok(())
}
