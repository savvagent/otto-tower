use reqwest::StatusCode;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("GitHub App private key could not be parsed as a PEM-encoded RSA key")]
    InvalidGithubPrivateKey(#[source] jsonwebtoken::errors::Error),

    #[error("{provider} request failed while {action}: {source}")]
    Http {
        provider: &'static str,
        action: &'static str,
        #[source]
        source: reqwest::Error,
    },

    #[error("{provider} returned HTTP {status} while {action}: {body}")]
    Api {
        provider: &'static str,
        action: &'static str,
        status: StatusCode,
        body: String,
    },

    #[error("{provider} returned an invalid response while {action}: {message}")]
    InvalidResponse {
        provider: &'static str,
        action: &'static str,
        message: String,
    },

    #[error("GitHub issue state must be \"open\" or \"closed\", got {0:?}")]
    InvalidGithubIssueState(String),

    /// The admin came back from GitHub claiming an installation their own
    /// GitHub account cannot see. Refusing here is what stops an org admin
    /// binding an installation id belonging to somebody else — see
    /// `docs/specs/2026-09-04-tracker-console-design.md` §2.
    #[error(
        "the signed-in GitHub account does not administer installation {installation_id}. \
         Run Connect GitHub again from the account that installed the App on that organization."
    )]
    GithubInstallationNotAdministered { installation_id: i64 },

    /// GitHub answers a spent, forged, or mismatched authorization code with
    /// HTTP 200 and an `error` field, so this is not reachable through
    /// [`Error::Api`] and needs saying separately.
    #[error(
        "GitHub rejected the authorization code ({0}). Run Connect GitHub again — a code can \
         only be redeemed once, so reloading the page after connecting reuses a spent one."
    )]
    GithubUserCodeRejected(String),

    #[error(
        "GitHub issue reference {owner:?}/{repo:?}#{issue_number} is not owner/repo#number \
         with an owner and repo of [A-Za-z0-9._-]; refusing to build a request URL from it"
    )]
    InvalidGithubIssueRef {
        owner: String,
        repo: String,
        issue_number: i64,
    },

    #[error("JIRA returned a refresh token that was not valid UTF-8")]
    InvalidJiraRefreshTokenEncoding,

    #[error(
        "JIRA issue key {0:?} does not match the PROJECT-123 grammar; refusing to build a request URL from it"
    )]
    InvalidJiraIssueKey(String),

    #[error("{0}")]
    InvalidWebhook(String),

    #[error("{0}")]
    Internal(String),

    #[error(transparent)]
    Core(#[from] of_core::Error),

    /// A seal/open failure from `otto_tenant::crypto::Cipher`.
    #[error(transparent)]
    Tenant(#[from] otto_tenant::Error),
}
