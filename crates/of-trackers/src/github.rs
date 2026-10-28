use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

const GITHUB_API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = concat!("otto-tower/", env!("CARGO_PKG_VERSION"));
const JWT_IAT_SKEW_SECONDS: i64 = 30;
const INSTALLATION_TOKEN_REFRESH_SKEW_SECONDS: i64 = 30;
const MAX_ERROR_BODY_BYTES: usize = 256;
/// The outbound sync path (Task 4) calls into this client synchronously,
/// after the job's own transaction has already committed, so a stalled
/// GitHub API would otherwise hold the MCP tool call open indefinitely —
/// reqwest sets no timeout by default. A bounded timeout turns "GitHub is
/// down" into a logged, best-effort failure instead of a hung agent call.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone, PartialEq, Eq)]
struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: i64,
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for InstallationTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallationTokenResponse")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Deserialize)]
struct IssueRecord {
    #[serde(default)]
    updated_at: Option<String>,
}

/// What `sync_ticket` reads from an issue: its body (for "Blocked by" lines)
/// and when it last changed (for the drift mark). Both are `None` when GitHub
/// sends them as null or not at all; an issue with no body is an ordinary one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueView {
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

pub struct GithubAppClient {
    app_id: i64,
    key: EncodingKey,
    http: reqwest::Client,
    api_base: String,
    installation_tokens: Mutex<HashMap<i64, CachedToken>>,
}

impl GithubAppClient {
    pub fn new(app_id: i64, private_key_pem: String) -> Result<Self> {
        let key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .map_err(Error::InvalidGithubPrivateKey)?;
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|source| Error::Http {
                provider: "GitHub",
                action: "building the HTTP client",
                source,
            })?;

