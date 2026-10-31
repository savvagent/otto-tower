//! `of-server` — one binary, one port, every surface.
//!
//! The crates are a compile-time layering discipline, not separate services, so
//! this is where they stop being libraries:
//!
//! ```text
//!   /healthz /readyz          health      no database on the liveness path
//!   /api/…                    of-web      the console API: session cookie or platform bearer token
//!   /auth/…                   of-web      console sign-in (OAuth code + PKCE against the platform)
//!   /webhooks/…               of-web      tracker deliveries, provider-signed
//!   /platform/webhooks        of-web      the platform's lifecycle events, signed
//!   /.well-known/…            of-mcp      discovery, open by necessity
//!   /mcp                      of-mcp      bearer tokens, the agent surface
//!   everything else           web/build   the console SPA, index.html fallback
//! ```
//!
//! Outside all of that, a request whose `Host` is one of `OF_REDIRECT_HOSTS` is
//! answered with a `308` to the same path and query on `OF_PUBLIC_URL`, for every
//! method and path, before host allow-listing, auth or anything below sees it.
//!
//! Around the rest, `of_mcp::legacy::mark` stamps a deprecation notice on
//! every response to a request that came through an `OF_LEGACY_HOSTS` host,
//! and such a request for the console (or `/auth/login`) is `308`-redirected
//! to the same path on `OF_PUBLIC_URL` instead of being served.
//!
//! Savvagent Tower is a resource server of the Savvagent platform, which is the OAuth
//! authorization server and the system of record for identity and billing. There
//! is no `/oauth/*` or account surface here; the console signs in *through* the
//! platform (`/auth/*`, an OAuth client), and both HTTP surfaces validate the
//! platform's tokens by introspection.
//!
//! Assembly is a library function rather than something buried in `main` so a
//! test can build the whole router. Axum panics on a route registered twice, and
//! a panic at startup is only a good failure if something other than a
//! deployment reaches it first.

pub mod config;
pub mod health;

use std::convert::Infallible;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use of_core::watch::Watcher;
use otto_resource::{ClientConfig, PlatformClient};
use otto_tenant::Db;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

pub use config::{Config, LogFormat};

/// Path prefixes that belong to an API rather than to the console's routing.
///
/// Everything else falls through to the single-page app, which is what makes a
/// hard refresh of `/o/acme/queue` work. An unmatched path under one of these
/// must not: a client that `GET`s `/api/orgs/nope` needs a `404` it can parse,
/// and `200 text/html` is the shape that makes an agent retry forever against a
/// route that will never exist. `/oauth` is listed although nothing is served
/// there any more: an old client that still tries it should get a JSON `404`
/// naming where authorization lives now, not a console page. `/auth` is the
/// console's own sign-in (`/auth/login`, `/auth/callback`, `/auth/logout`); a
/// mistyped path under it is likewise a JSON `404`, never `index.html` served
/// to a browser that is in the middle of a redirect.
const API_PREFIXES: [&str; 7] = [
    "/api",
    "/auth",
    "/oauth",
    "/mcp",
    "/.well-known",
    "/platform",
    "/webhooks",
];

