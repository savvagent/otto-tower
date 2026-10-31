//! The assembled application, exercised as one router.
//!
//! Everything below is about the seams *between* crates — the places a bug can
//! only exist once `of-web`, `of-mcp`, the health routes, and the console
//! bundle share an origin. Each crate's own behaviour is tested in that crate.

use axum::body::Body;
use of_core::watch::Watcher;
use of_server::config::LogFormat;
use of_server::Config;
use of_testkit::MockPlatform;
use otto_resource::Role;
use otto_tenant::Db;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

const PUBLIC: &str = "https://factory.test";
const RESOURCE: &str = of_testkit::RESOURCE_URI;
const PLATFORM: &str = "https://otto.test";

/// A 32-byte base64 key. Test material only.
const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

fn config(static_dir: &str) -> Config {
    Config {
        database_url: "postgres://unused".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_url: PUBLIC.into(),
        resource_uri: RESOURCE.into(),
        platform_url: PLATFORM.into(),
        introspection_secret: of_testkit::SECRET.into(),
        platform_webhook_secret: of_testkit::WEBHOOK_SECRET.into(),
        encryption_key: KEY.into(),
        github_app_id: None,
        github_app_private_key: None,
        github_app_webhook_secret: None,
        github_app_slug: None,
        github_app_client_id: None,
        github_app_client_secret: None,
        jira_client_id: None,
        jira_client_secret: None,
        console_client_id: None,
        upgrade_url: PLATFORM.into(),
        retired_set: vec![],
        extra_allowed_hosts: vec![],
        legacy_hosts: vec![],
        legacy_sunset: None,
        allowed_origins: vec![],
        static_dir: static_dir.into(),
        run_migrations: false,
        log_format: LogFormat::Text,
    }
}

/// A pool that will never connect. Port 1 refuses immediately rather than
/// hanging, so a test that wants a database failure gets one in milliseconds.
fn dead_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://localhost:1/nope")
        .expect("lazy pool")
}

/// A directory holding a stand-in `index.html`, removed on drop so a failing
/// assertion does not leave one behind.
struct Bundle(std::path::PathBuf);

impl Bundle {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("of-server-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.html"),
            "<!doctype html><title>console</title>",
        )
        .unwrap();
        Self(dir)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

async fn get(app: axum::Router, uri: &str) -> http::Response<Body> {
    app.oneshot(
        http::Request::builder()
            .uri(uri)
            .header("host", "factory.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn body_json(response: http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("body was not JSON")
}

async fn body_text(response: http::Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The whole router assembles. Axum panics on a route registered twice rather
/// than choosing, and without this test the panic is found by a deploy.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn the_whole_router_assembles(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();

    let _app = of_server::router(db, watcher.clone(), platform.client(), &config("web/build"))
        .expect("router");

    watcher.shutdown().await;
}

/// The protected-resource document answers without a credential and names the
/// *platform* as the authorization server. This is the whole of zero-install
/// onboarding: an agent given nothing but the MCP URL reads its way from here to
/// the platform and on to a token. This service serves no authorization-server
/// metadata of its own.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn discovery_is_open_and_points_at_the_platform(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(db, watcher.clone(), platform.client(), &config("web/build"))
        .expect("router");

    let resource = get(app.clone(), "/.well-known/oauth-protected-resource").await;
    assert_eq!(resource.status(), http::StatusCode::OK);
    let doc = body_json(resource).await;
    assert_eq!(doc["resource"], RESOURCE);
    // The console's signed-out "Back to Savvagent" link is built from exactly this
    // value. The signed-in side reads `/api/session`'s `platformUrl`:
    // `the_console_and_the_discovery_document_name_the_same_platform` pins the
    // config that feeds it, and `of-web`'s `console_login.rs` checks the served
    // value against the platform.
    assert_eq!(doc["authorization_servers"][0], PLATFORM);

    // Not an authorization server any more, and says so as JSON.
    for path in [
        "/.well-known/oauth-authorization-server",
        "/oauth/authorize",
        "/oauth/token",
        "/oauth/register",
        "/sso/callback",
    ] {
        let response = get(app.clone(), path).await;
        assert!(
            response.status() == http::StatusCode::NOT_FOUND
                || response.status() == http::StatusCode::OK,
            "{path}"
        );
        if path.starts_with("/oauth") || path.starts_with("/.well-known") {
            assert_eq!(response.status(), http::StatusCode::NOT_FOUND, "{path}");
            assert_eq!(body_json(response).await["error"], "not_found", "{path}");
        }
    }

    watcher.shutdown().await;
}

/// The `401` challenge points at a document served on this same origin. It is
/// built from `OF_PUBLIC_URL`, so a deployment that gets that wrong sends every
/// client somewhere that does not answer — and nothing else would notice.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn the_mcp_endpoint_points_an_unauthenticated_caller_at_this_origin(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(db, watcher.clone(), platform.client(), &config("web/build"))
        .expect("router");

    let response = app
        .clone()
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "factory.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    let challenge = response
        .headers()
        .get(http::header::WWW_AUTHENTICATE)
        .expect("no WWW-Authenticate header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.contains(&format!(
            r#"resource_metadata="{PUBLIC}/.well-known/oauth-protected-resource""#
        )),
        "{challenge}"
    );

    // And the document it names is reachable here, unauthenticated. A pointer
    // whose target lives on another origin is a closed loop for the client.
    let pointed_at = get(app, "/.well-known/oauth-protected-resource").await;
    assert_eq!(pointed_at.status(), http::StatusCode::OK);

    watcher.shutdown().await;
}