        Ok(Self {
            app_id,
            key,
            http,
            api_base: "https://api.github.com".into(),
            installation_tokens: Mutex::new(HashMap::new()),
        })
    }

    async fn installation_token(&self, installation_id: i64) -> Result<String> {
        if let Some(token) = self.cached_installation_token(installation_id)? {
            return Ok(token);
        }

        let minted = self.exchange_installation_token(installation_id).await?;
        let token = minted.token.clone();
        self.installation_tokens
            .lock()
            .map_err(|_| {
                Error::Internal(
                    "GitHub installation-token cache was poisoned; construct a new GithubAppClient"
                        .into(),
                )
            })?
            .insert(
                installation_id,
                CachedToken {
                    token: minted.token,
                    expires_at: minted.expires_at,
                },
            );
        Ok(token)
    }

    pub async fn post_comment(
        &self,
        installation_id: i64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        body: &str,
    ) -> Result<()> {
        let token = self.installation_token(installation_id).await?;
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{issue_number}/comments",
            self.api_base
        );
        self.send_without_response(
            self.github_request(&token, reqwest::Method::POST, &url)
                .json(&serde_json::json!({ "body": body })),
            "posting an issue comment",
        )
        .await
    }

    pub async fn get_issue_updated_at(
        &self,
        installation_id: i64,
        owner: &str,
        repo: &str,
        issue_number: i64,
    ) -> Result<Option<String>> {
        let token = self.installation_token(installation_id).await?;
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{issue_number}",
            self.api_base
        );
        let issue: IssueRecord = self
            .send_json(
                self.github_request(&token, reqwest::Method::GET, &url),
                "fetching an issue revision",
            )
            .await?;
        Ok(issue.updated_at)
    }

    /// `GET /repos/{owner}/{repo}/issues/{n}`: the same request
    /// [`Self::get_issue_updated_at`] makes, keeping the body as well.
    pub async fn get_issue(
        &self,
        installation_id: i64,
        owner: &str,
        repo: &str,
        issue_number: i64,
    ) -> Result<IssueView> {
        let token = self.installation_token(installation_id).await?;
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{issue_number}",
            self.api_base
        );
        self.send_json(
            self.github_request(&token, reqwest::Method::GET, &url),
            "fetching an issue",
        )
        .await
    }

    pub async fn list_labels(
        &self,
        installation_id: i64,
        owner: &str,
        repo: &str,
        issue_number: i64,
    ) -> Result<Vec<String>> {
        let token = self.installation_token(installation_id).await?;
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{issue_number}/labels",
            self.api_base
        );
        let labels: Vec<Label> = self
            .send_json(
                self.github_request(&token, reqwest::Method::GET, &url),
                "listing issue labels",
            )
            .await?;
        Ok(labels.into_iter().map(|label| label.name).collect())
    }

    pub async fn set_issue_state(
        &self,
        installation_id: i64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        state: &str,
        state_reason: Option<&str>,
    ) -> Result<Option<String>> {
        if !matches!(state, "open" | "closed") {
            return Err(Error::InvalidGithubIssueState(state.to_string()));
        }

        let token = self.installation_token(installation_id).await?;
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{issue_number}",
            self.api_base
        );
        let issue: IssueRecord = self
            .send_json(
                self.github_request(&token, reqwest::Method::PATCH, &url)
                    .json(&serde_json::json!({
                        "state": state,
                        "state_reason": state_reason,
                    })),
                "setting an issue state",
            )
            .await?;
        Ok(issue.updated_at)
    }

    fn cached_installation_token(&self, installation_id: i64) -> Result<Option<String>> {
        let now = Utc::now() + Duration::seconds(INSTALLATION_TOKEN_REFRESH_SKEW_SECONDS);
        let cached = self
            .installation_tokens
            .lock()
            .map_err(|_| {
                Error::Internal(
                    "GitHub installation-token cache was poisoned; construct a new GithubAppClient"
                        .into(),
                )
            })?
            .get(&installation_id)
            .filter(|cached| cached.expires_at > now)
            .map(|cached| cached.token.clone());
        Ok(cached)
    }

    async fn exchange_installation_token(
        &self,
        installation_id: i64,
    ) -> Result<InstallationTokenResponse> {
        let app_jwt = self.mint_app_jwt()?;
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api_base
        );
        self.send_json(
            self.github_request(&app_jwt, reqwest::Method::POST, &url),
            "minting an installation access token",
        )
        .await
    }

    fn mint_app_jwt(&self) -> Result<String> {
        let now = Utc::now();
        let claims = AppClaims {
            iat: (now - Duration::seconds(JWT_IAT_SKEW_SECONDS)).timestamp(),
            exp: (now + Duration::minutes(10)).timestamp(),
            iss: self.app_id,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key).map_err(|_| {
            Error::Internal(
                "GitHub App JWT signing failed; check the configured App id and private key".into(),
            )
        })
    }

    fn github_request(
        &self,
        token: &str,
        method: reqwest::Method,
        url: &str,
    ) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
    }

    async fn send_without_response(
        &self,
        request: reqwest::RequestBuilder,
        action: &'static str,
    ) -> Result<()> {
        let response = request.send().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "GitHub",
                action,
                status,
                body: sanitize_error_body(&body),
            });
        }
        Ok(())
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        request: reqwest::RequestBuilder,
        action: &'static str,
    ) -> Result<T> {
        let response = request.send().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "GitHub",
                action,
                status,
                body: sanitize_error_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(|error| Error::InvalidResponse {
            provider: "GitHub",
            action,
            message: format!("{error}; body was {}", sanitize_error_body(&body)),
        })
    }

    /// Send every call to `api_base` instead of `https://api.github.com`.
    /// It exists for tests pointing the client at a mock GitHub; production
    /// code never calls it, and no configuration sets it.
    pub fn with_api_base(mut self, api_base: String) -> Self {
        self.api_base = api_base;
        self
    }
}

/// The App's *user*-to-server OAuth half.
///
/// Separate from [`GithubAppClient`] because it shares none of its machinery:
/// no App JWT, no installation-token cache, and a different host. What it
/// shares is a purpose — this is the only thing that can answer "does the
/// human who just clicked through GitHub's install screen actually administer
/// the installation they came back claiming?", and the tracker console refuses
/// to write a connection row without that answer.
///
/// The alternative, `GET /app/installations/{id}` with the App JWT, proves the
/// installation exists. That is precisely what an attacker enumerating small
/// integers already assumes, so it proves nothing worth having.
pub struct GithubUserAuth {
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    /// `https://github.com` — where codes are redeemed. Not the API host.
    oauth_base: String,
    /// `https://api.github.com` — where the resulting token is spent.
    api_base: String,
}

