use savvagent_tenant::crypto::{Cipher, Sealed};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

const USER_AGENT: &str = concat!("otto-tower/", env!("CARGO_PKG_VERSION"));
const MAX_ERROR_BODY_BYTES: usize = 256;
/// The outbound sync path (Task 4) calls into this client synchronously,
/// after the job's own transaction has already committed, so a stalled
/// tracker API would otherwise hold the MCP tool call open indefinitely —
/// reqwest sets no timeout by default. A bounded timeout turns "the tracker
/// is down" into a logged, best-effort failure instead of a hung agent call.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// JIRA issue keys are spliced unescaped into REST URL paths below. Unlike
/// the GitHub side (`parse_github_ticket_ref` strictly splits
/// `"owner/repo#123"`), an issue key arriving from a webhook payload is
/// whatever the org's own JIRA Automation rule sends — not a value shaped by
/// Atlassian itself. Refusing anything that isn't `PROJECT-123`-shaped before
/// it reaches a URL keeps a misconfigured (or hostile) automation rule from
/// redirecting an outbound call to an unintended path segment.
///
/// Public so a caller can validate a `ticket_ref` up front — e.g.
/// `tower-mcp`'s `sync_ticket`, which needs to tell a malformed issue key
/// (caller input, never retriable) apart from a genuine JIRA API failure
/// (transient, retriable) before it ever reaches this client.
pub fn validate_jira_issue_key(issue_key: &str) -> Result<()> {
    let valid = issue_key.split_once('-').is_some_and(|(project, number)| {
        !project.is_empty()
            && project
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic())
            && project.chars().all(|c| {
                c.is_ascii_alphanumeric() && (c.is_ascii_uppercase() || c.is_ascii_digit())
            })
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
    });
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidJiraIssueKey(issue_key.to_string()))
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