fn is_api_path(path: &str) -> bool {
    API_PREFIXES
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

/// Whether a request that arrived through a legacy host is sent to the
/// canonical host instead of being served: the console, and the console's
/// sign-in.
///
/// The console cannot work on the old host. `/auth/login` would set its PKCE
/// cookie on the old host while the platform sends the browser back to
/// `OF_PUBLIC_URL`'s `/auth/callback`, where that cookie does not exist, so the
/// sign-in fails; and a session cookie set on the old host would make every
/// cookie-authenticated write fail the CSRF origin check. Moving the browser
/// before either happens is the only fix. In a Worker-fronted deployment the
/// Worker already does this before the request reaches here; production is
/// not fronted by it, which is why the origin has to.
///
/// Everything an agent, a provider or the platform calls stays where it is
/// for the whole deprecation window: `/mcp`, discovery, `/api`, the webhook
/// receivers, `/platform/webhooks` and the health probes are never redirected.
/// A redirect there would break a client that does not follow one (most
/// webhook senders) or that pins its OAuth resource to the URL it was given.
fn moves_to_canonical(path: &str) -> bool {
    path == "/auth/login" || (!is_api_path(path) && !is_health_path(path))
}

/// A health probe, matched on segment boundaries like [`is_api_path`] — the
/// same rule the Worker's `belongsToOrigin` applies to them, so `/readyz/x` is
/// the origin's (a `404`) whether or not the Worker is in front.
fn is_health_path(path: &str) -> bool {
    health::PATHS
        .iter()
        .any(|probe| path == *probe || path.starts_with(&format!("{probe}/")))
}

/// `308` a legacy-host console request to the same path and query on the
/// canonical host ([`of_mcp::Legacy::redirect`]). Runs inside
/// [`of_mcp::legacy::mark`], so the redirect carries the same
/// `Deprecation`/`Link`/`Sunset` notice as every other legacy-host response,
/// and its `Location` is the `Link`'s target.
async fn legacy_console_redirect(req: Request<Body>, next: Next) -> Response {
    match req.extensions().get::<of_mcp::Legacy>() {
        Some(legacy) if moves_to_canonical(req.uri().path()) => legacy.redirect(req.uri()),
        _ => next.run(req).await,
    }
}

/// The client every call to the Savvagent platform goes through.
///
/// Built once and shared: it holds the introspection and usage-status caches,
/// so a second client would be a second, colder cache. Fallible because the
/// platform URL is only a `String` until something parses it.
pub fn platform_client(config: &Config) -> Result<Arc<PlatformClient>> {
    let client = PlatformClient::new(ClientConfig::new(
        &config.platform_url,
        &config.resource_uri,
        &config.introspection_secret,
    ))
    .context("OF_PLATFORM_URL is not usable")?;
    Ok(Arc::new(client))
}

/// Build the whole application.
///
/// Fallible because the encryption key is only a `String` until something tries
/// to use it. `Config::from_env` cannot prove the key parses without building a
/// `Cipher`, and a `Config` assembled by hand — a test, a future binary — need
/// not have come from the environment at all. Returning the error keeps this
/// crate's rule that a bad value is a named failure and never a panic.
pub fn router(
    db: Db,
    watcher: Arc<Watcher>,
    platform: Arc<PlatformClient>,
    config: &Config,
) -> Result<Router> {
    assemble(db, watcher, platform, config, None)
}

/// [`router`], for a process that runs the usage shipper: `/readyz` also reports
/// `shipper`'s state (see [`health`]).
pub fn router_with_shipper(
    db: Db,
    watcher: Arc<Watcher>,
    platform: Arc<PlatformClient>,
    config: &Config,
    shipper: of_billing::outbox::ShipperStatus,
) -> Result<Router> {
    assemble(db, watcher, platform, config, Some(shipper))
}

fn assemble(
    db: Db,
    watcher: Arc<Watcher>,
    platform: Arc<PlatformClient>,
    config: &Config,
    shipper: Option<of_billing::outbox::ShipperStatus>,
) -> Result<Router> {
    let web = of_web::router(web_state(db.clone(), platform.clone(), config)?);

    // The agent surface, plus `/.well-known/oauth-protected-resource`.
    let mcp = of_mcp::router(db.clone(), watcher, mcp_config(platform, config));

    let health = match shipper {
        Some(shipper) => health::router_with_shipper(db, shipper),
        None => health::router(db),
    };
    Ok(health
        .merge(web)
        .merge(mcp)
        .fallback_service(console(config))
        // Inside `mark`, which is what puts the `Legacy` marker it reads in the
        // request, and so that the redirect it answers is stamped too.
        .layer(axum::middleware::from_fn(legacy_console_redirect))
        // Request spans, without headers. `DefaultMakeSpan::include_headers`
        // would put `Authorization` in the logs — every bearer token, in
        // plaintext, in whatever the log aggregator retains. Do not turn it on.
        .layer(TraceLayer::new_for_http())
        // Outermost, so every response through a legacy host carries the
        // deprecation notice whichever surface — or the console fallback, or a
        // `401` — answered it, and so the marker it leaves in the request is
        // there for `/mcp` and discovery to read. With no `OF_LEGACY_HOSTS` it
        // passes every request through untouched.
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(deprecation(config)?),
            of_mcp::legacy::mark,
        ))
        // Outside even `mark`: a redirect-only host is not a legacy host, gets
        // no notice, and nothing (allow-listing, auth, the marker) runs for it.
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(Redirects::new(config)),
            redirect_old_hosts,
        )))
}