/// The three surfaces that need the platform, through the one assembled
/// router: a real token opens the agent surface and the console API, and the
/// platform's signed webhook reaches its handler. Proves the pieces share one
/// platform client and one database without any crate's own test seeing the seam.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn one_platform_token_works_on_every_surface(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(db, watcher.clone(), platform.client(), &config("web/build"))
        .expect("router");

    let (org, user) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    platform.add_org(org, "acme", "Acme");
    platform.add_member(org, user, "rob@acme.test", Role::Owner);
    let token = platform.issue(org, user, Role::Owner, of_core::scopes::KNOWN);

    // Console API.
    let console = app
        .clone()
        .oneshot(
            http::Request::builder()
                .uri("/api/orgs/acme/repos")
                .header("host", "factory.test")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(console.status(), http::StatusCode::OK);

    // Agent surface: past the bearer middleware (the empty body is then refused
    // by the MCP transport, which is not a 401 or a 503).
    let mcp = app
        .clone()
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "factory.test")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(mcp.status(), http::StatusCode::UNAUTHORIZED);
    assert_ne!(mcp.status(), http::StatusCode::SERVICE_UNAVAILABLE);

    // Platform webhook: unsigned is refused, signed is accepted.
    let unsigned = app
        .clone()
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/platform/webhooks")
                .header("host", "factory.test")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unsigned.status(), http::StatusCode::UNAUTHORIZED);

    let (header, body) = MockPlatform::webhook(
        "member.removed",
        serde_json::json!({ "org_id": org, "user_id": user }),
    );
    let signed = app
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/platform/webhooks")
                .header("host", "factory.test")
                .header("otto-signature", header)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(signed.status(), http::StatusCode::OK);

    watcher.shutdown().await;
}

/// Liveness must not depend on the database, or one database blip restarts
/// every replica at once. Asked against a pool that cannot connect.
#[tokio::test]
async fn liveness_does_not_touch_the_database() {
    let app = of_server::health::router(Db::from_pool(dead_pool()));

    let response = get(app, "/healthz").await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(body_json(response).await["status"], "ok");
}

/// Readiness must, or a replica that can serve nothing keeps taking traffic.
#[tokio::test]
async fn readiness_fails_when_the_database_is_unreachable() {
    let app = of_server::health::router(Db::from_pool(dead_pool()));

    let response = get(app, "/readyz").await;
    assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    let doc = body_json(response).await;
    assert_eq!(doc["status"], "unready");
    // The reason distinguishes a refused connection from a timeout, because
    // "not ready" alone sends whoever is paged to look at the wrong thing.
    assert!(doc["reason"].is_string(), "{doc}");
}

