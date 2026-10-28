//! `of-mcp` — the Streamable HTTP MCP surface.
//!
//! This crate is the product's front door: one HTTPS endpoint that any
//! MCP-speaking coding agent can be pointed at, with OAuth 2.1 in front of it
//! (the otto platform is the authorization server) and `of-core`'s
//! tenant-pinned queue behind it.
//!
//! ```text
//!   POST /mcp
//!     └─ require_bearer ──── introspect the token at the platform, attach the principal
//!          └─ StreamableHttpService ──── rmcp session, JSON-RPC framing
//!               └─ Factory ──── one tool call
//!                    └─ Db::begin(org) ──── of-core, RLS, commit
//! ```
//!
//! ## Three decisions worth knowing before changing anything here
//!
//! **No SQL lives in this crate.** Every statement is a `of-core` method on a
//! [`otto_tenant::Tx`], which cannot be constructed without an org. A query written
//! here would bypass the pinning that tenant isolation's second guard depends
//! on, so there is no "just this once" version of it.
//!
//! **Nothing is client-specific.** No tool annotation, hook, or capability that
//! only one agent understands, no validation of `agentType` against a list of
//! known agents, no branch on the client's name. A feature that works in one
//! coding agent and not another does not ship, so the surface is plain
//! Streamable HTTP and plain JSON Schema.
//!
//! **The server ships no workflow.** These tools are primitives — queue,
//! dependencies, claims, leases, messages, change notification. How work is
//! specified, planned, reviewed, or measured belongs in the customer's own
//! skills calling these tools, which is what the opaque `metadata` field on a
//! job is for. When a proposed feature could live either here or in a caller's
//! skill, it belongs in the skill.

pub mod auth;
pub mod error;
pub mod legacy;
pub mod server;
pub mod tools;

use std::sync::Arc;

use axum::routing::{any_service, get};
use axum::Router;
use of_core::watch::Watcher;
use otto_resource::PlatformClient;
use otto_tenant::Db;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

pub use auth::ResourceServer;
pub use legacy::{Deprecation, Legacy};
pub use server::Factory;

/// Deployment-dependent settings the MCP surface cannot infer for itself.
#[derive(Debug, Clone)]
pub struct Config {
    /// The otto platform: authorization server, identity directory, billing.
    pub platform: Arc<PlatformClient>,
    /// The platform's public base URL, advertised as this resource's
    /// authorization server in its protected-resource metadata.
    pub platform_url: String,
    /// This resource's canonical URI, and the audience every token must carry.
    /// Must match the `resource_uri` registered with the platform.
    pub resource_uri: String,
    /// Public base URL of this service, used to build the discovery pointer in
    /// a `401`.
    pub public_url: String,
    /// Hostnames or `host:port` authorities accepted in the `Host` header.
    ///
    /// `rmcp` defaults this to loopback only, which is a sensible default for
    /// the local servers it is usually used to write and completely wrong for
    /// a hosted one: leave it and every request from the internet is rejected
    /// with an error that says nothing about hostnames. It is required here so
    /// nobody discovers that in production.
    pub allowed_hosts: Vec<String>,
    /// Browser origins accepted on requests that carry `Origin`. Empty disables
    /// the check, which is right for a surface reached by CLI agents rather
    /// than by pages.
    pub allowed_origins: Vec<String>,
    /// Where an owner goes to lift a budget or payment block. Named in the
    /// refusal itself, because an error that says "raise the budget" without
    /// saying where is a dead end. Defaults to the platform console's home,
    /// which lists the user's orgs; billing lives under each org there.
    pub upgrade_url: String,
    /// GitHub App id for outbound tracker sync. Optional because tracker sync
    /// itself is optional per deployment.
    pub github_app_id: Option<i64>,
    /// PEM-encoded GitHub App private key for installation-token minting.
    pub github_app_private_key: Option<String>,
    /// Atlassian OAuth client id for JIRA write-back.
    pub jira_client_id: Option<String>,
    /// Atlassian OAuth client secret for JIRA write-back.
    pub jira_client_secret: Option<String>,
    /// Encryption key for decrypting stored tracker credentials. Not used by
    /// most tool calls; carried here so the JIRA sync path can open the stored
    /// refresh token when tracker sync is enabled.
    pub encryption_key: Option<String>,
}