/// The redirect-only hosts (`OF_REDIRECT_HOSTS`) and where they send everyone.
struct Redirects {
    hosts: Vec<String>,
    /// `OF_PUBLIC_URL` without a trailing slash. The only source of the
    /// scheme and host in a `Location`; the request supplies the path and query
    /// and nothing else.
    public_url: String,
}

impl Redirects {
    fn new(config: &Config) -> Self {
        Self {
            hosts: config.redirect_hosts.clone(),
            public_url: config.public_url.trim_end_matches('/').to_string(),
        }
    }

    /// Whether the request was addressed to a redirect-only host. Reads `Host`
    /// (else the URI's authority, for HTTP/2), never `X-Forwarded-Host`, which
    /// any client can set; the port is ignored.
    fn matches(&self, req: &Request<Body>) -> bool {
        if self.hosts.is_empty() {
            return false;
        }
        let authority = req
            .headers()
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| req.uri().authority().map(|a| a.as_str()));
        let Some(authority) = authority else {
            return false;
        };
        let host = match authority.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
            _ => authority,
        };
        self.hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
    }

    fn response(&self, req: &Request<Body>) -> Response {
        let target = req.uri().path_and_query().map_or("/", |pq| pq.as_str());
        let location = http::HeaderValue::from_str(&format!("{}{target}", self.public_url))
            .or_else(|_| http::HeaderValue::from_str(&format!("{}/", self.public_url)));
        let mut response = StatusCode::PERMANENT_REDIRECT.into_response();
        if let Ok(location) = location {
            response
                .headers_mut()
                .insert(http::header::LOCATION, location);
        }
        response
    }
}

/// `308` every request to a redirect-only host to the canonical origin,
/// keeping method semantics (a `308` is replayed as the same method and body).
async fn redirect_old_hosts(
    axum::extract::State(redirects): axum::extract::State<Arc<Redirects>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if redirects.matches(&req) {
        redirects.response(&req)
    } else {
        next.run(req).await
    }
}

/// The legacy hosts and what their callers are told: the canonical public
/// origin and MCP URI as successors, and the sunset date if one is set.
///
/// Fallible for the same reason as [`router`]: a hand-built `Config` need not
/// have come through `from_env`'s checks, and a legacy URL built from a value
/// that does not parse would be a discovery document nobody can follow.
fn deprecation(config: &Config) -> Result<of_mcp::Deprecation> {
    if config.legacy_hosts.is_empty() {
        return Ok(of_mcp::Deprecation::default());
    }
    let public_url = url::Url::parse(&config.public_url)
        .with_context(|| format!("OF_PUBLIC_URL is not a URL: {:?}", config.public_url))?;
    let resource_uri = url::Url::parse(&config.resource_uri)
        .with_context(|| format!("OF_RESOURCE_URI is not a URL: {:?}", config.resource_uri))?;
    Ok(of_mcp::Deprecation::new(
        &public_url,
        &resource_uri,
        config.legacy_hosts.iter().cloned(),
        config.legacy_sunset,
    ))
}

/// `of-web`'s state, with the settings that are this deployment's to decide.
fn web_state(db: Db, platform: Arc<PlatformClient>, config: &Config) -> Result<of_web::AppState> {
    let cipher = otto_tenant::crypto::Cipher::from_base64_key(&config.encryption_key)
        .context("OF_ENCRYPTION_KEY is not a valid 32-byte base64 key")?;

    Ok(of_web::AppState::new(
        db,
        cipher,
        platform,
        web_config(config),
    ))
}

/// Every setting `of-web` takes from this deployment, in one place so it can be
/// tested without a database.
///
/// Split out because the failure mode is silence: a field added to
/// `of_web::Config` that nothing here assigns keeps its `Default`, and the
/// console then reports that default as fact.
fn web_config(config: &Config) -> of_web::Config {
    let mut web = of_web::Config::new(
        &config.public_url,
        &config.resource_uri,
        &config.platform_url,
        &config.platform_webhook_secret,
    );
    web.github_app_webhook_secret = config.github_app_webhook_secret.clone();
    // The tracker console needs all five to take an admin through connecting a
    // provider: the slug and the OAuth pair for GitHub, the OAuth pair for JIRA.
    web.github_app_slug = config.github_app_slug.clone();
    web.github_app_client_id = config.github_app_client_id.clone();
    web.github_app_client_secret = config.github_app_client_secret.clone();
    web.jira_client_id = config.jira_client_id.clone();
    web.jira_client_secret = config.jira_client_secret.clone();
    web.console_client_id = config.console_client_id.clone();
    web
}