#[derive(Deserialize)]
struct UserTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct UserInstallationsPage {
    #[serde(default)]
    installations: Vec<InstallationRecord>,
}

#[derive(Deserialize)]
struct InstallationRecord {
    id: i64,
}

impl GithubUserAuth {
    pub fn new(client_id: String, client_secret: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|source| Error::Http {
                provider: "GitHub",
                action: "building the HTTP client",
                source,
            })?;

        Ok(Self {
            client_id,
            client_secret,
            http,
            oauth_base: "https://github.com".into(),
            api_base: "https://api.github.com".into(),
        })
    }

    /// Redeem `code` and confirm it speaks for someone who administers
    /// `installation_id`.
    ///
    /// Returns `Ok(())` and nothing else on purpose: the user token is spent
    /// here and deliberately not handed back. Nothing downstream should hold a
    /// credential that speaks for a human — every later call is made by the
    /// App on the installation's behalf, which is the identity the audit trail
    /// and GitHub's own permissions are written in terms of.
    pub async fn verify_installation_access(&self, code: &str, installation_id: i64) -> Result<()> {
        let token = self.exchange_user_code(code).await?;
        if self.administers(&token, installation_id).await? {
            Ok(())
        } else {
            Err(Error::GithubInstallationNotAdministered { installation_id })
        }
    }

    async fn exchange_user_code(&self, code: &str) -> Result<String> {
        let action = "redeeming a user authorization code";
        let response: UserTokenResponse = self
            .send_json(
                self.http
                    .post(format!("{}/login/oauth/access_token", self.oauth_base))
                    // Without this GitHub answers form-encoded, and the body
                    // parses as an error rather than as a token.
                    .header(reqwest::header::ACCEPT, "application/json")
                    .json(&serde_json::json!({
                        "client_id": self.client_id,
                        "client_secret": self.client_secret,
                        "code": code,
                    })),
                action,
            )
            .await?;

        if let Some(token) = response.access_token {
            return Ok(token);
        }

        // A 200 with no token is the spent/forged-code case; say which, in
        // GitHub's own words, because the admin's next step depends on it.
        // Both halves are kept where GitHub sends both: the description is
        // what the admin reads, the code is what an operator greps for.
        Err(Error::GithubUserCodeRejected(
            match (response.error_description, response.error) {
                (Some(description), Some(code)) => format!("{description} [{code}]"),
                (Some(only), None) | (None, Some(only)) => only,
                (None, None) => "no access token in GitHub's response".into(),
            },
        ))
    }

    /// Whether the account behind `token` administers `installation_id`.
    ///
    /// Pages are followed to the end rather than trusting the first one. An
    /// account with more installations than fit on a page would otherwise have
    /// the tail of its list dropped, and a dropped installation reads exactly
    /// like one the admin does not administer — the wrong refusal, aimed at
    /// somebody legitimate.
    async fn administers(&self, token: &str, installation_id: i64) -> Result<bool> {
        let action = "listing the installations a user administers";
        let mut url = Some(format!("{}/user/installations?per_page=100", self.api_base));

        while let Some(next) = url {
            let response = self
                .http
                .get(&next)
                .bearer_auth(token)
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
                .send()
                .await
                .map_err(|source| Error::Http {
                    provider: "GitHub",
                    action,
                    source,
                })?;

            let status = response.status();
            let link = response
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let body = response.text().await.map_err(|source| Error::Http {
                provider: "GitHub",
                action,
                source,
            })?;

            if !status.is_success() {
                return Err(Error::Api {
                    provider: "GitHub",
                    action,
                    status,
                    body: sanitize_error_body(&body),
                });
            }

            let page: UserInstallationsPage =
                serde_json::from_str(&body).map_err(|error| Error::InvalidResponse {
                    provider: "GitHub",
                    action,
                    message: format!("{error}; body was {}", sanitize_error_body(&body)),
                })?;

            if page.installations.iter().any(|i| i.id == installation_id) {
                return Ok(true);
            }

            url = link.as_deref().and_then(next_page_url);
        }

        Ok(false)
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        request: reqwest::RequestBuilder,
        action: &'static str,
    ) -> Result<T> {
        let response = request.send().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "GitHub",
                action,
                status,
                body: sanitize_error_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(|error| Error::InvalidResponse {
            provider: "GitHub",
            action,
            message: format!("{error}; body was {}", sanitize_error_body(&body)),
        })
    }

    #[cfg(test)]
    fn with_bases(mut self, oauth_base: String, api_base: String) -> Self {
        self.oauth_base = oauth_base;
        self.api_base = api_base;
        self
    }
}