#[sqlx::test(migrations = "../of-core/migrations")]
async fn readiness_passes_against_a_live_database(pool: PgPool) {
    let db = Db::from_pool(pool);
    let app = of_server::health::router(db);

    let response = get(app, "/readyz").await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(body_json(response).await["status"], "ready");
}

/// The console's `index.html` answers any path the router does not, which is
/// what makes a hard refresh of a deep link work — and must *not* answer for an
/// API path, where `200 text/html` is the shape that makes an agent retry
/// forever against a route that will never exist.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn the_console_fallback_stops_at_the_api(pool: PgPool) {
    let bundle = Bundle::new("spa");
    let platform = MockPlatform::start().await;
    let db = Db::from_pool(pool.clone());
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(
        db,
        watcher.clone(),
        platform.client(),
        &config(bundle.path()),
    )
    .expect("router");

    // A console route the server has never heard of renders the app.
    let deep_link = get(app.clone(), "/o/acme/queue").await;
    assert_eq!(deep_link.status(), http::StatusCode::OK);
    assert!(body_text(deep_link)
        .await
        .contains("<title>console</title>"));

    // An API route that does not exist is a JSON 404 an agent can read.
    // Paths that match no route at all — `/api/orgs/nope` would be a `401`,
    // because it *is* a route and an unauthenticated caller is turned away
    // before anyone asks whether the org exists.
    for path in [
        "/api/no/such/thing",
        "/oauth/nope",
        // The console's own sign-in routes are API-shaped too: a mistyped one is
        // a JSON 404, never `index.html` served into a redirect.
        "/auth/nope",
        "/.well-known/nope",
        "/platform/nope",
    ] {
        let response = get(app.clone(), path).await;
        assert_eq!(
            response.status(),
            http::StatusCode::NOT_FOUND,
            "{path} should be a 404"
        );
        assert_eq!(body_json(response).await["error"], "not_found", "{path}");
    }

    // A console path that merely shares a prefix with an API one is still the
    // console: `apiary` is a legal org slug.
    for path in ["/apiary", "/authors"] {
        let lookalike = get(app.clone(), path).await;
        assert_eq!(lookalike.status(), http::StatusCode::OK, "{path}");
        assert!(body_text(lookalike)
            .await
            .contains("<title>console</title>"));
    }

    watcher.shutdown().await;
}

// ------------------------------------------------------------- legacy hosts

/// The deprecated host of the legacy tests. `PUBLIC` stays canonical.
const LEGACY: &str = "old.factory.test";
/// Where the Worker's subrequest lands: the origin's own name, in `Host`.
const ORIGIN: &str = "origin.factory.test";

fn legacy_config(static_dir: &str, sunset: Option<&str>) -> Config {
    let mut config = config(static_dir);
    config.legacy_hosts = vec![LEGACY.into()];
    config.extra_allowed_hosts = vec![ORIGIN.into()];
    config.legacy_sunset = sunset.map(|s| chrono::DateTime::parse_from_rfc3339(s).unwrap().into());
    config
}

/// A request as the Worker forwards it: `Host` is the origin, and the public
/// host the caller used survives only in `X-Forwarded-Host`.
fn via(method: &str, uri: &str, public_host: &str) -> http::request::Builder {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", ORIGIN)
        .header("x-forwarded-host", public_host)
}

async fn send(app: &axum::Router, req: http::Request<Body>) -> http::Response<Body> {
    app.clone().oneshot(req).await.unwrap()
}

fn header<'a>(response: &'a http::Response<Body>, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().expect("ascii header"))
}

fn initialize_body() -> Body {
    Body::from(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "legacy-test", "version": "1" },
            },
        })
        .to_string(),
    )
}

fn mcp_post(public_host: &str, token: &str, body: Body) -> http::Request<Body> {
    via("POST", "/mcp", public_host)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body)
        .unwrap()
}