fn mcp_config(platform: Arc<PlatformClient>, config: &Config) -> of_mcp::Config {
    let mut mcp = of_mcp::Config::new(
        platform,
        &config.platform_url,
        &config.resource_uri,
        &config.public_url,
    );
    mcp.allowed_hosts = config.allowed_hosts();
    mcp.allowed_origins = config.allowed_origins.clone();
    mcp.upgrade_url = config.upgrade_url.clone();
    mcp.github_app_id = config.github_app_id;
    mcp.github_app_private_key = config.github_app_private_key.clone();
    mcp.jira_client_id = config.jira_client_id.clone();
    mcp.jira_client_secret = config.jira_client_secret.clone();
    mcp.encryption_key = Some(config.encryption_key.clone());
    mcp
}

/// The console bundle, or a JSON `404` for anything API-shaped.
///
/// `ServeDir` falls back to `index.html` for any path it has no file for, which
/// is what `adapter-static` produces and what client-side routing needs. That
/// fallback is exactly why the API prefixes are checked first.
fn console(
    config: &Config,
) -> impl tower::Service<Request<Body>, Response = Response, Error = Infallible, Future = impl Send>
       + Clone
       + Send
       + 'static {
    let assets = ServeDir::new(&config.static_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(config.static_dir.join("index.html")));

    tower::service_fn(move |req: Request<Body>| {
        let assets = assets.clone();
        async move {
            if is_api_path(req.uri().path()) {
                return Ok(not_found(req.uri().path()));
            }
            Ok(assets
                .oneshot(req)
                .await
                .map(|res| res.map(Body::new))
                .into_response())
        }
    })
}

