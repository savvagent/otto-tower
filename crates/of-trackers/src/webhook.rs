//! Webhook verification and parsing.
//!
//! GitHub signs the raw body with one deployment-wide secret. JIRA Automation
//! does not sign payloads, so this crate expects an org-specific shared secret
//! in `X-OF-Webhook-Secret` and a `?site=<cloud-id>` query parameter in the
//! URL; the route resolves the site id to an org before it asks this module to
//! verify the shared secret against that org's sealed value.

use hmac::{Hmac, Mac};
use http::HeaderMap;
use of_core::trackers::{decode_stored_secret, Provider};
use otto_tenant::crypto::Cipher;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

const GITHUB_SIGNATURE_HEADER: &str = "x-hub-signature-256";
const GITHUB_EVENT_HEADER: &str = "x-github-event";
pub const JIRA_WEBHOOK_SECRET_HEADER: &str = "x-of-webhook-secret";

#[derive(Debug, Clone)]
pub enum Verification<'a> {
    Github {
        secret: &'a str,
    },
    Jira {
        cipher: &'a Cipher,
        encoded_secret: &'a str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedWebhook {
    Event(Box<WebhookEvent>),
    Ignored(IgnoredWebhook),
}

impl ParsedWebhook {
    pub fn provider(&self) -> Provider {
        match self {
            Self::Event(event) => event.provider,
            Self::Ignored(ignored) => ignored.provider,
        }
    }

