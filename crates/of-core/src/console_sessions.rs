//! The console's login sessions: a platform token pair held for a browser.
//!
//! The console signs people in with an OAuth authorization-code + PKCE flow
//! against the Savvagent platform (`of-web`'s `/auth/*`). What that leaves this
//! service holding is a [`Session`]: the user and org the platform vouched for,
//! and the access/refresh token pair it issued, sealed at rest. The browser holds
//! only a random cookie; the database holds only that cookie's SHA-256
//! ([`hash_cookie`]), so neither a dump nor a log line can be replayed as a
//! login.
//!
//! This module is SQL only: it knows nothing about HTTP or about the platform's
//! token endpoint. The refresh protocol lives in `of-web`, which uses [`lock`]
//! to make it single-flight (the platform rotates a refresh token on every use
//! and revokes the whole family if a consumed one is presented again, so two
//! concurrent refreshes would sign the user out).
//!
//! # Tenancy
//!
//! `console_sessions` is a tenant table whose policy is the two-branch kind
//! `usage_outbox` has: a request arrives with a cookie and no org, and the
//! cookie is how the org is learned, so [`find`] is necessarily unpinned. Everything
//! that acts on a session already known to belong to an org ([`lock`],
//! [`delete`], [`delete_for_user`], and the lifecycle webhooks) runs pinned and
//! names the org in its predicate as well.

use chrono::{DateTime, Utc};
use otto_tenant::crypto::Cipher;
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::{Db, Tx};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::trackers::{decode_stored_secret, encode_sealed};

/// A session whose access token expires within this long is refreshed before
/// it is used, so the token is still good for the request that triggered it.
pub const REFRESH_SKEW_SECS: i64 = 60;

/// How often an authenticated request is allowed to write `last_used_at`.
const TOUCH_INTERVAL_SECS: i64 = 300;

const COLS: &str = "id_hash, user_id, org_id, access_token_enc, refresh_token_enc, \
                    access_expires_at, scopes, created_at, last_used_at, expires_at";

/// The database key for a cookie value.
pub fn hash_cookie(cookie: &str) -> [u8; 32] {
    Sha256::digest(cookie.as_bytes()).into()
}

/// A stored session. The tokens are still sealed; open them with
/// [`Session::access_token`] / [`Session::refresh_token`].
#[derive(Clone, sqlx::FromRow)]
pub struct Session {
    pub id_hash: Vec<u8>,
    pub user_id: UserId,
    pub org_id: OrgId,
    access_token_enc: String,
    refresh_token_enc: String,
    pub access_expires_at: DateTime<Utc>,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

// Not derived: the sealed tokens stay out of log lines.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("org_id", &self.org_id)
            .field("access_expires_at", &self.access_expires_at)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub fn access_token(&self, cipher: &Cipher) -> Result<String> {
        open(cipher, &self.access_token_enc)
    }

    pub fn refresh_token(&self, cipher: &Cipher) -> Result<String> {
        open(cipher, &self.refresh_token_enc)
    }

    /// Whether the access token is expired or about to be.
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        self.access_expires_at - chrono::Duration::seconds(REFRESH_SKEW_SECS) <= now
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }
}

fn open(cipher: &Cipher, stored: &str) -> Result<String> {
    let sealed = decode_stored_secret(stored)?;
    let plain = cipher.open(&sealed.ciphertext, &sealed.nonce)?;
    String::from_utf8(plain).map_err(|_| {
        otto_tenant::Error::Crypto("a stored session token is not valid UTF-8".into()).into()
    })
}

fn seal(cipher: &Cipher, plain: &str) -> Result<String> {
    encode_sealed(&cipher.seal(plain.as_bytes())?)
}

/// A new session's contents. The tokens are plaintext here and sealed on insert.
pub struct NewSession<'a> {
    pub id_hash: [u8; 32],
    pub user_id: UserId,
    pub org_id: OrgId,
    pub access_token: &'a str,
    pub refresh_token: &'a str,
    pub access_expires_at: DateTime<Utc>,
    pub scopes: &'a [String],
    pub expires_at: DateTime<Utc>,
}