/// The `next` URL out of an RFC 8288 `Link` header, if there is one.
///
/// Deliberately narrow: it reads only the URL GitHub itself just sent, and
/// only from the relation named `next`. Building the next page's URL locally
/// from a page counter would mean re-deriving pagination GitHub already
/// described, and drifting from it the day it changes.
fn next_page_url(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.split(';')
            .any(|attr| attr.trim().replace(['"', '\''], "") == "rel=next")
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
    })
}

fn sanitize_error_body(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(mut value) => {
            redact_secret_fields(&mut value);
            truncate(&value.to_string(), MAX_ERROR_BODY_BYTES)
        }
        Err(_) => truncate(body, MAX_ERROR_BODY_BYTES),
    }
}

fn redact_secret_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if matches!(
                    key.as_str(),
                    "token" | "access_token" | "refresh_token" | "client_secret"
                ) {
                    *value = Value::String("<redacted>".into());
                } else {
                    redact_secret_fields(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_secret_fields(item);
            }
        }
        _ => {}
    }
}

fn truncate(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }

    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &input[..end])
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde::Deserialize;

    use super::*;
    use crate::test_support::{MockResponse, TestServer};

    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC3XlFCGcip1ief\nEPPBKThHItEglgMYCn//hcJ1kNWnClwkYmvy3pQ49Vyi+h0GZ7FQwB7K/19e28lY\n9rF/Wp0t9WA1gLh1A6MU6wUJ0fHllWfFy9+9K1CgnLZvO+x77JRODhfb7nxYUjjl\nTBYokVvw+5njbWZnxcrSR88+PprGJt7a3sxeLQ7N/TaYlyqqputMO4BtCp7dpDxg\nesElXSkCEG4DWj2IiH2LGfDxeKaxE1JetkUafCV7uMmfQ5v74sNFY5BfZNNJBfnA\nKBzK7oOF19THBxmtVWo/tYE5aBuADAUWZKOduvbA08lr3LsldJqdo5d8HJudrlte\ndl51wCcfAgMBAAECggEAUwcDXx1CpVghH56y6FsMLvWeYJVcOEYE2APOXaJjg1un\nBhiEjXdoAPRkai06+Dv6ZzhemQcRvWdiX4RwMVyrv/QTiJZMrzsi3CVgZiZoU86X\nKtIZ8FNNEjRzTKGC/kfMjR1Hg1+UcP9l4LlXbS4IRfD+qKJQFJvULuux9JqvRRnu\nq7QkKfqX8CbMAYpxSa74pBTip1V73smUqM1rP7gt0GAOd0LG19cQqPDMeWn2h14F\nDOd/48z/1gTPCCywL3AJ9Fz4rfEJi74cSqVEinJR2yduUJzXgZ/PRhKOvKicIstL\nayRSwwC2FKuM4WtOECpl8QwPn/ZV7ATxQOhcO/W55QKBgQDsRen/Ckrju3dC9zlf\nZ4+cCBFMQMjR6/gTox+4obxsZ0WynLbc5MqHP565VfUmOsl2X3aiLz/0kiiRdvQf\nLcorsA6ujYXEyRSNTAeHfzWgtaDw9dKEtLSSNjB6XJuSsYhWHvpzC2mManeWi3/0\nKFbZU+EQgDcWlLnY8W9ns6fDlQKBgQDGrZ9OU0BhIYTm8zyHrpAsrRXeOr2PkdYX\nP+ObF96UIYP8/AZBFM60/A2wvIeNei3IoVgNanLpGOcE0rHkUY5Bfd3wGAT1jXXK\nbc66rcOkI41tTebvJQClmupHSnhfLaGaqdxio7Syn5sj8qDbXsB9AaN/wAWLOd76\nhdMIZ/hS4wKBgQCsK1YT7uAbiqOhPJ2mE8TmIkrYkezEa3redGPNGq4/IBH90Yy+\n8klSvN1gmG6HaRcdFvtPu7aS9V5ygYfqoGdN5oEMWTw85XoAbIKgDeZ6MWARtk+t\nPDDIyowQ3iLPhmaeuvwtkQdctshl/0lCFZMT0reSWpvJ7J5wo55Wpud88QKBgHCv\neyKen24357xiC1vdi5J7XWLdKDTs/2PSbdLCmBCmbckoXJe/KHqIV299jtiUirE3\nqcx6KtDAug8HPbSE+U12CVIrHWz0nfGBlHZXJhbLv2RWgfvznclP8z8aIunA5N7n\nJsOfnFaPphueetPRixWbv1Mu4zYTTcAD9SzYY4UHAoGAcW+OQP6DFWhq7Yzi6Qn3\n7B1yXbvnlYP/Tj9/bXP25DxZcT8QSNfasbfq4afePduTEjXcGWvM7wTllyMif2/X\nx3+QNzDNxUxmRAIOZqSO+XBfq8Yf7unrUX92BItKKMc/8d+/7R5vmj9GdRmS746W\nWWOZZLPetCHggLA6DEPj4QM=\n-----END PRIVATE KEY-----\n";

    #[derive(Deserialize)]
    struct Claims {
        iat: i64,
        exp: i64,
        iss: i64,
    }

    #[test]
    fn app_jwt_carries_expected_claims() {
        let client = GithubAppClient::new(42, TEST_KEY.into()).expect("client");
        let jwt = client.mint_app_jwt().expect("jwt");
        let payload = jwt.split('.').nth(1).expect("payload");
        let decoded = URL_SAFE_NO_PAD.decode(payload).expect("decode payload");
        let claims: Claims = serde_json::from_slice(&decoded).expect("claims");

        assert_eq!(claims.iss, 42);
        assert!(claims.exp > claims.iat);
        assert!(claims.exp - claims.iat >= 600);
    }

    #[tokio::test]
    async fn installation_token_is_reused_while_valid() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(201, serde_json::json!({ "id": 1 })));
        server.push(MockResponse::json(
            200,
            serde_json::json!([{ "name": "bug" }]),
        ));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        client
            .post_comment(17, "octo", "repo", 99, "synced")
            .await
            .expect("comment succeeds");
        let labels = client
            .list_labels(17, "octo", "repo", 99)
            .await
            .expect("labels succeed");

        assert_eq!(labels, vec!["bug".to_string()]);
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/app/installations/17/access_tokens");
        assert_eq!(requests[0].headers["accept"], "application/vnd.github+json");
        assert_eq!(
            requests[0].headers["x-github-api-version"],
            GITHUB_API_VERSION
        );
        assert_eq!(
            requests[1].headers["authorization"],
            "Bearer installation-token-1"
        );
        assert_eq!(
            requests[2].headers["authorization"],
            "Bearer installation-token-1"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn expired_installation_token_is_reminted() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2000-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(201, serde_json::json!({ "id": 1 })));
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-2",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(
            200,
            serde_json::json!([{ "name": "triaged" }]),
        ));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        client
            .post_comment(17, "octo", "repo", 99, "synced")
            .await
            .expect("comment succeeds");
        client
            .list_labels(17, "octo", "repo", 99)
            .await
            .expect("labels succeed");

        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].path, "/app/installations/17/access_tokens");
        assert_eq!(
            requests[1].headers["authorization"],
            "Bearer installation-token-1"
        );
        assert_eq!(requests[2].path, "/app/installations/17/access_tokens");
        assert_eq!(
            requests[3].headers["authorization"],
            "Bearer installation-token-2"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn github_calls_send_expected_method_path_and_body() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(201, serde_json::json!({ "ok": true })));
        server.push(MockResponse::json(
            200,
            serde_json::json!([{ "name": "bug" }, { "name": "help wanted" }]),
        ));
        server.push(MockResponse::json(
            200,
            serde_json::json!({ "updated_at": "2026-09-03T18:12:00Z" }),
        ));
        server.push(MockResponse::json(
            200,
            serde_json::json!({ "updated_at": "2026-09-03T18:13:00Z" }),
        ));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        client
            .post_comment(17, "octo", "repo", 7, "hello world")
            .await
            .expect("comment succeeds");
        let labels = client
            .list_labels(17, "octo", "repo", 7)
            .await
            .expect("labels succeed");
        let closed_at = client
            .set_issue_state(17, "octo", "repo", 7, "closed", Some("completed"))
            .await
            .expect("state succeeds");
        let updated_at = client
            .get_issue_updated_at(17, "octo", "repo", 7)
            .await
            .expect("issue fetch succeeds");

        assert_eq!(labels, vec!["bug", "help wanted"]);
        assert_eq!(closed_at.as_deref(), Some("2026-09-03T18:12:00Z"));
        assert_eq!(updated_at.as_deref(), Some("2026-09-03T18:13:00Z"));
        let requests = server.requests();
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/repos/octo/repo/issues/7/comments");
        assert_eq!(requests[1].headers["accept"], "application/vnd.github+json");
        assert_eq!(
            requests[1].headers["x-github-api-version"],
            GITHUB_API_VERSION
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[1].body).expect("comment body"),
            serde_json::json!({ "body": "hello world" })
        );
        assert_eq!(requests[2].method, "GET");
        assert_eq!(requests[2].path, "/repos/octo/repo/issues/7/labels");
        assert_eq!(requests[3].method, "PATCH");
        assert_eq!(requests[3].path, "/repos/octo/repo/issues/7");
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[3].body).expect("patch body"),
            serde_json::json!({ "state": "closed", "state_reason": "completed" })
        );
        assert_eq!(requests[4].method, "GET");
        assert_eq!(requests[4].path, "/repos/octo/repo/issues/7");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn get_issue_reads_the_body_and_the_revision() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "body": "Blocked by #14",
                "updated_at": "2026-09-03T18:13:00Z"
            }),
        ));
        server.push(MockResponse::json(200, serde_json::json!({ "body": null })));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        let issue = client
            .get_issue(17, "octo", "repo", 7)
            .await
            .expect("issue fetch succeeds");
        assert_eq!(issue.body.as_deref(), Some("Blocked by #14"));
        assert_eq!(issue.updated_at.as_deref(), Some("2026-09-03T18:13:00Z"));

        let bare = client
            .get_issue(17, "octo", "repo", 8)
            .await
            .expect("issue fetch succeeds");
        assert_eq!(
            bare,
            IssueView {
                body: None,
                updated_at: None
            }
        );

        let requests = server.requests();
        assert_eq!(requests[1].method, "GET");
        assert_eq!(requests[1].path, "/repos/octo/repo/issues/7");
        assert_eq!(
            requests[1].headers["authorization"],
            "Bearer installation-token-1"
        );
        assert_eq!(requests[2].path, "/repos/octo/repo/issues/8");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn non_success_github_status_is_reported_with_status_and_body() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(
            422,
            serde_json::json!({ "message": "bad issue state", "token": "should-not-leak" }),
        ));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        let error = client
            .set_issue_state(17, "octo", "repo", 7, "closed", Some("completed"))
            .await
            .expect_err("request should fail");

        match error {
            Error::Api {
                provider,
                action,
                status,
                body,
            } => {
                assert_eq!(provider, "GitHub");
                assert_eq!(action, "setting an issue state");
                assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
                assert!(body.contains("bad issue state"));
                assert!(!body.contains("should-not-leak"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        server.shutdown().await;
    }

    #[tokio::test]
    async fn verifying_an_installation_accepts_one_the_user_administers() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!({ "access_token": "user-token", "token_type": "bearer" }),
        ));
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "total_count": 2,
                "installations": [{ "id": 11 }, { "id": 17 }]
            }),
        ));

        let auth = GithubUserAuth::new("client".into(), "secret".into())
            .expect("auth")
            .with_bases(server.base_url.clone(), server.base_url.clone());

        auth.verify_installation_access("the-code", 17)
            .await
            .expect("installation 17 is administered by this user");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/login/oauth/access_token");
        // The secret goes in the body, never the query string: a URL is logged
        // by every proxy between here and GitHub.
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("exchange body is json");
        assert_eq!(body["client_secret"], "secret");
        assert_eq!(body["code"], "the-code");
        assert_eq!(requests[1].method, "GET");
        assert!(requests[1].path.starts_with("/user/installations"));
        assert_eq!(requests[1].headers["authorization"], "Bearer user-token");
    }

    /// The whole reason this exchange exists. An installation id is a small
    /// integer; without this check any org admin could type one and drive
    /// another customer's issues with the operator's own App credentials.
    #[tokio::test]
    async fn verifying_an_installation_refuses_one_the_user_does_not_administer() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!({ "access_token": "user-token" }),
        ));
        server.push(MockResponse::json(
            200,
            serde_json::json!({ "total_count": 1, "installations": [{ "id": 11 }] }),
        ));

        let auth = GithubUserAuth::new("client".into(), "secret".into())
            .expect("auth")
            .with_bases(server.base_url.clone(), server.base_url.clone());

        let error = auth
            .verify_installation_access("the-code", 17)
            .await
            .expect_err("installation 17 is not in the user's list");

        assert!(
            matches!(
                error,
                Error::GithubInstallationNotAdministered {
                    installation_id: 17
                }
            ),
            "unexpected error: {error}"
        );
    }

    /// GitHub answers a spent or forged code with HTTP 200 and an `error` field
    /// rather than a failing status, so a client that only checked the status
    /// would sail on with no `access_token` and report something misleading.
    #[tokio::test]
    async fn a_rejected_authorization_code_is_reported_as_such() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "error": "bad_verification_code",
                "error_description": "The code passed is incorrect or expired."
            }),
        ));

        let auth = GithubUserAuth::new("client".into(), "secret".into())
            .expect("auth")
            .with_bases(server.base_url.clone(), server.base_url.clone());

        let error = auth
            .verify_installation_access("spent", 17)
            .await
            .expect_err("a spent code cannot verify anything");

        let message = error.to_string();
        assert!(
            message.contains("bad_verification_code"),
            "the admin needs GitHub's own reason: {message}"
        );
        assert!(
            message.contains("again"),
            "the message must say what to do next: {message}"
        );
        assert_eq!(server.requests().len(), 1, "no token, no second call");
    }

    /// An account with more installations than fit on one page must not have
    /// the tail of its list silently dropped — that would read as "you do not
    /// administer this" to someone who does.
    #[tokio::test]
    async fn installation_pages_are_followed_to_the_end() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!({ "access_token": "user-token" }),
        ));
        let mut first = MockResponse::json(
            200,
            serde_json::json!({ "total_count": 2, "installations": [{ "id": 11 }] }),
        );
        first.headers.push((
            "link".into(),
            format!(
                "<{}/user/installations?page=2>; rel=\"next\"",
                server.base_url
            ),
        ));
        server.push(first);
        server.push(MockResponse::json(
            200,
            serde_json::json!({ "total_count": 2, "installations": [{ "id": 17 }] }),
        ));

        let auth = GithubUserAuth::new("client".into(), "secret".into())
            .expect("auth")
            .with_bases(server.base_url.clone(), server.base_url.clone());

        auth.verify_installation_access("the-code", 17)
            .await
            .expect("installation 17 is on the second page");

        assert_eq!(server.requests().len(), 3);
    }
}