impl OAuthTokens {
    pub fn seal_refresh_token(&self, cipher: &Cipher) -> Result<Sealed> {
        cipher
            .seal(self.refresh_token.as_bytes())
            .map_err(Error::from)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessibleResource {
    pub id: String,
    pub name: String,
    pub url: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub id: String,
    pub name: String,
    pub to_status: String,
    pub to_status_category: String,
}

#[derive(Deserialize)]
struct TransitionListResponse {
    transitions: Vec<ApiTransition>,
}

#[derive(Deserialize)]
struct ApiTransition {
    id: String,
    name: String,
    to: ApiTransitionTo,
}

#[derive(Deserialize)]
struct ApiTransitionTo {
    name: String,
    #[serde(rename = "statusCategory")]
    status_category: ApiStatusCategory,
}

#[derive(Deserialize)]
struct ApiStatusCategory {
    #[serde(default)]
    key: Option<String>,
    name: String,
}

#[derive(Deserialize)]
struct JiraIssueResponse {
    fields: JiraIssueResponseFields,
}

#[derive(Deserialize)]
struct JiraIssueResponseFields {
    #[serde(default)]
    updated: Option<String>,
}

pub struct JiraClient {
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    auth_base: String,
    api_base: String,
}

impl JiraClient {
    pub fn new(client_id: String, client_secret: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|source| Error::Http {
                provider: "JIRA",
                action: "building the HTTP client",
                source,
            })?;
        Ok(Self {
            client_id,
            client_secret,
            http,
            auth_base: "https://auth.atlassian.com".into(),
            api_base: "https://api.atlassian.com".into(),
        })
    }

    pub async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<OAuthTokens> {
        self.send_json(
            self.http
                .post(format!("{}/oauth/token", self.auth_base))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .json(&serde_json::json!({
                    "grant_type": "authorization_code",
                    "client_id": self.client_id,
                    "client_secret": self.client_secret,
                    "code": code,
                    "redirect_uri": redirect_uri,
                })),
            "exchanging an authorization code",
        )
        .await
    }

    pub async fn refresh_access_token(&self, refresh_token: &str) -> Result<OAuthTokens> {
        self.send_json(
            self.http
                .post(format!("{}/oauth/token", self.auth_base))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .json(&serde_json::json!({
                    "grant_type": "refresh_token",
                    "client_id": self.client_id,
                    "client_secret": self.client_secret,
                    "refresh_token": refresh_token,
                })),
            "refreshing an access token",
        )
        .await
    }

    pub async fn accessible_resources(
        &self,
        access_token: &str,
    ) -> Result<Vec<AccessibleResource>> {
        self.send_json(
            self.http
                .get(format!(
                    "{}/oauth/token/accessible-resources",
                    self.api_base
                ))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .bearer_auth(access_token),
            "listing accessible resources",
        )
        .await
    }

    pub async fn post_comment(
        &self,
        access_token: &str,
        cloud_id: &str,
        issue_key: &str,
        body: &str,
    ) -> Result<()> {
        validate_jira_issue_key(issue_key)?;
        self.send_without_response(
            self.http
                .post(format!(
                    "{}/ex/jira/{cloud_id}/rest/api/3/issue/{issue_key}/comment",
                    self.api_base
                ))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .bearer_auth(access_token)
                .json(&serde_json::json!({
                    "body": {
                        "type": "doc",
                        "version": 1,
                        "content": [{
                            "type": "paragraph",
                            "content": [{
                                "type": "text",
                                "text": body,
                            }]
                        }]
                    }
                })),
            "posting an issue comment",
        )
        .await
    }

    pub async fn list_transitions(
        &self,
        access_token: &str,
        cloud_id: &str,
        issue_key: &str,
    ) -> Result<Vec<Transition>> {
        validate_jira_issue_key(issue_key)?;
        let response: TransitionListResponse = self
            .send_json(
                self.http
                    .get(format!(
                        "{}/ex/jira/{cloud_id}/rest/api/3/issue/{issue_key}/transitions",
                        self.api_base
                    ))
                    .header(reqwest::header::USER_AGENT, USER_AGENT)
                    .bearer_auth(access_token),
                "listing issue transitions",
            )
            .await?;

        Ok(response
            .transitions
            .into_iter()
            .map(|transition| Transition {
                id: transition.id,
                name: transition.name,
                to_status: transition.to.name,
                to_status_category: transition
                    .to
                    .status_category
                    .key
                    .unwrap_or(transition.to.status_category.name),
            })
            .collect())
    }

    pub async fn transition_issue(
        &self,
        access_token: &str,
        cloud_id: &str,
        issue_key: &str,
        transition_id: &str,
    ) -> Result<()> {
        validate_jira_issue_key(issue_key)?;
        self.send_without_response(
            self.http
                .post(format!(
                    "{}/ex/jira/{cloud_id}/rest/api/3/issue/{issue_key}/transitions",
                    self.api_base
                ))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .bearer_auth(access_token)
                .json(&serde_json::json!({
                    "transition": {
                        "id": transition_id,
                    }
                })),
            "transitioning an issue",
        )
        .await
    }

    pub async fn get_issue_updated_at(
        &self,
        access_token: &str,
        cloud_id: &str,
        issue_key: &str,
    ) -> Result<Option<String>> {
        validate_jira_issue_key(issue_key)?;
        let issue: JiraIssueResponse = self
            .send_json(
                self.http
                    .get(format!(
                        "{}/ex/jira/{cloud_id}/rest/api/3/issue/{issue_key}?fields=updated",
                        self.api_base
                    ))
                    .header(reqwest::header::USER_AGENT, USER_AGENT)
                    .bearer_auth(access_token),
                "fetching an issue revision",
            )
            .await?;
        Ok(issue.fields.updated)
    }

    pub fn open_refresh_token(cipher: &Cipher, sealed: &Sealed) -> Result<String> {
        let opened = cipher.open(&sealed.ciphertext, &sealed.nonce)?;
        String::from_utf8(opened).map_err(|_| Error::InvalidJiraRefreshTokenEncoding)
    }

    async fn send_without_response(
        &self,
        request: reqwest::RequestBuilder,
        action: &'static str,
    ) -> Result<()> {
        let response = request.send().await.map_err(|source| Error::Http {
            provider: "JIRA",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "JIRA",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "JIRA",
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
            provider: "JIRA",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "JIRA",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "JIRA",
                action,
                status,
                body: sanitize_error_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(|error| Error::InvalidResponse {
            provider: "JIRA",
            action,
            message: format!("{error}; body was {}", sanitize_error_body(&body)),
        })
    }

    #[cfg(test)]
    fn with_bases(mut self, auth_base: String, api_base: String) -> Self {
        self.auth_base = auth_base;
        self.api_base = api_base;
        self
    }
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
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;

    use super::*;
    use crate::test_support::{MockResponse, TestServer};

    #[test]
    fn issue_key_validation_accepts_the_project_number_grammar_and_rejects_everything_else() {
        assert!(validate_jira_issue_key("PROJ-123").is_ok());
        assert!(validate_jira_issue_key("A1B2-1").is_ok());

        assert!(validate_jira_issue_key("").is_err());
        assert!(validate_jira_issue_key("PROJ").is_err());
        assert!(validate_jira_issue_key("PROJ-").is_err());
        assert!(validate_jira_issue_key("-123").is_err());
        assert!(validate_jira_issue_key("proj-123").is_err());
        assert!(validate_jira_issue_key("PROJ-abc").is_err());
        assert!(validate_jira_issue_key("PROJ-123/../secrets").is_err());
        assert!(validate_jira_issue_key("PROJ/123").is_err());
        assert!(validate_jira_issue_key("PROJ-123?evil=1").is_err());
    }

    #[tokio::test]
    async fn outbound_calls_refuse_a_malformed_issue_key_before_building_a_url() {
        let server = TestServer::start().await;
        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .unwrap()
            .with_bases(server.base_url.clone(), server.base_url.clone());

        let error = client
            .post_comment("token", "cloud-1", "../not-an-issue-key", "hi")
            .await
            .expect_err("malformed issue key must be refused");
        assert!(matches!(error, Error::InvalidJiraIssueKey(_)));
        // No request was ever sent — the check runs before any HTTP call.
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn authorization_code_exchange_parses_token_response() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "expires_in": 3600,
            }),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .unwrap()
            .with_bases(server.base_url.clone(), server.base_url.clone());
        let tokens = client
            .exchange_code("auth-code", "https://example.com/callback")
            .await
            .expect("exchange succeeds");

        assert_eq!(
            tokens,
            OAuthTokens {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_in: 3600,
            }
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/oauth/token");
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).expect("exchange body"),
            serde_json::json!({
                "grant_type": "authorization_code",
                "client_id": "client-id",
                "client_secret": "client-secret",
                "code": "auth-code",
                "redirect_uri": "https://example.com/callback",
            })
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn refresh_returns_the_rotated_pair() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "access_token": "access-2",
                "refresh_token": "refresh-2",
                "expires_in": 7200,
            }),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .unwrap()
            .with_bases(server.base_url.clone(), server.base_url.clone());
        let tokens = client
            .refresh_access_token("refresh-1")
            .await
            .expect("refresh succeeds");