    pub fn connection_external_id(&self) -> &str {
        match self {
            Self::Event(event) => &event.connection_external_id,
            Self::Ignored(ignored) => &ignored.connection_external_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredWebhook {
    pub provider: Provider,
    pub connection_external_id: String,
    pub event: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEvent {
    pub provider: Provider,
    pub connection_external_id: String,
    pub binding_external_ref: String,
    pub action: String,
    pub kind: WebhookEventKind,
    pub issue: IssueSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookEventKind {
    Issue,
    IssueComment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSnapshot {
    pub id: String,
    pub reference: String,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub labels: Vec<String>,
    pub updated_at: Option<String>,
    pub state_reason: Option<String>,
    pub comment: Option<CommentSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentSnapshot {
    pub id: String,
    pub body: String,
    /// GitHub's `comment.user.type == "Bot"`: an app wrote it, otto-tower's own
    /// write-back comments included. False when GitHub sends no `user`, and
    /// always false for JIRA, whose automation payload does not say.
    pub author_is_bot: bool,
}

pub fn jira_site_id(query: Option<&str>) -> Result<String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find(|(key, _)| key == "site")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::InvalidWebhook(
                "JIRA webhook URLs must include a non-empty site query parameter".into(),
            )
        })
}

pub fn verify_and_parse(
    provider: Provider,
    headers: &HeaderMap,
    query: Option<&str>,
    raw_body: &[u8],
    verification: Verification<'_>,
) -> Result<ParsedWebhook> {
    match (provider, verification) {
        (Provider::Github, Verification::Github { secret }) => {
            verify_github_signature(headers, raw_body, secret)?;
            parse_github(headers, raw_body)
        }
        (
            Provider::Jira,
            Verification::Jira {
                cipher,
                encoded_secret,
            },
        ) => {
            let site_id = jira_site_id(query)?;
            verify_jira_secret(headers, cipher, encoded_secret)?;
            parse_jira(raw_body, site_id)
        }
        (Provider::Github, Verification::Jira { .. })
        | (Provider::Jira, Verification::Github { .. }) => Err(Error::Internal(format!(
            "webhook verifier did not match provider {provider}"
        ))),
    }
}

fn verify_github_signature(headers: &HeaderMap, raw_body: &[u8], secret: &str) -> Result<()> {
    let header = header_str(headers, GITHUB_SIGNATURE_HEADER)?;
    let signature = header
        .strip_prefix("sha256=")
        .ok_or_else(|| {
            Error::InvalidWebhook(
                "GitHub webhook signature must be formatted as sha256=<hex>".into(),
            )
        })
        .and_then(decode_hex)?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| {
        Error::Internal("failed to construct a GitHub webhook HMAC verifier".into())
    })?;
    mac.update(raw_body);
    mac.verify_slice(&signature)
        .map_err(|_| Error::InvalidWebhook("GitHub webhook signature did not match".into()))
}

fn verify_jira_secret(headers: &HeaderMap, cipher: &Cipher, encoded_secret: &str) -> Result<()> {
    let presented = header_str(headers, JIRA_WEBHOOK_SECRET_HEADER)?;
    let sealed = decode_stored_secret(encoded_secret)?;
    let expected = cipher.open(&sealed.ciphertext, &sealed.nonce)?;
    // Hash both sides to a fixed-length digest before comparing. `subtle`'s
    // slice `ConstantTimeEq` short-circuits (non-constant-time) when the two
    // slices differ in length, which would otherwise leak the stored secret's
    // byte length through response timing. Comparing two 32-byte SHA-256
    // digests instead means the lengths always match, so the comparison never
    // takes the early-return path.
    let expected_digest = Sha256::digest(&expected);
    let presented_digest = Sha256::digest(presented.as_bytes());
    if expected_digest
        .as_slice()
        .ct_eq(presented_digest.as_slice())
        .unwrap_u8()
        == 1
    {
        Ok(())
    } else {
        Err(Error::InvalidWebhook(
            "JIRA webhook shared secret did not match".into(),
        ))
    }
}

fn parse_github(headers: &HeaderMap, raw_body: &[u8]) -> Result<ParsedWebhook> {
    let event = header_str(headers, GITHUB_EVENT_HEADER)?;
    match event {
        "issues" => {
            let payload: GithubIssuePayload = parse_json(raw_body, "GitHub")?;
            Ok(ParsedWebhook::Event(Box::new(WebhookEvent {
                provider: Provider::Github,
                connection_external_id: payload.installation.id.to_string(),
                binding_external_ref: payload.repository.full_name,
                action: payload.action,
                kind: WebhookEventKind::Issue,
                issue: IssueSnapshot {
                    id: payload.issue.id.to_string(),
                    reference: payload.issue.number.to_string(),
                    title: payload.issue.title,
                    body: payload.issue.body,
                    state: payload.issue.state,
                    labels: payload
                        .issue
                        .labels
                        .into_iter()
                        .map(|label| label.name)
                        .collect(),
                    updated_at: payload.issue.updated_at,
                    state_reason: payload.issue.state_reason,
                    comment: None,
                },
            })))
        }
        "issue_comment" => {
            let payload: GithubIssueCommentPayload = parse_json(raw_body, "GitHub")?;
            Ok(ParsedWebhook::Event(Box::new(WebhookEvent {
                provider: Provider::Github,
                connection_external_id: payload.installation.id.to_string(),
                binding_external_ref: payload.repository.full_name,
                action: payload.action,
                kind: WebhookEventKind::IssueComment,
                issue: IssueSnapshot {
                    id: payload.issue.id.to_string(),
                    reference: payload.issue.number.to_string(),
                    title: payload.issue.title,
                    body: payload.issue.body,
                    state: payload.issue.state,
                    labels: payload
                        .issue
                        .labels
                        .into_iter()
                        .map(|label| label.name)
                        .collect(),
                    updated_at: payload.issue.updated_at,
                    state_reason: payload.issue.state_reason,
                    comment: Some(CommentSnapshot {
                        id: payload.comment.id.to_string(),
                        body: payload.comment.body,
                        author_is_bot: payload
                            .comment
                            .user
                            .and_then(|user| user.kind)
                            .is_some_and(|kind| kind == "Bot"),
                    }),
                },
            })))
        }
        other => {
            let payload: GithubEnvelope = parse_json(raw_body, "GitHub")?;
            Ok(ParsedWebhook::Ignored(IgnoredWebhook {
                provider: Provider::Github,
                connection_external_id: payload.installation.id.to_string(),
                event: other.to_string(),
            }))
        }
    }
}

fn parse_jira(raw_body: &[u8], site_id: String) -> Result<ParsedWebhook> {
    let payload: JiraAutomationPayload = parse_json(raw_body, "JIRA")?;
    Ok(ParsedWebhook::Event(Box::new(WebhookEvent {
        provider: Provider::Jira,
        connection_external_id: site_id,
        binding_external_ref: payload.issue.fields.project.key.clone(),
        action: payload.action,
        kind: if payload.comment.is_some() {
            WebhookEventKind::IssueComment
        } else {
            WebhookEventKind::Issue
        },
        issue: IssueSnapshot {
            id: payload.issue.id,
            reference: payload.issue.key,
            title: payload.issue.fields.summary,
            body: payload.issue.fields.description,
            state: payload.issue.fields.status.name,
            labels: payload.issue.fields.labels,
            updated_at: payload.issue.fields.updated,
            state_reason: None,
            comment: payload.comment.map(|comment| CommentSnapshot {
                id: comment.id,
                body: comment.body,
                author_is_bot: false,
            }),
        },
    })))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str> {
    headers
        .get(name)
        .ok_or_else(|| Error::InvalidWebhook(format!("missing webhook header {name}")))?
        .to_str()
        .map_err(|_| Error::InvalidWebhook(format!("webhook header {name} was not valid UTF-8")))
}

fn parse_json<T: for<'de> Deserialize<'de>>(raw_body: &[u8], provider: &str) -> Result<T> {
    serde_json::from_slice(raw_body).map_err(|error| {
        Error::InvalidWebhook(format!(
            "{provider} webhook payload was not valid JSON: {error}"
        ))
    })
}

fn decode_hex(input: &str) -> Result<Vec<u8>> {
    if !input.len().is_multiple_of(2) {
        return Err(Error::InvalidWebhook(
            "GitHub webhook signature hex had an odd number of characters".into(),
        ));
    }

    let mut out = Vec::with_capacity(input.len() / 2);
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let hi = hex_nibble(bytes[index])?;
        let lo = hex_nibble(bytes[index + 1])?;
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::InvalidWebhook(
            "GitHub webhook signature hex contained a non-hex character".into(),
        )),
    }
}

#[derive(Deserialize)]
struct GithubEnvelope {
    installation: GithubInstallation,
}

#[derive(Deserialize)]
struct GithubIssuePayload {
    action: String,
    installation: GithubInstallation,
    repository: GithubRepository,
    issue: GithubIssue,
}

#[derive(Deserialize)]
struct GithubIssueCommentPayload {
    action: String,
    installation: GithubInstallation,
    repository: GithubRepository,
    issue: GithubIssue,
    comment: GithubComment,
}

#[derive(Deserialize)]
struct GithubInstallation {
    id: i64,
}

#[derive(Deserialize)]
struct GithubRepository {
    full_name: String,
}

#[derive(Deserialize)]
struct GithubIssue {
    id: i64,
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    state_reason: Option<String>,
    labels: Vec<GithubLabel>,
}

#[derive(Deserialize)]
struct GithubComment {
    id: i64,
    body: String,
    #[serde(default)]
    user: Option<GithubUser>,
}

#[derive(Deserialize)]
struct GithubUser {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct GithubLabel {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JiraAutomationPayload {
    action: String,
    issue: JiraAutomationIssue,
    #[serde(default)]
    comment: Option<JiraAutomationComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JiraAutomationIssue {
    id: String,
    key: String,
    fields: JiraAutomationFields,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JiraAutomationFields {
    summary: String,
    #[serde(default)]
    description: Option<String>,
    status: JiraAutomationStatus,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    updated: Option<String>,
    project: JiraAutomationProject,
}

#[derive(Deserialize)]
struct JiraAutomationStatus {
    name: String,
}

#[derive(Deserialize)]
struct JiraAutomationProject {
    key: String,
}

#[derive(Deserialize)]
struct JiraAutomationComment {
    id: String,
    body: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;

    const GITHUB_SECRET: &str = "github-secret";
    const JIRA_SECRET: &str = "jira-secret";
    const GITHUB_ISSUES_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/github-issues.json");
    const GITHUB_COMMENT_FIXTURE: &[u8] =
        include_bytes!("../tests/fixtures/github-issue-comment.json");
    const JIRA_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/jira-automation.json");

    #[test]
    fn github_signature_accepts_a_valid_fixture() {
        let headers = github_headers("issues", GITHUB_ISSUES_FIXTURE);
        let parsed = verify_and_parse(
            Provider::Github,
            &headers,
            None,
            GITHUB_ISSUES_FIXTURE,
            Verification::Github {
                secret: GITHUB_SECRET,
            },
        )
        .expect("valid signature");

        let ParsedWebhook::Event(event) = parsed else {
            panic!("expected an event");
        };
        assert_eq!(event.connection_external_id, "123456");
        assert_eq!(event.binding_external_ref, "acme/api");
        assert_eq!(event.kind, WebhookEventKind::Issue);
        assert_eq!(event.issue.reference, "17");
        assert_eq!(event.issue.labels, vec!["bug", "trackers"]);
        assert_eq!(
            event.issue.updated_at.as_deref(),
            Some("2026-09-03T18:00:00Z")
        );
        assert_eq!(event.issue.state_reason, None);
    }

    #[test]
    fn github_signature_rejects_a_tampered_body() {
        let headers = github_headers("issues", GITHUB_ISSUES_FIXTURE);
        let mut tampered = GITHUB_ISSUES_FIXTURE.to_vec();
        tampered[20] ^= 1;
        let err = verify_and_parse(
            Provider::Github,
            &headers,
            None,
            &tampered,
            Verification::Github {
                secret: GITHUB_SECRET,
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidWebhook(_)), "{err}");
    }

    #[test]
    fn github_signature_verification_is_stateless_for_replays() {
        let headers = github_headers("issues", GITHUB_ISSUES_FIXTURE);
        for _ in 0..2 {
            verify_and_parse(
                Provider::Github,
                &headers,
                None,
                GITHUB_ISSUES_FIXTURE,
                Verification::Github {
                    secret: GITHUB_SECRET,
                },
            )
            .expect("replayed signature still verifies");
        }
    }

    #[test]
    fn github_issue_comment_payload_parses() {
        let headers = github_headers("issue_comment", GITHUB_COMMENT_FIXTURE);
        let parsed = verify_and_parse(
            Provider::Github,
            &headers,
            None,
            GITHUB_COMMENT_FIXTURE,
            Verification::Github {
                secret: GITHUB_SECRET,
            },
        )
        .expect("comment parses");

        let ParsedWebhook::Event(event) = parsed else {
            panic!("expected an event");
        };
        assert_eq!(event.kind, WebhookEventKind::IssueComment);
        assert_eq!(event.action, "created");
        assert_eq!(
            event.issue.updated_at.as_deref(),
            Some("2026-09-03T18:05:00Z")
        );
        assert_eq!(event.issue.state_reason.as_deref(), Some("reopened"));
        assert_eq!(
            event
                .issue
                .comment
                .as_ref()
                .map(|comment| comment.id.as_str()),
            Some("9001")
        );
    }

    #[test]
    fn github_issue_comment_reads_whether_the_author_is_a_bot() {
        let parse = |body: &[u8]| {
            let headers = github_headers("issue_comment", body);
            let parsed = verify_and_parse(
                Provider::Github,
                &headers,
                None,
                body,
                Verification::Github {
                    secret: GITHUB_SECRET,
                },
            )
            .expect("comment parses");
            let ParsedWebhook::Event(event) = parsed else {
                panic!("expected an event");
            };
            event.issue.comment.expect("comment present").author_is_bot
        };
        let with_user = |user: serde_json::Value| {
            let mut v: serde_json::Value = serde_json::from_slice(GITHUB_COMMENT_FIXTURE).unwrap();
            v["comment"]["user"] = user;
            serde_json::to_vec(&v).unwrap()
        };

        // The fixture carries no `user` at all.
        assert!(!parse(GITHUB_COMMENT_FIXTURE));
        assert!(parse(&with_user(
            serde_json::json!({"login": "otto-tower[bot]", "type": "Bot"})
        )));
        assert!(!parse(&with_user(
            serde_json::json!({"login": "alice", "type": "User"})
        )));
        assert!(!parse(&with_user(serde_json::json!({"login": "alice"}))));
        assert!(!parse(&with_user(serde_json::Value::Null)));
    }

    #[test]
    fn jira_comments_are_never_marked_as_bot_authored() {
        let cipher = cipher();
        let encoded = encoded_secret(&cipher, JIRA_SECRET);
        let parsed = verify_and_parse(
            Provider::Jira,
            &jira_headers(JIRA_SECRET),
            Some("site=cloud-123"),
            JIRA_FIXTURE,
            Verification::Jira {
                cipher: &cipher,
                encoded_secret: &encoded,
            },
        )
        .expect("valid secret");
        let ParsedWebhook::Event(event) = parsed else {
            panic!("expected an event");
        };
        assert!(!event.issue.comment.expect("comment present").author_is_bot);
    }

    #[test]
    fn jira_shared_secret_accepts_a_valid_fixture() {
        let cipher = cipher();
        let encoded = encoded_secret(&cipher, JIRA_SECRET);
        let headers = jira_headers(JIRA_SECRET);
        let parsed = verify_and_parse(
            Provider::Jira,
            &headers,
            Some("site=cloud-123"),
            JIRA_FIXTURE,
            Verification::Jira {
                cipher: &cipher,
                encoded_secret: &encoded,
            },
        )
        .expect("valid secret");

        let ParsedWebhook::Event(event) = parsed else {
            panic!("expected an event");
        };
        assert_eq!(event.connection_external_id, "cloud-123");
        assert_eq!(event.binding_external_ref, "DF");
        assert_eq!(event.kind, WebhookEventKind::IssueComment);
        assert_eq!(event.action, "comment_created");
        assert_eq!(event.issue.id, "10001");
        assert_eq!(event.issue.reference, "DF-9");
        assert_eq!(event.issue.title, "Implement webhook ingest");
        assert_eq!(
            event.issue.body.as_deref(),
            Some("Verify signatures before syncing.")
        );
        assert_eq!(event.issue.state, "In Progress");
        assert_eq!(event.issue.labels, vec!["trackers", "webhooks"]);
        assert_eq!(
            event.issue.updated_at.as_deref(),
            Some("2026-09-03T18:10:00Z")
        );
        assert_eq!(event.issue.state_reason, None);
        let comment = event.issue.comment.as_ref().expect("comment present");
        assert_eq!(comment.id, "20002");
        assert_eq!(comment.body, "Sync this change into otto-tower.");
    }

    #[test]
    fn jira_shared_secret_rejects_the_wrong_header() {
        let cipher = cipher();
        let encoded = encoded_secret(&cipher, JIRA_SECRET);
        let headers = jira_headers("wrong-secret");
        let err = verify_and_parse(
            Provider::Jira,
            &headers,
            Some("site=cloud-123"),
            JIRA_FIXTURE,
            Verification::Jira {
                cipher: &cipher,
                encoded_secret: &encoded,
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidWebhook(_)), "{err}");
    }

    #[test]
    fn jira_site_query_is_required() {
        let err = jira_site_id(None).unwrap_err();
        assert!(matches!(err, Error::InvalidWebhook(_)), "{err}");
    }

    fn github_headers(event: &str, body: &[u8]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(GITHUB_EVENT_HEADER, event.parse().unwrap());
        headers.insert(
            GITHUB_SIGNATURE_HEADER,
            format!("sha256={}", github_signature(body))
                .parse()
                .unwrap(),
        );
        headers
    }

    fn jira_headers(secret: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(JIRA_WEBHOOK_SECRET_HEADER, secret.parse().unwrap());
        headers
    }

    fn github_signature(body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(GITHUB_SECRET.as_bytes()).unwrap();
        mac.update(body);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn cipher() -> Cipher {
        Cipher::from_base64_key(&B64.encode([7u8; 32])).unwrap()
    }

    fn encoded_secret(cipher: &Cipher, secret: &str) -> String {
        let sealed = cipher.seal(secret.as_bytes()).unwrap();
        B64.encode([sealed.nonce, sealed.ciphertext].concat())
    }
}