impl Config {
    pub fn new(
        platform: Arc<PlatformClient>,
        platform_url: impl Into<String>,
        resource_uri: impl Into<String>,
        public_url: impl Into<String>,
    ) -> Self {
        let public_url = public_url.into();
        let host = url::Url::parse(&public_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();

        let platform_url = platform_url.into();
        Self {
            platform,
            platform_url: platform_url.clone(),
            resource_uri: resource_uri.into(),
            allowed_hosts: if host.is_empty() { vec![] } else { vec![host] },
            allowed_origins: vec![],
            // The platform owns billing, so its console is where a human goes.
            upgrade_url: of_billing::default_fix_url(&platform_url),
            github_app_id: None,
            github_app_private_key: None,
            jira_client_id: None,
            jira_client_secret: None,
            encryption_key: None,
            public_url,
        }
    }
}

/// Build the MCP surface, ready to be merged into `of-server`'s router.
///
/// Two routes, and only one of them is authenticated:
///
/// - `/.well-known/oauth-protected-resource` is deliberately **open**. It is
///   what an unauthenticated client reads to discover how to authenticate, so
///   putting it behind authentication would be a closed loop. It names the
///   platform as the authorization server.
/// - `/mcp` requires a bearer token audienced for [`Config::resource_uri`],
///   validated by the platform.
///
/// The transport runs **stateless** (`stateful_mode: false`, `json_response:
/// true`). otto-tower never pushes to a client — `watch` is a long poll the
/// agent initiates — so a server-held session would buy nothing and cost the
/// one thing a hosted multi-replica deployment cannot easily give it: every
/// request from one client landing on the replica that holds its session.
/// Stateless means any replica can serve any request, with no sticky routing
/// and no shared session store.
/// Every path [`router`] serves, so `of-server` can check what it would do with
/// them (a legacy host must never redirect one) without starting a server.
pub const ROUTES: [&str; 2] = ["/.well-known/oauth-protected-resource", "/mcp"];

pub fn router(db: Db, watcher: Arc<Watcher>, config: Config) -> Router {
    let [metadata, mcp] = ROUTES;
    let rs = Arc::new(ResourceServer::new(
        db.clone(),
        config.platform.clone(),
        config.resource_uri.clone(),
        config.public_url.clone(),
        config.platform_url.clone(),
    ));

    // #[non_exhaustive], so built by mutation rather than a struct literal.
    let mut transport = StreamableHttpServerConfig::default();
    transport.stateful_mode = false;
    transport.json_response = true;
    transport.allowed_hosts = config.allowed_hosts;
    transport.allowed_origins = config.allowed_origins;

    let tracker_sync = crate::server::TrackerSyncConfig {
        github_app_id: config.github_app_id,
        github_app_private_key: config.github_app_private_key.clone(),
        // Deliberately not configurable: only tests point the client elsewhere.
        github_api_base: None,
        jira_client_id: config.jira_client_id.clone(),
        jira_client_secret: config.jira_client_secret.clone(),
        encryption_key: config.encryption_key.clone(),
    };
    let factory = Factory::new_with_tracker_sync(
        db,
        watcher,
        config.platform.clone(),
        of_billing::Meter::new(config.platform.clone(), config.upgrade_url),
        tracker_sync,
    );
    let service = StreamableHttpService::new(
        // Called per session. `Factory` is cheap to clone — a pool handle, a few
        // `Arc`s, and the tool router — so this is not a per-request cost worth
        // engineering around.
        move || Ok(factory.clone()),
        Arc::new(LocalSessionManager::default()),
        transport,
    );

    Router::new()
        .route(metadata, get(auth::protected_resource_metadata))
        .with_state(rs.clone())
        .route_service(
            mcp,
            any_service(service).layer(axum::middleware::from_fn_with_state(
                rs,
                auth::require_bearer,
            )),
        )
}