/// Refused with `AccessRevoked` if the org was deleted or the user removed: a
/// sign-in that completes while the platform's clean-up runs must not leave a
/// session behind it (see `platform_events::begin_live`).
pub async fn create(db: &Db, cipher: &Cipher, new: &NewSession<'_>) -> Result<()> {
    let access = seal(cipher, new.access_token)?;
    let refresh = seal(cipher, new.refresh_token)?;
    let mut tx = crate::platform_events::begin_live(db, new.org_id, Some(new.user_id)).await?;
    sqlx::query(
        "INSERT INTO console_sessions \
           (id_hash, user_id, org_id, access_token_enc, refresh_token_enc, \
            access_expires_at, scopes, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&new.id_hash[..])
    .bind(new.user_id)
    .bind(new.org_id)
    .bind(access)
    .bind(refresh)
    .bind(new.access_expires_at)
    .bind(new.scopes)
    .bind(new.expires_at)
    .execute(tx.conn())
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Find a session by its cookie's hash. **Unpinned**: the org is what this
/// lookup exists to learn. An expired row is not returned (and is purged by
/// [`purge_expired`] in due course).
///
/// Slides `last_used_at` at most every few minutes, so an authenticated request
/// is not a write.
pub async fn find(db: &Db, id_hash: &[u8; 32]) -> Result<Option<Session>> {
    let session: Option<Session> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM console_sessions WHERE id_hash = $1 AND expires_at > now()"
    ))
    .bind(&id_hash[..])
    .fetch_optional(db.pool())
    .await?;

    if let Some(s) = &session {
        if Utc::now() - s.last_used_at > chrono::Duration::seconds(TOUCH_INTERVAL_SECS) {
            // Bookkeeping only: a failure to record it must not fail the request.
            if let Err(e) = sqlx::query(
                "UPDATE console_sessions SET last_used_at = now() \
                 WHERE id_hash = $1 AND org_id = $2",
            )
            .bind(&id_hash[..])
            .bind(s.org_id)
            .execute(db.pool())
            .await
            {
                tracing::warn!(error = %e, "could not record a console session's last use");
            }
        }
    }
    Ok(session)
}

/// Lock one session's row for the length of the returned transaction.
///
/// This is the single-flight guard for token refresh: a concurrent caller blocks
/// here until the holder commits, then reads the rotated tokens and finds
/// nothing to refresh. `None` means the session is gone (logged out, purged, or
/// its member removed) since the caller found it.
pub async fn lock(
    db: &Db,
    org: OrgId,
    id_hash: &[u8; 32],
) -> Result<Option<(Tx<'static>, Session)>> {
    let mut tx = db.begin(org).await?;
    let session: Option<Session> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM console_sessions \
         WHERE org_id = $1 AND id_hash = $2 AND expires_at > now() FOR UPDATE"
    ))
    .bind(org)
    .bind(&id_hash[..])
    .fetch_optional(tx.conn())
    .await?;
    Ok(session.map(|s| (tx, s)))
}

/// Store a rotated token pair on a session locked with [`lock`].
pub async fn rotate(
    tx: &mut Tx<'_>,
    cipher: &Cipher,
    id_hash: &[u8; 32],
    access_token: &str,
    refresh_token: &str,
    access_expires_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    let org = tx.org();
    sqlx::query(
        "UPDATE console_sessions \
         SET access_token_enc = $3, refresh_token_enc = $4, access_expires_at = $5, \
             expires_at = $6, last_used_at = now() \
         WHERE org_id = $1 AND id_hash = $2",
    )
    .bind(org)
    .bind(&id_hash[..])
    .bind(seal(cipher, access_token)?)
    .bind(seal(cipher, refresh_token)?)
    .bind(access_expires_at)
    .bind(expires_at)
    .execute(tx.conn())
    .await?;
    Ok(())
}

/// Delete a session inside a transaction that already holds it (an
/// `invalid_grant` on refresh).
pub async fn delete_locked(tx: &mut Tx<'_>, id_hash: &[u8; 32]) -> Result<()> {
    let org = tx.org();
    sqlx::query("DELETE FROM console_sessions WHERE org_id = $1 AND id_hash = $2")
        .bind(org)
        .bind(&id_hash[..])
        .execute(tx.conn())
        .await?;
    Ok(())
}

/// Delete one session. Idempotent.
pub async fn delete(db: &Db, org: OrgId, id_hash: &[u8; 32]) -> Result<u64> {
    let mut tx = db.begin(org).await?;
    let n = sqlx::query("DELETE FROM console_sessions WHERE org_id = $1 AND id_hash = $2")
        .bind(org)
        .bind(&id_hash[..])
        .execute(tx.conn())
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(n)
}

/// Delete every session `user` holds in `org`, inside the caller's transaction
/// (the `member.removed` clean-up).
pub async fn delete_for_user(tx: &mut Tx<'_>, user: UserId) -> Result<u64> {
    let org = tx.org();
    Ok(
        sqlx::query("DELETE FROM console_sessions WHERE org_id = $1 AND user_id = $2")
            .bind(org)
            .bind(user)
            .execute(tx.conn())
            .await?
            .rows_affected(),
    )
}

/// Delete rows past their deadline. Not pinned: it spans orgs, which only
/// background code may do. Safe at any time.
pub async fn purge_expired(db: &Db) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM console_sessions WHERE expires_at <= now()")
            .execute(db.pool())
            .await?
            .rows_affected(),
    )
}
