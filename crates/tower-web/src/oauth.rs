//! The platform's OAuth token endpoints, as the console's login uses them.
//!
//! Three calls, all server to server (the platform serves no CORS, and the
//! console is a public client, so there is no secret to authenticate with —
//! `client_id` is the whole identity, and PKCE is what binds a code to the
//! browser that asked for it):
//!
//! - `authorization_code` + `code_verifier` → a token pair;
//! - `refresh_token` → a **rotated** pair. The platform consumes the refresh
//!   token it is given, and presenting a consumed one revokes the entire token
//!   family. So a refresh must happen at most once per token; see
//!   [`crate::console`] for how that is guaranteed.
//! - revoke, best effort, at logout.
//!
//! [`TokenError`] separates the three outcomes a caller acts on differently: the
//! grant is dead ([`TokenError::InvalidGrant`]: drop the session, the user signs
//! in again), the platform refused for some other reason
//! ([`TokenError::Rejected`]: a deployment fault, not the user's), and the
//! platform could not be reached ([`TokenError::Unavailable`]: try again, change
//! nothing).

use std::time::Duration;

use serde::Deserialize;

/// What the platform issues for an authorization code or a refresh token.
#[derive(Clone, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds until `access_token` expires.
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Space-separated, and what was actually granted (which can be less than
    /// what was asked for).
    #[serde(default)]
    pub scope: Option<String>,
}

// Not derived: neither token may reach a log line.
impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The code or refresh token is spent, expired, revoked, or was never valid.
    #[error("the platform reports the grant is no longer valid")]
    InvalidGrant,
    /// Any other refusal (`invalid_client`, `invalid_target`, …). Not the
    /// user's doing: the client registration or this configuration is wrong.
    #[error("the platform refused the token request: {0}")]
    Rejected(String),
    /// No usable answer: transport failure, timeout, 5xx, or a body that is not
    /// a token response.
    #[error("the platform could not answer the token request: {0}")]
    Unavailable(String),
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: String,
}

pub struct PlatformOAuth {
    http: reqwest::Client,
    token_url: String,
    revoke_url: String,
}

impl std::fmt::Debug for PlatformOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformOAuth")
            .field("token_url", &self.token_url)
            .finish_non_exhaustive()
    }
}

impl PlatformOAuth {
    pub fn new(platform_url: &str) -> Self {
        let base = platform_url.trim_end_matches('/');
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                // The token endpoint never redirects. Following one would send a
                // code or a refresh token wherever it pointed.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("the TLS backend initialises"),
            token_url: format!("{base}/oauth/token"),
            revoke_url: format!("{base}/oauth/revoke"),
        }
    }

    pub async fn exchange_code(
        &self,
        client_id: &str,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
        resource: &str,
    ) -> Result<TokenSet, TokenError> {
        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
            ("client_id", client_id),
            ("resource", resource),
        ])
        .await
    }

    pub async fn refresh(
        &self,
        client_id: &str,
        refresh_token: &str,
        resource: &str,
    ) -> Result<TokenSet, TokenError> {
        self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("resource", resource),
        ])
        .await
    }

    /// Revoke a refresh token. The platform always answers `200`, so the only
    /// failure worth reporting is not reaching it, which the caller logs and
    /// moves past: a logout must not depend on the platform being up.
    pub async fn revoke(&self, client_id: &str, token: &str) -> Result<(), TokenError> {
        self.http
            .post(&self.revoke_url)
            .form(&[("token", token), ("client_id", client_id)])
            .send()
            .await
            .map_err(|e| TokenError::Unavailable(e.to_string()))?;
        Ok(())
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenSet, TokenError> {
        let response = self
            .http
            .post(&self.token_url)
            .form(form)
            .send()
            .await
            .map_err(|e| TokenError::Unavailable(e.to_string()))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| TokenError::Unavailable(e.to_string()))?;

        if status.is_success() {
            return serde_json::from_slice(&body)
                .map_err(|e| TokenError::Unavailable(format!("unreadable token response: {e}")));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(TokenError::Unavailable(format!("HTTP {status}")));
        }
        let error = serde_json::from_slice::<ErrorBody>(&body)
            .map(|b| b.error)
            .unwrap_or_default();
        match error.as_str() {
            "invalid_grant" => Err(TokenError::InvalidGrant),
            "" => Err(TokenError::Rejected(format!("HTTP {status}"))),
            other => Err(TokenError::Rejected(other.to_string())),
        }
    }
}