fn not_found(path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "not_found",
            "error_description":
                format!("no route serves {path}. See /api/openapi.json for the console API, \
                         and /.well-known/oauth-protected-resource for the MCP endpoint and the \
                         authorization server (the Savvagent platform) that issues its tokens."),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A setting that `of-server` reads but never passes on is invisible: the
    /// console runs on `of_web::Config`'s default and calls it fact.
    #[test]
    fn every_deployment_setting_reaches_of_web() {
        let mut config = Config::for_test();
        config.github_app_webhook_secret = Some("webhook-secret".into());
        config.github_app_slug = Some("otto-tower".into());
        config.github_app_client_id = Some("gh-client".into());
        config.github_app_client_secret = Some("gh-secret".into());
        config.jira_client_id = Some("jira-client".into());
        config.jira_client_secret = Some("jira-secret".into());
        config.console_client_id = Some("console-client".into());

        let web = web_config(&config);

        assert_eq!(web.console_client_id.as_deref(), Some("console-client"));
        assert_eq!(
            web.console_redirect_uri(),
            "https://factory.example.com/auth/callback"
        );

        assert_eq!(
            web.github_app_webhook_secret.as_deref(),
            Some("webhook-secret")
        );
        assert_eq!(web.platform_url, config.platform_url);
        assert_eq!(web.platform_webhook_secret, config.platform_webhook_secret);
        assert!(
            web.github_tracker_configured() && web.jira_tracker_configured(),
            "the tracker console cannot connect a provider it was never told the credentials for"
        );
        assert_eq!(web.public_url, config.public_url);
        assert_eq!(web.resource_uri, config.resource_uri);
    }

    #[test]
    fn every_tracker_setting_reaches_of_mcp() {
        let mut config = Config::for_test();
        config.github_app_id = Some(42);
        config.github_app_private_key = Some("pem".into());
        config.jira_client_id = Some("jira-client".into());
        config.jira_client_secret = Some("jira-secret".into());

        let platform = platform_client(&config).expect("platform client");
        let mcp = mcp_config(platform, &config);

        assert_eq!(mcp.github_app_id, Some(42));
        assert_eq!(mcp.github_app_private_key.as_deref(), Some("pem"));
        assert_eq!(mcp.jira_client_id.as_deref(), Some("jira-client"));
        assert_eq!(mcp.jira_client_secret.as_deref(), Some("jira-secret"));
        assert_eq!(mcp.encryption_key.as_deref(), Some("k"));
    }

    /// The console's "Back to Savvagent" link reads the platform's address from
    /// `/api/session` (`of-web`'s `platform_url`) when signed in, and from the
    /// discovery document's `authorization_servers` (`of-mcp`'s `platform_url`,
    /// handed to `ResourceServer` in `of_mcp::router`) when not. A signed-out
    /// visitor is sent somewhere else the day those two stop being one value.
    ///
    /// This checks the two configs agree; it cannot see what either crate then
    /// serves. `discovery_is_open_and_points_at_the_platform` (`tests/server.rs`)
    /// reads the served document's `authorization_servers[0]` back against the
    /// configured address, and `of-web`'s `console_login.rs` does the same for
    /// `/api/session`'s `platformUrl`.
    #[test]
    fn the_console_and_the_discovery_document_name_the_same_platform() {
        let config = Config::for_test();
        let platform = platform_client(&config).expect("platform client");
        let mcp = mcp_config(platform, &config);
        let web = web_config(&config);

        assert_eq!(mcp.platform_url, config.platform_url);
        assert_eq!(
            mcp.platform_url.trim_end_matches('/'),
            web.platform_url.trim_end_matches('/')
        );
    }

    #[test]
    fn api_prefixes_do_not_match_by_string_prefix_alone() {
        assert!(is_api_path("/api"));
        assert!(is_api_path("/api/orgs/acme"));
        assert!(is_api_path("/mcp"));
        assert!(is_api_path("/.well-known/oauth-protected-resource"));
        assert!(is_api_path("/platform/webhooks"));
        assert!(is_api_path("/webhooks/github"));
        assert!(is_api_path("/auth/callback"));
        assert!(is_api_path("/auth"));

        // An org slug or a page name that merely starts with the same letters
        // belongs to the console. `/apiary` is a legal org route and must not
        // answer a JSON 404 that the SPA never gets to render.
        assert!(!is_api_path("/apiary"));
        assert!(!is_api_path("/mcp-guide"));
        assert!(!is_api_path("/authors"));
        assert!(!is_api_path("/o/acme/queue"));
        assert!(!is_api_path("/"));
    }

    #[test]
    fn only_the_console_and_its_sign_in_move_to_the_canonical_host() {
        for moved in ["/", "/o/acme/queue", "/apiary", "/_app/x.js", "/auth/login"] {
            assert!(moves_to_canonical(moved), "{moved}");
        }
        for kept in [
            "/mcp",
            "/.well-known/oauth-protected-resource",
            "/api/orgs/acme/repos",
            "/api/openapi.json",
            "/webhooks/github",
            "/platform/webhooks",
            "/auth/callback",
            "/auth/logout",
            "/auth/login/extra",
            "/oauth/authorize",
            "/healthz",
            "/readyz",
            "/readyz/x",
        ] {
            assert!(!moves_to_canonical(kept), "{kept}");
        }
        assert!(moves_to_canonical("/readyzzz"));
    }

    /// No routed path is ever redirected off a legacy host: every console API
    /// route, sign-in route and webhook receiver in `of-web`'s catalog, both of
    /// `of-mcp`'s, and the probes. A route added outside `API_PREFIXES` would
    /// otherwise answer a `308` to the canonical host on the old one, which a
    /// webhook sender or an agent will not follow. `/auth/login` is the one
    /// deliberate exception.
    #[test]
    fn no_routed_path_moves_to_the_canonical_host() {
        let routed = of_web::catalog::catalog()
            .into_iter()
            .map(|endpoint| endpoint.path)
            .chain(of_mcp::ROUTES)
            .chain(health::PATHS)
            .filter(|path| *path != "/auth/login")
            .collect::<Vec<_>>();
        assert!(routed.len() > 10, "the catalog came back nearly empty");
        for path in routed {
            assert!(!moves_to_canonical(path), "{path} would be redirected");
        }
        assert!(
            of_web::catalog::catalog()
                .iter()
                .any(|e| e.path == "/auth/login"),
            "the exception names a route that no longer exists"
        );
    }
}
