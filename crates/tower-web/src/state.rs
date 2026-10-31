//! Everything a console handler needs, and the configuration it cannot infer.

use std::sync::Arc;

use savvagent_resource::PlatformClient;

use crate::oauth::PlatformOAuth;
use savvagent_tenant::crypto::Cipher;
use savvagent_tenant::Db;

/// What the JIRA connection asks Atlassian for.
///
/// Exactly what `tower_trackers::jira::JiraClient` calls: reading an issue's
/// `updated` field and its transitions, writing a comment and a transition.
/// `offline_access` is what yields the refresh token the connection is stored
/// with. Asking for more than this would put a consent screen in front of an
/// admin listing permissions the product never uses.
const JIRA_SCOPES: &str = "read:jira-work write:jira-work offline_access";

/// Deployment-dependent settings.
///
/// `Debug` is written by hand and prints no secret: this holds the platform
/// webhook secret and the tracker providers' client secrets.
#[derive(Clone)]
pub struct Config {
    /// Public base URL of this service — the origin a browser sees. Every link
    /// the product hands out is built from it (the tracker OAuth callbacks).
    pub public_url: String,

    /// Canonical URI of the MCP resource server. Tokens issued by the platform
    /// are audienced for exactly this, and `tower-mcp` refuses anything else.
    ///
    /// Configuration rather than something derived from the request, for the
    /// same reason as in `tower-mcp`: a `Host` header is attacker-controlled, and
    /// an audience derived from one is not an audience check.
    pub resource_uri: String,

    /// Base URL of the Savvagent platform: the authorization server and identity
    /// directory. Where a person is sent to sign in, manage members, or upgrade.
    pub platform_url: String,

    /// The key the platform signs its lifecycle webhooks with
    /// (`/platform/webhooks`). Issued when the platform's webhook URL is
    /// registered for this resource server; not the introspection credential.
    pub platform_webhook_secret: String,

    /// Shared secret for GitHub webhook signature verification. Optional
    /// because tracker integration itself is optional per deployment.
    pub github_app_webhook_secret: Option<String>,

    /// The GitHub App's URL slug, and its *user*-to-server OAuth credentials.
    ///
    /// All three are needed before an admin can connect a GitHub installation:
    /// the slug builds the install link, and the client id/secret pair is what
    /// verifies that the admin who came back from GitHub actually administers
    /// the installation they are claiming. A deployment holding some but not
    /// all of them offers no Connect GitHub button at all — see
    /// [`Config::github_tracker_configured`], which is the one place that
    /// conjunction is computed so the console can never offer a flow the server
    /// cannot finish.
    pub github_app_slug: Option<String>,
    pub github_app_client_id: Option<String>,
    pub github_app_client_secret: Option<String>,

    /// Atlassian's OAuth client credentials, for the JIRA 3LO exchange the
    /// tracker console performs. Optional for the same reason.
    pub jira_client_id: Option<String>,
    pub jira_client_secret: Option<String>,

    /// The console's own OAuth client id at the platform: a public first-party
    /// client, registered by an operator
    /// (`savvagent-platform-server client register … --first-party`). `None` turns
    /// console login off — `/auth/login` answers `503 console_login_disabled`
    /// — while the rest of the server runs as usual, so a deployment can be
    /// brought up before the client is registered.
    pub console_client_id: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("public_url", &self.public_url)
            .field("resource_uri", &self.resource_uri)
            .field("platform_url", &self.platform_url)
            .field("console_client_id", &self.console_client_id)
            .field(
                "github_tracker_configured",
                &self.github_tracker_configured(),
            )
            .field("jira_tracker_configured", &self.jira_tracker_configured())
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn new(
        public_url: impl Into<String>,
        resource_uri: impl Into<String>,
        platform_url: impl Into<String>,
        platform_webhook_secret: impl Into<String>,
    ) -> Self {
        Self {
            public_url: public_url.into().trim_end_matches('/').to_string(),
            resource_uri: resource_uri.into(),
            platform_url: platform_url.into().trim_end_matches('/').to_string(),
            platform_webhook_secret: platform_webhook_secret.into(),
            github_app_webhook_secret: None,
            github_app_slug: None,
            github_app_client_id: None,
            github_app_client_secret: None,
            jira_client_id: None,
            jira_client_secret: None,
            console_client_id: None,
        }
    }

    /// Where the platform sends a browser back with an authorization code. The
    /// URI registered for the console client must be exactly this.
    pub fn console_redirect_uri(&self) -> String {
        self.url("/auth/callback")
    }

    /// Where both providers send a browser back after authorization.
    ///
    /// One static string per deployment, registered with GitHub and Atlassian,
    /// which is exactly why it cannot be org-scoped: a redirect URI is fixed at
    /// the provider and an org slug varies per customer. The org travels in the
    /// OAuth `state` instead — see
    /// `docs/specs/2026-09-04-tracker-console-design.md` §1.
    pub fn tracker_callback_url(&self) -> String {
        self.url("/trackers/callback")
    }