/// Every surface answers a legacy-host request with the notice — the agent
/// surface's `401`, discovery, the console API, a webhook receiver, health, and
/// the SPA fallback (which answers with a redirect there, carrying the notice
/// too) — and the same requests on the canonical host carry none of it.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn every_response_through_a_legacy_host_carries_the_deprecation_notice(pool: PgPool) {
    let bundle = Bundle::new("legacy-notice");
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(
        db,
        watcher.clone(),
        platform.client(),
        &legacy_config(bundle.path(), Some("2027-01-31T00:00:00Z")),
    )
    .expect("router");

    for (method, path) in [
        ("POST", "/mcp"),
        ("GET", "/.well-known/oauth-protected-resource"),
        ("GET", "/api/orgs/acme/repos"),
        ("POST", "/platform/webhooks"),
        ("GET", "/healthz"),
        ("GET", "/o/acme/queue"),
    ] {
        let legacy = send(&app, via(method, path, LEGACY).body(Body::empty()).unwrap()).await;
        assert_eq!(header(&legacy, "deprecation"), Some("true"), "{path}");
        assert_eq!(
            header(&legacy, "link"),
            Some(format!(r#"<{PUBLIC}{path}>; rel="successor-version""#).as_str()),
            "{path}"
        );
        assert_eq!(
            header(&legacy, "sunset"),
            Some("Sun, 31 Jan 2027 00:00:00 GMT"),
            "{path}"
        );

        let canonical = send(
            &app,
            via(method, path, "factory.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        if path == "/o/acme/queue" {
            // The console moves to the canonical host rather than being
            // served on the old one (`the_console_on_a_legacy_host_moves_to_the_canonical_host`).
            assert_eq!(legacy.status(), http::StatusCode::PERMANENT_REDIRECT);
            assert_eq!(canonical.status(), http::StatusCode::OK);
        } else {
            assert_eq!(canonical.status(), legacy.status(), "{path}");
        }
        for name in ["deprecation", "link", "sunset"] {
            assert_eq!(header(&canonical, name), None, "{path} {name}");
        }
    }

    // With no proxy in front there is no X-Forwarded-Host, and Host is the
    // public host itself.
    let direct = send(
        &app,
        http::Request::builder()
            .uri("/healthz")
            .header("host", LEGACY)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(header(&direct, "deprecation"), Some("true"));

    // And X-Forwarded-Host wins over Host: a legacy Host forwarded for the
    // canonical name is canonical.
    let forwarded_canonical = send(
        &app,
        http::Request::builder()
            .uri("/healthz")
            .header("host", LEGACY)
            .header("x-forwarded-host", "factory.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(header(&forwarded_canonical, "deprecation"), None);

    watcher.shutdown().await;
}

/// A browser that opens the console on the old host is sent to the same page
/// on the canonical one, and so is the console's sign-in — answered on the old
/// host it would set its PKCE cookie where the platform's redirect back to
/// `OF_PUBLIC_URL` never brings the browser. Both with the Worker's
/// `X-Forwarded-Host` and with the old name in `Host` itself, which is how
/// production serves it (no Worker in front). Nothing an agent, a provider or
/// the platform calls is redirected.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn the_console_on_a_legacy_host_moves_to_the_canonical_host(pool: PgPool) {
    let bundle = Bundle::new("legacy-redirect");
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let mut config = legacy_config(bundle.path(), Some("2027-01-31T00:00:00Z"));
    config.console_client_id = Some("console-client".into());
    let app = of_server::router(db, watcher.clone(), platform.client(), &config).expect("router");

    let direct = |method: &str, uri: &str| {
        http::Request::builder()
            .method(method)
            .uri(uri)
            .header("host", LEGACY)
            .body(Body::empty())
            .unwrap()
    };
    let forwarded = |method: &str, uri: &str| via(method, uri, LEGACY).body(Body::empty()).unwrap();

    for build in [
        &direct as &dyn Fn(&str, &str) -> http::Request<Body>,
        &forwarded,
    ] {
        for (method, uri) in [
            ("GET", "/"),
            ("GET", "/o/acme/queue?status=pending"),
            ("GET", "/auth/login?org=acme&next=%2Fo%2Facme%2Fqueue"),
            ("GET", "/_app/immutable/entry.js"),
            ("POST", "/o/acme/queue"),
        ] {
            let response = send(&app, build(method, uri)).await;
            assert_eq!(
                response.status(),
                http::StatusCode::PERMANENT_REDIRECT,
                "{method} {uri}"
            );
            let target = format!("{PUBLIC}{uri}");
            assert_eq!(
                header(&response, "location"),
                Some(target.as_str()),
                "{uri}"
            );
            assert_eq!(header(&response, "deprecation"), Some("true"), "{uri}");
            assert_eq!(
                header(&response, "link"),
                Some(format!(r#"<{target}>; rel="successor-version""#).as_str()),
                "{uri}"
            );
            assert_eq!(
                header(&response, "sunset"),
                Some("Sun, 31 Jan 2027 00:00:00 GMT"),
                "{uri}"
            );
            assert!(
                header(&response, "set-cookie").is_none(),
                "{uri}: nothing may be set on the old host"
            );
        }

        // Never redirected: the agent surface, discovery, the console API, the
        // webhook receivers, the platform's webhooks, the rest of sign-in, and
        // the probes.
        for (method, uri) in [
            ("POST", "/mcp"),
            ("GET", "/.well-known/oauth-protected-resource"),
            ("GET", "/.well-known/oauth-protected-resource/mcp"),
            ("GET", "/api/orgs/acme/repos"),
            ("GET", "/api/openapi.json"),
            ("POST", "/webhooks/github"),
            ("POST", "/platform/webhooks"),
            ("GET", "/auth/callback?code=x&state=y"),
            ("GET", "/healthz"),
        ] {
            let response = send(&app, build(method, uri)).await;
            assert_ne!(
                response.status(),
                http::StatusCode::PERMANENT_REDIRECT,
                "{method} {uri} must be served on the old host"
            );
            assert!(
                !header(&response, "location").is_some_and(|l| l.starts_with(PUBLIC)),
                "{method} {uri} must not move to the canonical host"
            );
            assert_eq!(header(&response, "deprecation"), Some("true"), "{uri}");
            // Served in place, so the query (an authorization code, here) is
            // not copied into the Link: only the path is.
            let path = uri.split('?').next().unwrap();
            assert_eq!(
                header(&response, "link"),
                Some(format!(r#"<{PUBLIC}{path}>; rel="successor-version""#).as_str()),
                "{uri}"
            );
        }

        // A target that only looks like another host stays on the canonical
        // origin: the redirect is never to anywhere but `OF_PUBLIC_URL`.
        for uri in [
            "//evil.com",
            "//evil.com/x?y=1",
            "/\\evil.com",
            "/@evil.com",
        ] {
            if uri.parse::<http::Uri>().is_err() {
                continue; // not a request target `http` will carry at all
            }
            let response = send(&app, build("GET", uri)).await;
            assert_eq!(
                response.status(),
                http::StatusCode::PERMANENT_REDIRECT,
                "{uri}"
            );
            let location = header(&response, "location").unwrap();
            assert_eq!(location, format!("{PUBLIC}{uri}"), "{uri}");
            assert!(location.starts_with(&format!("{PUBLIC}/")), "{uri}");
        }
    }

    // On the canonical host the console and its sign-in are served, not moved.
    let canonical = send(
        &app,
        http::Request::builder()
            .uri("/o/acme/queue")
            .header("host", "factory.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(canonical.status(), http::StatusCode::OK);
    let login = send(
        &app,
        http::Request::builder()
            .uri("/auth/login?org=acme")
            .header("host", "factory.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_ne!(login.status(), http::StatusCode::PERMANENT_REDIRECT);
    assert!(
        header(&login, "location").is_some_and(|l| l.starts_with(PLATFORM)),
        "canonical sign-in goes to the platform: {:?}",
        header(&login, "location")
    );

    watcher.shutdown().await;
}

/// `Sunset` is announced only when a date was configured; the rest of the
/// notice does not depend on it.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn the_sunset_header_appears_only_when_a_date_is_configured(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(
        db,
        watcher.clone(),
        platform.client(),
        &legacy_config("web/build", None),
    )
    .expect("router");

    let response = send(
        &app,
        via("GET", "/healthz", LEGACY).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(header(&response, "deprecation"), Some("true"));
    assert!(header(&response, "link").is_some());
    assert_eq!(header(&response, "sunset"), None);

    watcher.shutdown().await;
}

/// Before the cutover `OF_LEGACY_HOSTS` is unset, and nothing changes: not even
/// a request that names the would-be legacy host gets a notice.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn without_legacy_hosts_nothing_is_deprecated(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(db, watcher.clone(), platform.client(), &config("web/build"))
        .expect("router");

    let response = send(
        &app,
        via("GET", "/healthz", LEGACY).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    for name in ["deprecation", "link", "sunset"] {
        assert_eq!(header(&response, name), None, "{name}");
    }

    watcher.shutdown().await;
}

/// RFC 9728 §3.3: a client checks that the metadata's `resource` is the URL it
/// started from. On the legacy host both the document and the `401` pointing
/// at it therefore name the legacy host, or a fresh sign-in on the old URL
/// fails until the sunset. The canonical host is unchanged.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn legacy_discovery_advertises_the_legacy_resource(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(
        db,
        watcher.clone(),
        platform.client(),
        &legacy_config("web/build", None),
    )
    .expect("router");

    let prm = |host: &'static str| {
        via("GET", "/.well-known/oauth-protected-resource", host)
            .body(Body::empty())
            .unwrap()
    };
    // Both answers depend on the host, and say so to any cache in between.
    let legacy_response = send(&app, prm(LEGACY)).await;
    assert_eq!(
        header(&legacy_response, "vary"),
        Some("X-Forwarded-Host, Host")
    );
    let legacy_doc = body_json(legacy_response).await;
    assert_eq!(legacy_doc["resource"], format!("https://{LEGACY}/mcp"));
    assert_eq!(legacy_doc["authorization_servers"][0], PLATFORM);
    let canonical_doc = body_json(send(&app, prm("factory.test")).await).await;
    assert_eq!(canonical_doc["resource"], RESOURCE);

    let challenge = |response: &http::Response<Body>| {
        header(response, "www-authenticate")
            .expect("no WWW-Authenticate")
            .to_string()
    };
    let legacy_401 = send(
        &app,
        via("POST", "/mcp", LEGACY).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(legacy_401.status(), http::StatusCode::UNAUTHORIZED);
    assert_eq!(header(&legacy_401, "vary"), Some("X-Forwarded-Host, Host"));
    assert!(
        challenge(&legacy_401).contains(&format!(
            r#"resource_metadata="https://{LEGACY}/.well-known/oauth-protected-resource""#
        )),
        "{}",
        challenge(&legacy_401)
    );
    let canonical_401 = send(
        &app,
        via("POST", "/mcp", "factory.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        challenge(&canonical_401).contains(&format!(
            r#"resource_metadata="{PUBLIC}/.well-known/oauth-protected-resource""#
        )),
        "{}",
        challenge(&canonical_401)
    );

    watcher.shutdown().await;
}

/// `initialize` through the legacy host opens its instructions with the
/// deprecation paragraph — the new URL and the sunset date — and through the
/// canonical host it does not.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn initialize_over_a_legacy_host_opens_with_the_deprecation_paragraph(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(
        db,
        watcher.clone(),
        platform.client(),
        &legacy_config("web/build", Some("2027-01-31T00:00:00Z")),
    )
    .expect("router");
    let (org, user) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let token = platform.issue(org, user, Role::Owner, of_core::scopes::KNOWN);

    let instructions = |response: serde_json::Value| {
        response["result"]["instructions"]
            .as_str()
            .unwrap_or_else(|| panic!("no instructions: {response}"))
            .to_string()
    };

    let legacy = send(&app, mcp_post(LEGACY, &token, initialize_body())).await;
    assert_eq!(legacy.status(), http::StatusCode::OK);
    assert_eq!(header(&legacy, "deprecation"), Some("true"));
    let text = instructions(body_json(legacy).await);
    let notice = text.split("\n\n").next().unwrap();
    assert!(notice.starts_with("DEPRECATED"), "{text}");
    assert!(notice.contains(RESOURCE), "names the new URL: {notice}");
    assert!(
        notice.contains("2027-01-31T00:00:00Z"),
        "names the sunset: {notice}"
    );
    assert!(text.contains("Call whoami"), "the guidance follows: {text}");

    let canonical = send(&app, mcp_post("factory.test", &token, initialize_body())).await;
    assert_eq!(canonical.status(), http::StatusCode::OK);
    let text = instructions(body_json(canonical).await);
    assert!(!text.contains("DEPRECATED"), "{text}");
    assert!(text.starts_with("Savvagent Tower coordinates"), "{text}");

    watcher.shutdown().await;
}

/// The invariant the whole feature rests on: the host selects what is
/// *advertised*, never what is *accepted*. Over the legacy host a token minted
/// for `OF_RESOURCE_URI` works, and a token minted for the legacy host's own
/// URI does not — this service asks the platform about every token as
/// `OF_RESOURCE_URI` and nothing else, so no header a caller sends can choose
/// the audience. (In production the platform's alias makes a token requested
/// for the old URI introspect as this resource; that is the platform's to do,
/// not something this binary infers from a Host.)
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_legacy_host_never_chooses_the_audience(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let platform = MockPlatform::start().await;
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = of_server::router(
        db,
        watcher.clone(),
        platform.client(),
        &legacy_config("web/build", None),
    )
    .expect("router");
    let (org, user) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

    let canonical_token = platform.issue(org, user, Role::Owner, of_core::scopes::KNOWN);
    let ok = send(&app, mcp_post(LEGACY, &canonical_token, initialize_body())).await;
    assert_eq!(ok.status(), http::StatusCode::OK);

    let legacy_uri = format!("https://{LEGACY}/mcp");
    let legacy_token =
        platform.issue_for(&legacy_uri, org, user, Role::Owner, of_core::scopes::KNOWN);
    for host in [LEGACY, "factory.test"] {
        let refused = send(&app, mcp_post(host, &legacy_token, initialize_body())).await;
        assert_eq!(refused.status(), http::StatusCode::UNAUTHORIZED, "{host}");
    }

    watcher.shutdown().await;
}

/// A shipper stalled on its meter declaration is visible on `/readyz`, without
/// failing readiness: the replica still serves, and nothing is lost.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn readiness_reports_a_stalled_meter_declaration(pool: PgPool) {
    use of_billing::outbox::{self, ShipperConfig, ShipperStatus};
    use of_testkit::MeterMode;
    use std::time::Duration;

    let platform = MockPlatform::start().await;
    platform.set_meter_mode(MeterMode::ServiceNotSet);
    let db = Db::from_pool(pool.clone());
    sqlx::query(
        "INSERT INTO usage_outbox (org_id, tool, billable, meter, quantity) \
         VALUES ($1, 'complete_job', true, 'otto-tower/jobs_completed', 1)",
    )
    .bind(uuid::Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();

    let status = ShipperStatus::default();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let cfg = ShipperConfig {
        base_backoff: Duration::from_millis(20),
        declare_max_backoff: Duration::from_millis(100),
        poll: Duration::from_millis(10),
        ..ShipperConfig::default()
    };
    let task = tokio::spawn(outbox::run_with_status(
        db.clone(),
        platform.client(),
        cfg,
        rx,
        status.clone(),
    ));
    let app = of_server::health::router_with_shipper(db, status);

    let stalled = get(app.clone(), "/readyz").await;
    assert_eq!(stalled.status(), http::StatusCode::OK);
    let doc = body_json(stalled).await;
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["usage"]["meters_declared"], false, "{doc}");
    // The queue depth is a cross-tenant figure and this route is unauthenticated.
    assert!(doc["usage"].get("rows_waiting").is_none(), "{doc}");

    platform.set_meter_mode(MeterMode::Accept);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let doc = body_json(get(app.clone(), "/readyz").await).await;
        if doc["usage"]["meters_declared"] == true {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never recovered: {doc}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    stop.send(true).unwrap();
    task.await.unwrap();
}

/// Without a shipper beside it the probe says nothing about usage.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn readiness_without_a_shipper_has_no_usage_detail(pool: PgPool) {
    let app = of_server::health::router(Db::from_pool(pool));
    let doc = body_json(get(app, "/readyz").await).await;
    assert!(doc.get("usage").is_none(), "{doc}");
}
