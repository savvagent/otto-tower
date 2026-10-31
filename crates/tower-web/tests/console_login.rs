//! The console's own sign-in: OAuth authorization code + PKCE against the
//! platform, and the cookie session it leaves behind.
//!
//! The platform is `tower-testkit`'s mock, speaking the real wire format over real
//! HTTP, including the parts that make the flow dangerous: authorization codes
//! are single use and PKCE-bound, and a refresh token is **consumed by use**,
//! with reuse of a consumed one revoking the whole login. The tests drive the
//! browser's side by hand (`sign_in` plays what the platform's `/oauth/authorize`
//! page does between `/auth/login` and `/auth/callback`).

mod common;

use std::collections::HashMap;

use common::{
    harness, harness_with_login, Call, Harness, Reply, CONSOLE_CLIENT, PUBLIC_URL, RESOURCE,
};
use http::StatusCode;
use savvagent_resource::Role;
use sqlx::PgPool;
use tower_core::console_sessions::hash_cookie;
use uuid::Uuid;

const ALL: &[&str] = tower_core::scopes::KNOWN;
const SESSION: &str = "__Host-of_session";
const OAUTH: &str = "__Host-of_oauth";

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn params(location: &str) -> HashMap<String, String> {
    url::Url::parse(location)
        .expect("an absolute URL")
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn cookie(value: &str) -> String {
    format!("{SESSION}={value}")
}

/// A fixture org with an owner, ready to sign in.
struct World {
    org: Uuid,
    owner: Uuid,
}

fn world(h: &Harness, slug: &str) -> World {
    let (org, owner) = (Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(org, slug, slug);
    h.platform.add_member(
        org,
        owner,
        &format!("{slug}-owner@test.example"),
        Role::Owner,
    );
    World { org, owner }
}

/// `GET /auth/login`, returning the reply, the sealed oauth cookie's value, and
/// the authorize URL's parameters.
async fn begin(h: &Harness, query: &str) -> (Reply, String, HashMap<String, String>) {
    let reply = Call::get(format!("/auth/login{query}"))
        .send(&h.router)
        .await;
    reply.expect(StatusCode::SEE_OTHER);
    let oauth = reply.cookie_value(OAUTH).expect("an oauth cookie");
    let p = params(reply.location().expect("a Location"));
    (reply, oauth, p)
}

/// A whole sign-in as `user` in `org`. Returns the callback's reply.
async fn sign_in(h: &Harness, org: Uuid, user: Uuid, role: Role, login_query: &str) -> Reply {
    let (_, oauth, p) = begin(h, login_query).await;
    let code = h.platform.issue_code(
        CONSOLE_CLIENT,
        &p["redirect_uri"],
        &p["code_challenge"],
        org,
        user,
        role,
        ALL,
    );
    callback(
        h,
        &format!("code={code}&state={}", p["state"]),
        Some(&oauth),
    )
    .await
}

async fn callback(h: &Harness, query: &str, oauth_cookie: Option<&str>) -> Reply {
    let mut call = Call::get(format!("/auth/callback?{query}"));
    if let Some(oauth) = oauth_cookie {
        call = call.header("cookie", format!("{OAUTH}={oauth}"));
    }
    call.send(&h.router).await
}

/// Sign in as the world's owner and return the session cookie's value.
async fn session_for(h: &Harness, w: &World) -> String {
    let reply = sign_in(h, w.org, w.owner, Role::Owner, "").await;
    reply.expect(StatusCode::SEE_OTHER);
    reply.cookie_value(SESSION).expect("a session cookie")
}

async fn rows(h: &Harness) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM console_sessions")
        .fetch_one(h.db.pool())
        .await
        .unwrap()
}

async fn get_with(h: &Harness, uri: &str, session: &str) -> Reply {
    Call::get(uri)
        .header("cookie", cookie(session))
        .send(&h.router)
        .await
}

// ------------------------------------------------------------------- /auth/login

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn login_sends_the_browser_to_the_platform_with_pkce_and_state(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let (reply, oauth, p) = begin(&h, "?org=acme&next=%2Fo%2Facme%2Fjobs").await;

    let location = reply.location().unwrap();
    assert!(
        location.starts_with(&format!("{}/oauth/authorize?", h.platform.url)),
        "{location}"
    );
    assert_eq!(p["response_type"], "code");
    assert_eq!(p["client_id"], CONSOLE_CLIENT);
    assert_eq!(p["redirect_uri"], format!("{PUBLIC_URL}/auth/callback"));
    assert_eq!(p["code_challenge_method"], "S256");
    assert_eq!(
        p["code_challenge"].len(),
        43,
        "S256 of a verifier, base64url"
    );
    assert_eq!(p["state"].len(), 43);
    assert_eq!(p["resource"], RESOURCE);
    assert_eq!(p["org_hint"], "acme");
    let scopes: Vec<&str> = p["scope"].split(' ').collect();
    for scope in [
        "jobs:read",
        "repos:read",
        "repos:write",
        "trackers",
        "org:admin",
    ] {
        assert!(scopes.contains(&scope), "missing {scope} in {scopes:?}");
    }

    // The state and the verifier travel in a sealed cookie, never in the clear.
    assert!(!oauth.contains(&p["state"]));
    let header = reply.set_cookie(OAUTH).unwrap();
    for attr in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=600",
    ] {
        assert!(header.contains(attr), "{attr} missing from {header}");
    }
    assert!(!header.contains("Domain"), "a __Host- cookie has no Domain");
    assert_eq!(reply.headers[http::header::CACHE_CONTROL], "no-store");
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn every_login_has_its_own_state_and_challenge(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let (_, _, a) = begin(&h, "").await;
    let (_, _, b) = begin(&h, "").await;
    assert_ne!(a["state"], b["state"]);
    assert_ne!(a["code_challenge"], b["code_challenge"]);
    assert!(!a.contains_key("org_hint"), "no org asked for, none hinted");
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn login_refuses_an_org_that_is_not_a_slug(pool: PgPool) {
    let h = harness_with_login(pool).await;
    for bad in ["a%20b", "a%2Fb", "a%26org_hint%3Dx", "%3Cscript%3E"] {
        let reply = Call::get(format!("/auth/login?org={bad}"))
            .send(&h.router)
            .await;
        reply.expect(StatusCode::BAD_REQUEST);
        assert!(reply.set_cookie(OAUTH).is_none());
    }
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn without_a_console_client_id_login_is_503_and_the_server_still_serves(pool: PgPool) {
    let h = harness(pool).await;

    for uri in ["/auth/login", "/auth/callback?code=x&state=y"] {
        let reply = Call::get(uri).send(&h.router).await;
        reply.expect(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.error_code(), Some("console_login_disabled"));
    }
    // Bearer tokens are untouched by it.
    let w = world(&h, "acme");
    let token = h.platform.issue(w.org, w.owner, Role::Owner, ALL);
    Call::get("/api/orgs/acme/repos")
        .with_session(&token)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
}

// ---------------------------------------------------------------- /auth/callback

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn callback_creates_an_encrypted_session_and_sets_the_cookie(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let reply = sign_in(&h, w.org, w.owner, Role::Owner, "?org=acme").await;

    reply.expect(StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), Some("/o/acme"));
    let cookie_value = reply.cookie_value(SESSION).expect("session cookie");
    let header = reply.set_cookie(SESSION).unwrap();
    for attr in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/", "Max-Age="] {
        assert!(header.contains(attr), "{attr} missing from {header}");
    }
    assert!(!header.contains("Domain"));
    // The half-finished login is cleared.
    assert!(reply.set_cookie(OAUTH).unwrap().contains("Max-Age=0"));

    assert_eq!(rows(&h).await, 1);
    let row = tower_core::console_sessions::find(&h.db, &hash_cookie(&cookie_value))
        .await
        .unwrap()
        .expect("the cookie opens the session");
    assert_eq!(row.org_id.as_uuid(), w.org);
    assert_eq!(row.user_id.as_uuid(), w.owner);
    let access = row.access_token(&h.cipher).unwrap();
    assert!(access.starts_with("sav_at_"));
    assert!(row.refresh_token(&h.cipher).unwrap().starts_with("sav_rt_"));
    assert!(row.scopes.contains(&"org:admin".to_string()));
    assert!(row.expires_at > chrono::Utc::now() + chrono::Duration::days(29));

    // Neither the cookie nor a token is in the table in the clear.
    let dump: String = sqlx::query_scalar(
        "SELECT access_token_enc || refresh_token_enc || encode(id_hash, 'hex') \
         FROM console_sessions",
    )
    .fetch_one(h.db.pool())
    .await
    .unwrap();
    assert!(
        !dump.contains(&cookie_value) && !dump.contains("sav_at_") && !dump.contains("sav_rt_")
    );
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_mismatched_state_is_rejected_before_the_code_is_redeemed(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let (_, oauth, p) = begin(&h, "").await;
    let code = h.platform.issue_code(
        CONSOLE_CLIENT,
        &p["redirect_uri"],
        &p["code_challenge"],
        w.org,
        w.owner,
        Role::Owner,
        ALL,
    );

    let reply = callback(
        &h,
        &format!("code={code}&state=not-the-state"),
        Some(&oauth),
    )
    .await;
    reply.expect(StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), Some("/?login_error=invalid_state"));
    assert!(reply.cookie_value(SESSION).is_none());
    assert_eq!(h.platform.code_exchanges(), 0, "the code must not be spent");
    assert_eq!(rows(&h).await, 0);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_callback_without_this_browsers_oauth_cookie_is_rejected(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let (_, oauth, p) = begin(&h, "").await;
    let code = h.platform.issue_code(
        CONSOLE_CLIENT,
        &p["redirect_uri"],
        &p["code_challenge"],
        w.org,
        w.owner,
        Role::Owner,
        ALL,
    );
    let query = format!("code={code}&state={}", p["state"]);

    // No cookie: a login someone else started, fed to this browser.
    let reply = callback(&h, &query, None).await;
    assert_eq!(reply.location(), Some("/?login_error=invalid_state"));

    // A cookie that was not sealed by us, or was tampered with.
    let mut tampered = oauth.clone();
    tampered.replace_range(10..11, if &oauth[10..11] == "A" { "B" } else { "A" });
    for bad in ["garbage", tampered.as_str()] {
        let reply = callback(&h, &query, Some(bad)).await;
        assert_eq!(
            reply.location(),
            Some("/?login_error=invalid_state"),
            "{bad}"
        );
    }

    assert_eq!(h.platform.code_exchanges(), 0);
    assert_eq!(rows(&h).await, 0);
    // And the genuine article still works afterwards.
    callback(&h, &query, Some(&oauth))
        .await
        .expect(StatusCode::SEE_OTHER);
    assert_eq!(rows(&h).await, 1);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_login_error_from_the_platform_comes_back_as_a_fixed_code(pool: PgPool) {
    let h = harness_with_login(pool).await;
    for (error, expected) in [
        ("access_denied", "access_denied"),
        ("server_error", "platform_unavailable"),
        ("temporarily_unavailable", "platform_unavailable"),
        // Nothing attacker-chosen is reflected.
        ("%3Cscript%3Ealert(1)%3C%2Fscript%3E", "failed"),
    ] {
        let reply = callback(&h, &format!("error={error}&state=x"), None).await;
        reply.expect(StatusCode::SEE_OTHER);
        assert_eq!(
            reply.location(),
            Some(format!("/?login_error={expected}").as_str())
        );
        assert!(reply.set_cookie(OAUTH).unwrap().contains("Max-Age=0"));
        assert!(reply.cookie_value(SESSION).is_none());
    }
    assert_eq!(rows(&h).await, 0);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_replayed_callback_signs_nobody_in(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let (_, oauth, p) = begin(&h, "").await;
    let code = h.platform.issue_code(
        CONSOLE_CLIENT,
        &p["redirect_uri"],
        &p["code_challenge"],
        w.org,
        w.owner,
        Role::Owner,
        ALL,
    );
    let query = format!("code={code}&state={}", p["state"]);
    callback(&h, &query, Some(&oauth))
        .await
        .expect(StatusCode::SEE_OTHER);
    assert_eq!(rows(&h).await, 1);

    let again = callback(&h, &query, Some(&oauth)).await;
    assert_eq!(again.location(), Some("/?login_error=failed"));
    assert!(again.cookie_value(SESSION).is_none());
    assert_eq!(rows(&h).await, 1);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_platform_outage_during_callback_is_reported_not_swallowed(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let (_, oauth, p) = begin(&h, "").await;
    let code = h.platform.issue_code(
        CONSOLE_CLIENT,
        &p["redirect_uri"],
        &p["code_challenge"],
        w.org,
        w.owner,
        Role::Owner,
        ALL,
    );
    h.platform.set_down(true);
    let reply = callback(
        &h,
        &format!("code={code}&state={}", p["state"]),
        Some(&oauth),
    )
    .await;
    assert_eq!(reply.location(), Some("/?login_error=platform_unavailable"));
    assert_eq!(rows(&h).await, 0);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn next_is_honoured_only_when_it_is_a_same_origin_path(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");

    let landing = |next: &'static str| {
        let (h, w) = (&h, &w);
        async move {
            let query = format!("?org=acme&next={}", enc(next));
            let reply = sign_in(h, w.org, w.owner, Role::Owner, &query).await;
            reply.expect(StatusCode::SEE_OTHER);
            reply.location().unwrap().to_string()
        }
    };

    assert_eq!(
        landing("/o/acme/jobs?status=pending").await,
        "/o/acme/jobs?status=pending"
    );
    for evil in [
        "//evil.test/x",
        "/\\evil.test",
        "https://evil.test/",
        "javascript:alert(1)",
        "/\t/evil.test",
        "/auth/login",
    ] {
        assert_eq!(landing(evil).await, "/o/acme", "next={evil:?}");
    }
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn logging_in_purges_sessions_that_have_expired(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let old = session_for(&h, &w).await;
    sqlx::query("UPDATE console_sessions SET expires_at = now() - interval '1 day'")
        .execute(h.db.pool())
        .await
        .unwrap();
    get_with(&h, "/api/orgs/acme/repos", &old)
        .await
        .expect(StatusCode::UNAUTHORIZED);

    session_for(&h, &w).await;
    assert_eq!(rows(&h).await, 1, "only the new session remains");
}

// ------------------------------------------------------ authenticating with it

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_cookie_session_authenticates_org_routes_and_reports_who_it_is(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let session = session_for(&h, &w).await;

    get_with(&h, "/api/orgs/acme/repos", &session)
        .await
        .expect(StatusCode::OK);
    // By id as well as by slug.
    get_with(&h, &format!("/api/orgs/{}/repos", w.org), &session)
        .await
        .expect(StatusCode::OK);

    let me = get_with(&h, "/api/session", &session).await;
    me.expect(StatusCode::OK);
    assert_eq!(me.body["user"]["id"], w.owner.to_string());
    assert_eq!(me.body["user"]["email"], "acme-owner@test.example");
    assert_eq!(me.body["org"]["slug"], "acme");
    assert_eq!(me.body["org"]["id"], w.org.to_string());
    assert_eq!(me.body["org"]["plan"], "free");
    assert_eq!(me.body["role"], "owner");
    assert_eq!(me.body["platformUrl"], h.platform.url);
    assert!(me.body["scopes"]
        .as_array()
        .unwrap()
        .contains(&"repos:read".into()));
    assert_eq!(me.headers[http::header::CACHE_CONTROL], "no-store");
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn no_session_is_unauthenticated(pool: PgPool) {
    let h = harness_with_login(pool).await;
    Call::get("/api/session")
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    // A cookie that opens nothing.
    for junk in ["short", &"A".repeat(43)] {
        let reply = get_with(&h, "/api/session", junk).await;
        reply.expect(StatusCode::UNAUTHORIZED);
        assert_eq!(reply.error_code(), Some("unauthenticated"));
    }
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_bearer_token_decides_the_request_even_when_it_is_bad(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let session = session_for(&h, &w).await;

    // A valid cookie does not rescue an invalid bearer...
    Call::get("/api/orgs/acme/repos")
        .with_session(&tower_testkit::new_token())
        .header("cookie", cookie(&session))
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    // ...and the bearer path is the unchanged one: another org is a 404.
    let token = h.platform.issue(w.org, w.owner, Role::Owner, ALL);
    Call::get("/api/orgs/globex/repos")
        .with_session(&token)
        .header("cookie", cookie(&session))
        .send(&h.router)
        .await
        .expect(StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_session_for_another_org_asks_to_sign_in_again_rather_than_404(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let other = world(&h, "globex");
    let session = session_for(&h, &w).await;

    for uri in [
        "/api/orgs/globex/repos",
        &format!("/api/orgs/{}/jobs", other.org),
    ] {
        let reply = get_with(&h, uri, &session).await;
        reply.expect(StatusCode::UNAUTHORIZED);
        assert_eq!(reply.error_code(), Some("org_session_mismatch"), "{uri}");
    }
    // The same call for an org that does not exist at all answers the same, so
    // the response says nothing about which orgs exist.
    let reply = get_with(&h, "/api/orgs/nowhere/repos", &session).await;
    assert_eq!(reply.error_code(), Some("org_session_mismatch"));
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_scope_the_platform_did_not_grant_is_forbidden_to_a_cookie_session_too(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let member = Uuid::new_v4();
    h.platform
        .add_member(w.org, member, "m@test.example", Role::Member);

    // The platform drops org:admin for a non-admin; the audit route needs it.
    let (_, oauth, p) = begin(&h, "").await;
    let code = h.platform.issue_code(
        CONSOLE_CLIENT,
        &p["redirect_uri"],
        &p["code_challenge"],
        w.org,
        member,
        Role::Member,
        &["jobs:read", "repos:read"],
    );
    let reply = callback(
        &h,
        &format!("code={code}&state={}", p["state"]),
        Some(&oauth),
    )
    .await;
    let session = reply.cookie_value(SESSION).unwrap();

    get_with(&h, "/api/orgs/acme/repos", &session)
        .await
        .expect(StatusCode::OK);
    get_with(&h, "/api/orgs/acme/audit", &session)
        .await
        .expect(StatusCode::FORBIDDEN);
    let me = get_with(&h, "/api/session", &session).await;
    assert_eq!(me.body["role"], "member");
}

// ------------------------------------------------------------------ refreshing

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn concurrent_requests_near_expiry_refresh_exactly_once(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");

    // Sign in with an access token that is already inside the refresh skew...
    h.platform.set_access_ttl_secs(30);
    let session = session_for(&h, &w).await;
    // ...then make the refresh slow and its result long-lived, so every request
    // below overlaps the one that refreshes.
    h.platform.set_access_ttl_secs(3600);
    h.platform.set_refresh_delay_ms(300);

    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let router = h.router.clone();
            let session = session.clone();
            tokio::spawn(async move {
                Call::get("/api/orgs/acme/repos")
                    .header("cookie", cookie(&session))
                    .send(&router)
                    .await
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().expect(StatusCode::OK);
    }

    assert_eq!(
        h.platform.refresh_calls(),
        1,
        "the refresh token is single use"
    );
    assert!(
        !h.platform.any_family_revoked(),
        "a second refresh would have revoked the whole login"
    );
    // The rotated pair is what is stored, and later requests need no refresh.
    get_with(&h, "/api/orgs/acme/repos", &session)
        .await
        .expect(StatusCode::OK);
    assert_eq!(h.platform.refresh_calls(), 1);
    assert_eq!(rows(&h).await, 1);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_refresh_stores_the_rotated_pair_encrypted(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    h.platform.set_access_ttl_secs(30);
    let session = session_for(&h, &w).await;
    let id_hash = hash_cookie(&session);
    let before = tower_core::console_sessions::find(&h.db, &id_hash)
        .await
        .unwrap()
        .unwrap();
    h.platform.set_access_ttl_secs(3600);

    get_with(&h, "/api/orgs/acme/repos", &session)
        .await
        .expect(StatusCode::OK);

    let after = tower_core::console_sessions::find(&h.db, &id_hash)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        before.access_token(&h.cipher).unwrap(),
        after.access_token(&h.cipher).unwrap()
    );
    assert_ne!(
        before.refresh_token(&h.cipher).unwrap(),
        after.refresh_token(&h.cipher).unwrap()
    );
    assert!(after.access_expires_at > chrono::Utc::now() + chrono::Duration::minutes(50));
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_refresh_the_platform_refuses_drops_the_session(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    h.platform.set_access_ttl_secs(30);
    let session = session_for(&h, &w).await;
    h.platform.revoke_all_logins();

    let reply = get_with(&h, "/api/orgs/acme/repos", &session).await;
    reply.expect(StatusCode::UNAUTHORIZED);
    assert_eq!(reply.error_code(), Some("unauthenticated"));
    assert_eq!(rows(&h).await, 0, "a dead session is not kept");
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_platform_outage_during_refresh_is_503_and_keeps_the_session(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    h.platform.set_access_ttl_secs(30);
    let session = session_for(&h, &w).await;
    h.platform.set_access_ttl_secs(3600);

    h.platform.set_down(true);
    let reply = get_with(&h, "/api/orgs/acme/repos", &session).await;
    reply.expect(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.error_code(), Some("platform_unavailable"));
    assert_eq!(rows(&h).await, 1, "a 503 must not sign anyone out");

    h.platform.set_down(false);
    get_with(&h, "/api/orgs/acme/repos", &session)
        .await
        .expect(StatusCode::OK);
}

/// The platform's introspection cache would vouch for a revoked token for up to
/// a minute, so this uses a client with no cache to see the session's own
/// behaviour: once the platform stops honouring the token, the session ends.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_token_the_platform_no_longer_honours_ends_the_session(pool: PgPool) {
    let platform = tower_testkit::MockPlatform::start().await;
    let mut cfg =
        savvagent_resource::ClientConfig::new(&platform.url, RESOURCE, tower_testkit::SECRET);
    cfg.introspection_ttl = std::time::Duration::ZERO;
    cfg.negative_ttl = std::time::Duration::ZERO;
    let mut config = tower_web::Config::new(
        PUBLIC_URL,
        RESOURCE,
        &platform.url,
        tower_testkit::WEBHOOK_SECRET,
    );
    config.console_client_id = Some(CONSOLE_CLIENT.into());
    let db = savvagent_tenant::Db::from_pool(pool);
    let router = tower_web::router(tower_web::AppState::new(
        db.clone(),
        common::cipher(),
        platform.client_with(cfg),
        config,
    ));
    let h = Harness {
        db,
        router,
        cipher: common::cipher(),
        platform,
    };

    let w = world(&h, "acme");
    let session = session_for(&h, &w).await;
    get_with(&h, "/api/orgs/acme/repos", &session)
        .await
        .expect(StatusCode::OK);

    h.platform.revoke_all_logins();
    get_with(&h, "/api/orgs/acme/repos", &session)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    assert_eq!(
        rows(&h).await,
        0,
        "nothing will revive it, so it is dropped"
    );
}

// ------------------------------------------------------------------------ CSRF

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_cross_origin_write_with_the_cookie_is_refused_and_a_bearer_write_is_not(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let session = session_for(&h, &w).await;
    let body = |slug: &str| serde_json::json!({ "slug": slug });
    let post = |slug: &'static str| {
        Call::post("/api/orgs/acme/repos")
            .json(body(slug))
            .header("cookie", cookie(&session))
    };

    for origin in [
        "https://evil.test",
        // A same-site sibling: SameSite=Lax does not stop it.
        "https://otto.console.otto-tower.test",
        "null",
    ] {
        let reply = post("nope").header("origin", origin).send(&h.router).await;
        reply.expect(StatusCode::FORBIDDEN);
        assert_eq!(reply.error_code(), Some("cross_site_request"), "{origin}");
    }
    // No Origin, no fetch metadata, no Referer: nothing proves where it came from.
    post("nope")
        .send(&h.router)
        .await
        .expect(StatusCode::FORBIDDEN);
    post("nope")
        .header("sec-fetch-site", "same-site")
        .send(&h.router)
        .await
        .expect(StatusCode::FORBIDDEN);

    // From this site: allowed, by any of the three proofs.
    post("one")
        .header("origin", PUBLIC_URL)
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);
    post("two")
        .header("sec-fetch-site", "same-origin")
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);
    post("three")
        .header("referer", format!("{PUBLIC_URL}/o/acme"))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    // A bearer token is not an ambient credential: exempt, whatever the Origin.
    let token = h.platform.issue(w.org, w.owner, Role::Owner, ALL);
    Call::post("/api/orgs/acme/repos")
        .json(body("four"))
        .with_session(&token)
        .header("origin", "https://evil.test")
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    // None of the refused attempts did anything.
    let repos = Call::get("/api/orgs/acme/repos")
        .with_session(&token)
        .send(&h.router)
        .await;
    let slugs: Vec<&str> = repos
        .body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["slug"].as_str())
        .collect();
    assert!(!slugs.contains(&"nope"), "{slugs:?}");
    assert_eq!(slugs.len(), 4);
}

// ---------------------------------------------------------------------- logout

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn logout_revokes_at_the_platform_deletes_the_session_and_clears_the_cookie(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let session = session_for(&h, &w).await;
    let refresh = tower_core::console_sessions::find(&h.db, &hash_cookie(&session))
        .await
        .unwrap()
        .unwrap()
        .refresh_token(&h.cipher)
        .unwrap();

    // Cross-origin logout is refused like any other cookie write.
    Call::post("/auth/logout")
        .header("cookie", cookie(&session))
        .header("origin", "https://evil.test")
        .send(&h.router)
        .await
        .expect(StatusCode::FORBIDDEN);
    assert_eq!(rows(&h).await, 1);

    let reply = Call::post("/auth/logout")
        .header("cookie", cookie(&session))
        .header("origin", PUBLIC_URL)
        .send(&h.router)
        .await;
    reply.expect(StatusCode::NO_CONTENT);
    assert!(reply.set_cookie(SESSION).unwrap().contains("Max-Age=0"));
    assert_eq!(h.platform.revoked_refresh_tokens(), vec![refresh]);
    assert_eq!(rows(&h).await, 0);

    get_with(&h, "/api/session", &session)
        .await
        .expect(StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn logout_works_while_the_platform_is_down_and_with_no_session(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let session = session_for(&h, &w).await;

    h.platform.set_down(true);
    Call::post("/auth/logout")
        .header("cookie", cookie(&session))
        .header("origin", PUBLIC_URL)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(&h).await,
        0,
        "signing out must not depend on the platform"
    );

    // Idempotent, and harmless with nothing to sign out of.
    Call::post("/auth/logout")
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);
}

// ------------------------------------------------------------------- lifecycle

async fn webhook(h: &Harness, kind: &str, data: serde_json::Value) -> Reply {
    let (header, body) = tower_testkit::MockPlatform::webhook(kind, data);
    Call::post("/platform/webhooks")
        .raw_json(body)
        .header("Otto-Signature", header)
        .send(&h.router)
        .await
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn member_removed_ends_that_users_sessions_only(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let other_org = world(&h, "globex");
    let bob = Uuid::new_v4();
    h.platform
        .add_member(w.org, bob, "bob@test.example", Role::Member);
    h.platform
        .add_member(other_org.org, bob, "bob@test.example", Role::Member);

    let rob = session_for(&h, &w).await;
    let bob_acme = sign_in(&h, w.org, bob, Role::Member, "")
        .await
        .cookie_value(SESSION)
        .unwrap();
    let bob_globex = sign_in(&h, other_org.org, bob, Role::Member, "")
        .await
        .cookie_value(SESSION)
        .unwrap();
    assert_eq!(rows(&h).await, 3);

    webhook(
        &h,
        "member.removed",
        serde_json::json!({ "org_id": w.org, "user_id": bob }),
    )
    .await
    .expect(StatusCode::OK);

    assert_eq!(rows(&h).await, 2, "only bob's acme session goes");
    get_with(&h, "/api/session", &bob_acme)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    get_with(&h, "/api/session", &rob)
        .await
        .expect(StatusCode::OK);
    get_with(&h, "/api/session", &bob_globex)
        .await
        .expect(StatusCode::OK);

    // A redelivery re-runs the clean-up and is still a success.
    webhook(
        &h,
        "member.removed",
        serde_json::json!({ "org_id": w.org, "user_id": bob }),
    )
    .await
    .expect(StatusCode::OK);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn org_deleted_ends_every_session_of_that_org(pool: PgPool) {
    let h = harness_with_login(pool).await;
    let w = world(&h, "acme");
    let other = world(&h, "globex");
    let acme = session_for(&h, &w).await;
    session_for(&h, &w).await;
    let globex = session_for(&h, &other).await;
    assert_eq!(rows(&h).await, 3);

    webhook(&h, "org.deleted", serde_json::json!({ "org_id": w.org }))
        .await
        .expect(StatusCode::OK);

    assert_eq!(rows(&h).await, 1);
    get_with(&h, "/api/session", &acme)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    get_with(&h, "/api/session", &globex)
        .await
        .expect(StatusCode::OK);
}

// -------------------------------------------------------------------- misc

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_openapi_document_describes_the_cookie_session_and_the_sign_in_routes(pool: PgPool) {
    let h = harness(pool).await;
    let doc = Call::get("/api/openapi.json").send(&h.router).await;
    doc.expect(StatusCode::OK);
    let scheme = &doc.body["components"]["securitySchemes"]["consoleSession"];
    assert_eq!(scheme["in"], "cookie");
    assert_eq!(scheme["name"], "__Host-of_session");
    for (path, verb) in [
        ("/auth/login", "get"),
        ("/auth/callback", "get"),
        ("/auth/logout", "post"),
        ("/api/session", "get"),
    ] {
        assert!(doc.body["paths"][path][verb].is_object(), "{verb} {path}");
    }
    assert!(doc.body["paths"]["/auth/login"]["get"]["responses"]["303"].is_object());
    assert!(doc.body["paths"]["/auth/login"]["get"]
        .get("security")
        .is_none());
}