    /// Whether this deployment can take an admin through connecting GitHub.
    ///
    /// The conjunction lives here rather than in the console because getting it
    /// wrong is silent in the worst direction: a Connect GitHub button on a
    /// deployment with no OAuth client sends an admin to GitHub, through an
    /// install, and back to an error — having already installed an App that now
    /// has to be uninstalled by hand.
    ///
    /// The App id and private key are deliberately *not* part of this. They are
    /// what `tower-mcp` mints tokens with once a connection exists; a deployment
    /// missing them has working setup and broken sync, which is a different
    /// failure with a different message, and pretending otherwise here would
    /// hide the connection an operator needs to see to diagnose it.
    pub fn github_tracker_configured(&self) -> bool {
        self.github_app_slug.is_some()
            && self.github_app_client_id.is_some()
            && self.github_app_client_secret.is_some()
    }

    pub fn jira_tracker_configured(&self) -> bool {
        self.jira_client_id.is_some() && self.jira_client_secret.is_some()
    }

    /// The GitHub App installation link, minus its `state`.
    ///
    /// Built here rather than in the console bundle: a hard-coded App slug is
    /// how a staging console sends an admin to install the production App.
    pub fn github_install_url(&self) -> Option<String> {
        if !self.github_tracker_configured() {
            return None;
        }
        let slug = self.github_app_slug.as_ref()?;
        let mut url =
            url::Url::parse(&format!("https://github.com/apps/{slug}/installations/new")).ok()?;
        url.query_pairs_mut()
            .append_pair("redirect_uri", &self.tracker_callback_url());
        Some(url.into())
    }

    /// The Atlassian consent link, minus its `state`.
    ///
    /// `offline_access` is not optional decoration — it is what makes Atlassian
    /// return a refresh token, and without one a connection works until the
    /// first access token expires and then stops, an hour after the admin
    /// watched it succeed.
    pub fn jira_authorize_url(&self) -> Option<String> {
        if !self.jira_tracker_configured() {
            return None;
        }
        let client_id = self.jira_client_id.as_ref()?;
        let mut url = url::Url::parse("https://auth.atlassian.com/authorize").ok()?;
        url.query_pairs_mut()
            .append_pair("audience", "api.atlassian.com")
            .append_pair("client_id", client_id)
            .append_pair("scope", JIRA_SCOPES)
            .append_pair("redirect_uri", &self.tracker_callback_url())
            .append_pair("response_type", "code")
            // Atlassian returns a refresh token only when consent is actually
            // shown; without this an admin who has authorized before gets a
            // silent re-approval and no refresh token, and the connection dies
            // an hour later.
            .append_pair("prompt", "consent");
        Some(url.into())
    }

    /// Join a path onto the public URL. Every link handed to a human goes
    /// through here so there is one place a trailing slash can be got wrong.
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.public_url, path.trim_start_matches('/'))
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    /// Decrypts secrets at rest (currently tracker webhook secrets and JIRA
    /// OAuth credentials — see `tower_core::trackers` and `tower_trackers::jira`).
    /// Held as an `Arc` because the key material is
    /// loaded once at startup and shared by every request.
    pub cipher: Arc<Cipher>,
    pub config: Arc<Config>,
    /// The Savvagent platform: introspects the bearer tokens console requests carry
    /// and answers identity questions (org slug, team by slug or id).
    pub platform: Arc<PlatformClient>,
    /// The platform's token endpoints (code exchange, refresh, revoke), for the
    /// console's own login. Server-to-server only: the platform serves no CORS.
    pub oauth: Arc<PlatformOAuth>,
    /// Reads the org's billing standing before a tracker webhook opens its
    /// transaction, so a blocked org's ticket close is recorded unbilled
    /// (`routes::webhooks`). Never refuses anything here, so its fix URL is
    /// only the default.
    pub meter: tower_billing::Meter,
}

impl AppState {
    pub fn new(db: Db, cipher: Cipher, platform: Arc<PlatformClient>, config: Config) -> Self {
        Self {
            db,
            cipher: Arc::new(cipher),
            oauth: Arc::new(PlatformOAuth::new(&config.platform_url)),
            meter: tower_billing::Meter::new(
                platform.clone(),
                tower_billing::default_fix_url(&config.platform_url),
            ),
            config: Arc::new(config),
            platform,
        }
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No cipher, and not the whole config: it holds the platform webhook
        // secret and the tracker providers' client secrets.
        f.debug_struct("AppState")
            .field("public_url", &self.config.public_url)
            .field("resource_uri", &self.config.resource_uri)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_a_secret() {
        let mut c = Config::new(
            "https://c.test",
            "https://m.test/mcp",
            "https://o.test",
            "whsec-SECRET",
        );
        c.github_app_client_secret = Some("gh-SECRET".into());
        c.jira_client_secret = Some("jira-SECRET".into());
        c.github_app_webhook_secret = Some("hook-SECRET".into());
        assert!(!format!("{c:?}").contains("SECRET"));
    }

    #[test]
    fn urls_survive_a_trailing_slash_on_the_configured_base() {
        let config = Config::new(
            "https://console.test/",
            "https://mcp.test/mcp",
            "https://otto.test/",
            "whsec",
        );
        assert_eq!(config.url("/verify"), "https://console.test/verify");
        assert_eq!(config.url("verify"), "https://console.test/verify");
    }
}