        assert_eq!(tokens.access_token, "access-2");
        assert_eq!(tokens.refresh_token, "refresh-2");
        assert_eq!(tokens.expires_in, 7200);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).expect("refresh body"),
            serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": "client-id",
                "client_secret": "client-secret",
                "refresh_token": "refresh-1",
            })
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn accessible_resources_parses_the_site_list() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!([
                {
                    "id": "cloud-1",
                    "name": "Engineering",
                    "url": "https://example.atlassian.net",
                    "scopes": ["read:jira-work", "write:jira-work"]
                }
            ]),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .unwrap()
            .with_bases(server.base_url.clone(), server.base_url.clone());
        let resources = client
            .accessible_resources("access-1")
            .await
            .expect("resources succeed");

        assert_eq!(
            resources,
            vec![AccessibleResource {
                id: "cloud-1".into(),
                name: "Engineering".into(),
                url: "https://example.atlassian.net".into(),
                scopes: vec!["read:jira-work".into(), "write:jira-work".into()],
            }]
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/oauth/token/accessible-resources");
        assert_eq!(requests[0].headers["authorization"], "Bearer access-1");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn issue_calls_send_expected_method_path_and_body() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({ "id": "comment-1" }),
        ));
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "transitions": [
                    {
                        "id": "31",
                        "name": "Start progress",
                        "to": {
                            "name": "In Progress",
                            "statusCategory": { "key": "indeterminate", "name": "In Progress" }
                        }
                    }
                ]
            }),
        ));
        server.push(MockResponse::text(204, ""));
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "fields": { "updated": "2026-09-03T18:10:00Z" }
            }),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .unwrap()
            .with_bases(server.base_url.clone(), server.base_url.clone());

        client
            .post_comment("access-1", "cloud-1", "ENG-7", "hello jira")
            .await
            .expect("comment succeeds");
        let transitions = client
            .list_transitions("access-1", "cloud-1", "ENG-7")
            .await
            .expect("transitions succeed");
        client
            .transition_issue("access-1", "cloud-1", "ENG-7", "31")
            .await
            .expect("transition succeeds");
        let updated_at = client
            .get_issue_updated_at("access-1", "cloud-1", "ENG-7")
            .await
            .expect("issue fetch succeeds");

        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].path,
            "/ex/jira/cloud-1/rest/api/3/issue/ENG-7/comment"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).expect("comment body"),
            serde_json::json!({
                "body": {
                    "type": "doc",
                    "version": 1,
                    "content": [{
                        "type": "paragraph",
                        "content": [{
                            "type": "text",
                            "text": "hello jira"
                        }]
                    }]
                }
            })
        );
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].id, "31");
        assert_eq!(transitions[0].to_status, "In Progress");
        assert_eq!(transitions[0].to_status_category, "indeterminate");
        assert_eq!(requests[1].method, "GET");
        assert_eq!(
            requests[1].path,
            "/ex/jira/cloud-1/rest/api/3/issue/ENG-7/transitions"
        );
        assert_eq!(requests[2].method, "POST");
        assert_eq!(
            requests[2].path,
            "/ex/jira/cloud-1/rest/api/3/issue/ENG-7/transitions"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[2].body).expect("transition body"),
            serde_json::json!({
                "transition": { "id": "31" }
            })
        );
        assert_eq!(updated_at.as_deref(), Some("2026-09-03T18:10:00Z"));
        assert_eq!(requests[3].method, "GET");
        assert_eq!(
            requests[3].path,
            "/ex/jira/cloud-1/rest/api/3/issue/ENG-7?fields=updated"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn non_success_jira_status_is_reported_with_status_and_body() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            403,
            serde_json::json!({
                "errorMessages": ["forbidden"],
                "access_token": "should-not-leak"
            }),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .unwrap()
            .with_bases(server.base_url.clone(), server.base_url.clone());
        let error = client
            .accessible_resources("access-1")
            .await
            .expect_err("resources should fail");

        match error {
            Error::Api {
                provider,
                action,
                status,
                body,
            } => {
                assert_eq!(provider, "JIRA");
                assert_eq!(action, "listing accessible resources");
                assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
                assert!(body.contains("forbidden"));
                assert!(!body.contains("should-not-leak"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        server.shutdown().await;
    }

    #[test]
    fn refresh_tokens_can_be_sealed_and_opened() {
        let cipher = Cipher::from_base64_key(&B64.encode([9u8; 32])).expect("cipher");
        let tokens = OAuthTokens {
            access_token: "access-1".into(),
            refresh_token: "refresh-1".into(),
            expires_in: 3600,
        };

        let sealed = tokens.seal_refresh_token(&cipher).expect("seal");
        let opened = JiraClient::open_refresh_token(&cipher, &sealed).expect("open");

        assert_eq!(opened, "refresh-1");
    }
}
