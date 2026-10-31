//! The console API as a resource server of the Savvagent platform.
//!
//! What is under test here is the seam between this service and the platform:
//! who a bearer token says its holder is, which org it opens, what happens when
//! the platform cannot answer, how a team id earns its way onto a repo, and what
//! the platform's signed lifecycle webhooks do. Identity itself is the
//! platform's and is covered there.

mod common;

use common::{
    add_member, harness, harness_uncached_members, onboard, org_with_owner, Call, Harness,
};
use http::StatusCode;
use savvagent_resource::Role;
use savvagent_tenant::ids::{OrgId, TeamId, UserId};
use sqlx::PgPool;
use tower_core::repos::{RepoRef, ReposExt};
use tower_testkit::MockPlatform;
use uuid::Uuid;

const ALL: &[&str] = tower_core::scopes::KNOWN;

// ------------------------------------------------------------ authentication

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_request_with_no_token_is_unauthenticated(pool: PgPool) {
    let h = harness(pool).await;
    let reply = Call::get("/api/orgs/acme/repos").send(&h.router).await;
    reply.expect(StatusCode::UNAUTHORIZED);
    assert_eq!(reply.error_code(), Some("unauthenticated"));
    assert_eq!(h.platform.introspect_calls(), 0);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_token_the_platform_does_not_know_is_unauthenticated(pool: PgPool) {
    let h = harness(pool).await;
    let junk = tower_testkit::new_token();
    Call::get("/api/orgs/acme/repos")
        .with_session(&junk)
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
}

/// A token minted for a different resource server is inactive here, and the
/// console API says nothing more than that.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_token_for_another_resource_is_refused(pool: PgPool) {
    let h = harness(pool).await;
    let (org, user) = (Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(org, "acme", "Acme");
    let foreign =
        h.platform
            .issue_for("https://someone-else.test/mcp", org, user, Role::Owner, ALL);
    Call::get("/api/orgs/acme/repos")
        .with_session(&foreign)
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
}

/// A platform outage is not an authentication failure.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_platform_outage_is_503_not_401(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    h.platform.set_down(true);
    let reply = Call::get("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    reply.expect(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.error_code(), Some("platform_unavailable"));
    assert!(reply.headers.contains_key(http::header::RETRY_AFTER));
    assert!(
        !reply.text.contains("mock platform"),
        "the platform's own words must not reach the client: {}",
        reply.text
    );

    h.platform.set_down(false);
    Call::get("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
}

/// A token opens exactly the org it was issued for. Another real org, and an org
/// that does not exist, answer identically, so a token is not a directory of who
/// uses the product.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_token_cannot_reach_another_orgs_console(pool: PgPool) {
    let h = harness(pool).await;
    let (acme, globex) = (Uuid::new_v4(), Uuid::new_v4());
    let (rob, eve) = (Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(acme, "acme", "Acme");
    h.platform.add_org(globex, "globex", "Globex");
    h.platform
        .add_member(acme, rob, "rob@acme.test", Role::Owner);
    h.platform
        .add_member(globex, eve, "eve@globex.test", Role::Owner);
    let rob_token = h.platform.issue(acme, rob, Role::Owner, ALL);

    Call::get("/api/orgs/acme/repos")
        .with_session(&rob_token)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    let real = Call::get("/api/orgs/globex/repos")
        .with_session(&rob_token)
        .send(&h.router)
        .await;
    let fake = Call::get("/api/orgs/nonesuch/repos")
        .with_session(&rob_token)
        .send(&h.router)
        .await;
    real.expect(StatusCode::NOT_FOUND);
    fake.expect(StatusCode::NOT_FOUND);
    assert_eq!(real.error_code(), fake.error_code());

    // And writing into it is no easier than reading it.
    Call::post("/api/orgs/globex/repos")
        .with_session(&rob_token)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::NOT_FOUND);

    // The org may also be named by id, but only its own.
    Call::get(format!("/api/orgs/{acme}/repos"))
        .with_session(&rob_token)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
    Call::get(format!("/api/orgs/{globex}/repos"))
        .with_session(&rob_token)
        .send(&h.router)
        .await
        .expect(StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_member_removed_at_the_platform_is_locked_out(pool: PgPool) {
    let h = harness_uncached_members(pool).await;
    let (org, user) = (Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(org, "acme", "Acme");
    h.platform
        .add_member(org, user, "bob@acme.test", Role::Member);
    let token = h.platform.issue(org, user, Role::Member, ALL);

    Call::get("/api/orgs/acme/repos")
        .with_session(&token)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    h.platform.remove_member(org, user);
    // Introspection is cached for a minute, but the org lookup is not: a member
    // the platform no longer sees is refused immediately.
    Call::get("/api/orgs/acme/repos")
        .with_session(&token)
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn roles_and_scopes_both_have_to_allow_a_write(pool: PgPool) {
    let h = harness(pool).await;
    let (org, admin, member) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(org, "acme", "Acme");
    h.platform
        .add_member(org, admin, "ada@acme.test", Role::Admin);
    h.platform
        .add_member(org, member, "bob@acme.test", Role::Member);
    let body = serde_json::json!({ "slug": "api" });

    // A member's token with every scope still cannot administer.
    let member_token = h.platform.issue(org, member, Role::Member, ALL);
    let reply = Call::post("/api/orgs/acme/repos")
        .with_session(&member_token)
        .json(body.clone())
        .send(&h.router)
        .await;
    reply.expect(StatusCode::FORBIDDEN);
    assert!(reply.text.contains("member"), "{}", reply.text);

    // An admin's token without the scope cannot write, and is told which scope.
    let read_only = h.platform.issue(org, admin, Role::Admin, &["repos:read"]);
    let reply = Call::post("/api/orgs/acme/repos")
        .with_session(&read_only)
        .json(body.clone())
        .send(&h.router)
        .await;
    reply.expect(StatusCode::FORBIDDEN);
    assert!(reply.text.contains("repos:write"), "{}", reply.text);
    Call::get("/api/orgs/acme/repos")
        .with_session(&read_only)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    let full = h.platform.issue(org, admin, Role::Admin, ALL);
    Call::post("/api/orgs/acme/repos")
        .with_session(&full)
        .json(body)
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);
}

/// Lease activity is a `jobs:read` fact over MCP (`list_leases`), so the console
/// must not hand it to a token carrying only `repos:read` — neither as the lease
/// list itself nor as `hasActiveLease` on the repo list.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn reading_leases_needs_jobs_read_as_it_does_over_mcp(pool: PgPool) {
    let h = harness(pool).await;
    let (org, admin) = (Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(org, "acme", "Acme");
    h.platform
        .add_member(org, admin, "ada@acme.test", Role::Admin);
    let full = h.platform.issue(org, admin, Role::Admin, ALL);
    Call::post("/api/orgs/acme/repos")
        .with_session(&full)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    let repos_only = h.platform.issue(org, admin, Role::Admin, &["repos:read"]);
    let reply = Call::get("/api/orgs/acme/repos/api/leases")
        .with_session(&repos_only)
        .send(&h.router)
        .await;
    reply.expect(StatusCode::FORBIDDEN);
    assert!(reply.text.contains("jobs:read"), "{}", reply.text);
    let reply = Call::get("/api/orgs/acme/repos?includeLeaseStatus=true")
        .with_session(&repos_only)
        .send(&h.router)
        .await;
    reply.expect(StatusCode::FORBIDDEN);
    assert!(reply.text.contains("jobs:read"), "{}", reply.text);
    // The plain listing is still a `repos:read` read.
    Call::get("/api/orgs/acme/repos")
        .with_session(&repos_only)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    let both = h
        .platform
        .issue(org, admin, Role::Admin, &["repos:read", "jobs:read"]);
    Call::get("/api/orgs/acme/repos/api/leases")
        .with_session(&both)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
    Call::get("/api/orgs/acme/repos?includeLeaseStatus=true")
        .with_session(&both)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
}

// -------------------------------------------------------------------- teams

async fn registered_slugs(h: &Harness, org: OrgId) -> Vec<String> {
    let mut tx = h.db.begin(org).await.unwrap();
    let repos = tx.list_repos(true, None).await.unwrap();
    tx.commit().await.unwrap();
    repos.into_iter().map(|r| r.slug).collect()
}

/// An unknown team must never become "no team", which is org-wide.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_repo_cannot_be_scoped_to_a_team_the_platform_does_not_know(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;

    let reply = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api", "teamId": TeamId::new() }))
        .send(&h.router)
        .await;
    reply.expect(StatusCode::NOT_FOUND);
    assert_eq!(reply.error_code(), Some("team_not_found"));
    assert!(
        registered_slugs(&h, org).await.is_empty(),
        "the repo was created anyway"
    );
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_repo_cannot_be_scoped_to_another_orgs_team(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;
    let eve = onboard(&h, "eve@globex.test").await;
    let globex = org_with_owner(&h, "globex", &eve).await;
    let theirs = TeamId::new();
    h.platform
        .add_team(globex.as_uuid(), theirs.as_uuid(), "secret", "Secret");

    let reply = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api", "teamId": theirs }))
        .send(&h.router)
        .await;
    reply.expect(StatusCode::NOT_FOUND);
    assert!(registered_slugs(&h, acme).await.is_empty());
}

/// If the platform cannot say whether the team exists, the answer is "try
/// again", not "assume it does" and not "assume there is none" -- and the repo is
/// not created. (`VerifiedTeam::verify`'s own outage behavior is covered in
/// tower-core; this proves the HTTP edge never writes anything on the way.)
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_team_scoped_write_is_refused_while_the_platform_is_unreachable(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    let team = TeamId::new();
    h.platform
        .add_team(org.as_uuid(), team.as_uuid(), "platform", "Platform");

    h.platform.set_down(true);
    let reply = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api", "teamId": team }))
        .send(&h.router)
        .await;
    reply.expect(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.error_code(), Some("platform_unavailable"));
    h.platform.set_down(false);

    assert!(registered_slugs(&h, org).await.is_empty());
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_known_team_scopes_a_repo_and_can_be_released(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    let team = TeamId::new();
    h.platform
        .add_team(org.as_uuid(), team.as_uuid(), "platform", "Platform");

    let created = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api", "teamId": team }))
        .send(&h.router)
        .await;
    created.expect(StatusCode::CREATED);
    assert_eq!(created.body["teamId"], team.to_string());

    // Moving it to a team that does not exist is refused and changes nothing.
    let refused = Call::patch("/api/orgs/acme/repos/api")
        .with_session(&rob.session)
        .json(serde_json::json!({ "teamId": TeamId::new() }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::NOT_FOUND);
    let still = Call::get("/api/orgs/acme/repos/api")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    assert_eq!(still.body["teamId"], team.to_string());

    // An explicit null is the deliberate, admin-only way to make it org-wide.
    let released = Call::patch("/api/orgs/acme/repos/api")
        .with_session(&rob.session)
        .json(serde_json::json!({ "teamId": null }))
        .send(&h.router)
        .await;
    released.expect(StatusCode::OK);
    assert!(released.body["teamId"].is_null());
}

/// A member on no team sees org-wide repos only; a team-scoped repo is for its
/// team and for admins. (The team-membership cases are in `team_scoping.rs`.)
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_team_scoped_repo_is_hidden_from_non_admins(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    let bob = onboard(&h, "bob@acme.test").await;
    add_member(&h, org, bob.user, Role::Member).await;
    let team = TeamId::new();
    h.platform
        .add_team(org.as_uuid(), team.as_uuid(), "platform", "Platform");

    for body in [
        serde_json::json!({ "slug": "scoped", "teamId": team }),
        serde_json::json!({ "slug": "open" }),
    ] {
        Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(body)
            .send(&h.router)
            .await
            .expect(StatusCode::CREATED);
    }

    let bobs = Call::get("/api/orgs/acme/repos")
        .with_session(&bob.session)
        .send(&h.router)
        .await;
    bobs.expect(StatusCode::OK);
    let slugs: Vec<&str> = bobs
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, vec!["open"]);
    Call::get("/api/orgs/acme/repos/scoped")
        .with_session(&bob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NOT_FOUND);

    let robs = Call::get("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    assert_eq!(robs.body.as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_job_team_filter_resolves_through_the_platform_and_never_falls_open(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    h.platform
        .add_team(org.as_uuid(), Uuid::new_v4(), "platform", "Platform");

    // A real team with no jobs: an empty list.
    let known = Call::get("/api/orgs/acme/jobs?team=platform")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    known.expect(StatusCode::OK);
    assert!(known.body.as_array().unwrap().is_empty());

    // A typo is an error, not an unfiltered queue.
    let typo = Call::get("/api/orgs/acme/jobs?team=platfrom")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    typo.expect(StatusCode::NOT_FOUND);
    assert!(typo.text.contains("platfrom"), "{}", typo.text);
}

// -------------------------------------------------------- platform webhooks

fn sign(kind: &str, data: serde_json::Value) -> (String, Vec<u8>) {
    MockPlatform::webhook(kind, data)
}

async fn deliver(h: &Harness, header: Option<&str>, body: Vec<u8>) -> common::Reply {
    let mut call = Call::post("/platform/webhooks").raw_json(body);
    if let Some(header) = header {
        call = call.header("Otto-Signature", header);
    }
    call.send(&h.router).await
}

async fn seed_repo(h: &Harness, owner: &common::Account, slug: &str) {
    Call::post("/api/orgs/acme/repos")
        .with_session(&owner.session)
        .json(serde_json::json!({ "slug": slug }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn an_unsigned_or_badly_signed_webhook_is_rejected_and_ignored(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    seed_repo(&h, &rob, "api").await;
    let (good, body) = sign(
        "org.deleted",
        serde_json::json!({ "org_id": org.as_uuid() }),
    );

    // No header at all.
    let reply = deliver(&h, None, body.clone()).await;
    reply.expect(StatusCode::UNAUTHORIZED);
    assert_eq!(reply.error_code(), Some("invalid_signature"));

    // Garbage in the header.
    deliver(&h, Some("not a signature"), body.clone())
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // Signed with the wrong key.
    let wrong = savvagent_resource::webhook::sign(
        "otto_whsec_wrong",
        chrono::Utc::now().timestamp(),
        &body,
    );
    deliver(&h, Some(&wrong), body.clone())
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // A genuine signature over a different body.
    let (_, other) = sign(
        "org.deleted",
        serde_json::json!({ "org_id": Uuid::new_v4() }),
    );
    deliver(&h, Some(&good), other)
        .await
        .expect(StatusCode::UNAUTHORIZED);

    // A genuine signature from long ago: a replayed capture.
    let stale = savvagent_resource::webhook::sign(
        tower_testkit::WEBHOOK_SECRET,
        chrono::Utc::now().timestamp() - 3600,
        &body,
    );
    deliver(&h, Some(&stale), body.clone())
        .await
        .expect(StatusCode::UNAUTHORIZED);

    assert_eq!(
        registered_slugs(&h, org).await,
        vec!["api".to_string()],
        "a rejected webhook changed data"
    );
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn org_deleted_purges_the_org_and_a_redelivery_is_acknowledged(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;
    seed_repo(&h, &rob, "api").await;
    seed_repo(&h, &rob, "web").await;

    let eve = onboard(&h, "eve@globex.test").await;
    let globex = org_with_owner(&h, "globex", &eve).await;
    Call::post("/api/orgs/globex/repos")
        .with_session(&eve.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    let id = Uuid::new_v4();
    let (header, body) = MockPlatform::webhook_with_id(
        id,
        "org.deleted",
        serde_json::json!({ "org_id": acme.as_uuid() }),
    );
    let reply = deliver(&h, Some(&header), body.clone()).await;
    reply.expect(StatusCode::OK);
    assert_eq!(reply.body["result"], "applied");
    assert_eq!(reply.body["detail"]["repos"], 2);

    assert!(registered_slugs(&h, acme).await.is_empty());
    assert_eq!(registered_slugs(&h, globex).await, vec!["api".to_string()]);

    // The platform retries until it sees a 2xx; the second delivery is a no-op.
    let again = MockPlatform::webhook_with_id(
        id,
        "org.deleted",
        serde_json::json!({ "org_id": acme.as_uuid() }),
    );
    let reply = deliver(&h, Some(&again.0), again.1).await;
    reply.expect(StatusCode::OK);
    assert_eq!(reply.body["result"], "duplicate");
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn team_deleted_never_widens_a_repos_scope(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    let team = TeamId::new();
    h.platform
        .add_team(org.as_uuid(), team.as_uuid(), "platform", "Platform");
    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api", "teamId": team }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    h.platform.remove_team(org.as_uuid(), team.as_uuid());
    let (header, body) = sign(
        "team.deleted",
        serde_json::json!({ "org_id": org.as_uuid(), "team_id": team.as_uuid() }),
    );
    let reply = deliver(&h, Some(&header), body).await;
    reply.expect(StatusCode::OK);
    assert_eq!(reply.body["result"], "applied");

    let mut tx = h.db.begin(org).await.unwrap();
    let repo = tx
        .resolve_repo(&RepoRef {
            slug: Some("api".into()),
            remote: None,
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        repo.team_id,
        Some(team),
        "the deleted team's scope was widened to org-wide"
    );

    // Still hidden from a plain member, still visible to the admin.
    let bob = onboard(&h, "bob@acme.test").await;
    add_member(&h, org, bob.user, Role::Member).await;
    Call::get("/api/orgs/acme/repos/api")
        .with_session(&bob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NOT_FOUND);
    Call::get("/api/orgs/acme/repos/api")
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    // The audit trail says what happened.
    let audit = Call::get("/api/orgs/acme/audit?actionPrefix=platform.")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    audit.expect(StatusCode::OK);
    assert_eq!(audit.body[0]["action"], "platform.team.deleted");
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn member_removed_releases_their_claims_and_leases(pool: PgPool) {
    use tower_core::jobs::JobsExt;
    use tower_core::leases::LeasesExt;

    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    seed_repo(&h, &rob, "api").await;
    let bob = UserId::new();

    let mut tx = h.db.begin(org).await.unwrap();
    let repo = tx
        .resolve_repo(&RepoRef {
            slug: Some("api".into()),
            remote: None,
        })
        .await
        .unwrap();
    let job = tx
        .add_job(tower_core::jobs::NewJob {
            repo_id: repo.id,
            title: "bobs work".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.claim_jobs(std::slice::from_ref(&job.id), bob, None, None)
        .await
        .unwrap();
    tx.acquire_lease(repo.id, "branch:main", bob, None, None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let (header, body) = sign(
        "member.removed",
        serde_json::json!({ "org_id": org.as_uuid(), "user_id": bob.as_uuid() }),
    );
    deliver(&h, Some(&header), body)
        .await
        .expect(StatusCode::OK);

    let leases = Call::get("/api/orgs/acme/repos/api/leases")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    assert!(leases.body.as_array().unwrap().is_empty());
    let jobs = Call::get("/api/orgs/acme/jobs?status=pending")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    assert_eq!(jobs.body.as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn an_event_this_version_does_not_know_is_acknowledged(pool: PgPool) {
    let h = harness(pool).await;
    let (header, body) = sign("plan.changed", serde_json::json!({ "plan": "team" }));
    let reply = deliver(&h, Some(&header), body).await;
    reply.expect(StatusCode::OK);
    assert_eq!(reply.body["result"], "ignored");
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_signed_but_unreadable_body_is_a_bad_request_not_a_signature_failure(pool: PgPool) {
    let h = harness(pool).await;
    let body = br#"{"not":"an event"}"#.to_vec();
    let header = savvagent_resource::webhook::sign(
        tower_testkit::WEBHOOK_SECRET,
        chrono::Utc::now().timestamp(),
        &body,
    );
    deliver(&h, Some(&header), body)
        .await
        .expect(StatusCode::BAD_REQUEST);
}

/// The route sits beside, not inside, the tracker webhook route.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_platform_route_does_not_collide_with_the_tracker_route(pool: PgPool) {
    let h = harness(pool).await;
    // A tracker request for an unknown provider is still the uniform 404...
    Call::post("/webhooks/platform")
        .raw_json(b"{}".to_vec())
        .send(&h.router)
        .await
        .expect(StatusCode::NOT_FOUND);
    // ...and the platform route is its own thing.
    deliver(&h, None, b"{}".to_vec())
        .await
        .expect(StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_audit_log_needs_an_admin_and_the_org_admin_scope(pool: PgPool) {
    let h = harness_uncached_members(pool).await;
    let (org, admin) = (Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(org, "acme", "Acme");
    h.platform
        .add_member(org, admin, "ada@acme.test", Role::Admin);

    let no_scope = h
        .platform
        .issue(org, admin, Role::Admin, &["repos:read", "jobs:read"]);
    Call::get("/api/orgs/acme/audit")
        .with_session(&no_scope)
        .send(&h.router)
        .await
        .expect(StatusCode::FORBIDDEN);

    let full = h.platform.issue(org, admin, Role::Admin, ALL);
    Call::get("/api/orgs/acme/audit")
        .with_session(&full)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    // Demoted at the platform: admin routes close immediately, even though the
    // introspection cache still holds the old role.
    h.platform.set_role(org, admin, Role::Member);
    Call::get("/api/orgs/acme/audit")
        .with_session(&full)
        .send(&h.router)
        .await
        .expect(StatusCode::FORBIDDEN);
}

/// Identity is only ever the platform's answer: headers and query strings that
/// claim an org or user are ignored. A session cookie is a credential only if it
/// opens a stored console session (`tests/console_login.rs`); a made-up one is
/// not, and beside a bearer token it is not even consulted.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn nothing_but_the_token_decides_who_is_calling(pool: PgPool) {
    let h = harness(pool).await;
    let (acme, globex, rob) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(acme, "acme", "Acme");
    h.platform.add_org(globex, "globex", "Globex");
    h.platform
        .add_member(acme, rob, "rob@acme.test", Role::Member);
    let token = h.platform.issue(acme, rob, Role::Member, ALL);

    Call::get(format!("/api/orgs/globex/repos?org={acme}&user={rob}"))
        .with_session(&token)
        .header("x-org-id", globex.to_string())
        .header("x-user-id", rob.to_string())
        .header("cookie", "__Host-of_session=anything")
        .send(&h.router)
        .await
        .expect(StatusCode::NOT_FOUND);
    // No token, only claims in headers: unauthenticated.
    Call::get("/api/orgs/acme/repos")
        .header("x-org-id", acme.to_string())
        .header("x-user-id", rob.to_string())
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    // A session cookie that opens no stored session is no credential either,
    // whatever it is named or what else the request claims.
    Call::get("/api/orgs/acme/repos")
        .header("x-org-id", acme.to_string())
        .header("cookie", "__Host-of_session=anything")
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    Call::get("/api/orgs/acme/repos")
        .header("cookie", format!("__Host-of_session={}", "A".repeat(43)))
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
}

/// The platform's introspection cache would keep vouching for these tokens for
/// up to a minute; the tombstones refuse them at once.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_deleted_org_and_a_removed_member_are_refused_before_the_cache_expires(pool: PgPool) {
    let h = harness(pool).await;
    let (org, rob, bob) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    h.platform.add_org(org, "acme", "Acme");
    h.platform
        .add_member(org, rob, "rob@acme.test", Role::Owner);
    h.platform
        .add_member(org, bob, "bob@acme.test", Role::Member);
    let (rob_t, bob_t) = (
        h.platform.issue(org, rob, Role::Owner, ALL),
        h.platform.issue(org, bob, Role::Member, ALL),
    );
    for t in [&rob_t, &bob_t] {
        Call::get("/api/orgs/acme/repos")
            .with_session(t)
            .send(&h.router)
            .await
            .expect(StatusCode::OK); // warms the introspection cache
    }

    // Removed at the platform; the platform still "vouches" (mock unchanged).
    let (header, body) = sign(
        "member.removed",
        serde_json::json!({ "org_id": org, "user_id": bob }),
    );
    deliver(&h, Some(&header), body)
        .await
        .expect(StatusCode::OK);
    Call::get("/api/orgs/acme/repos")
        .with_session(&bob_t)
        .send(&h.router)
        .await
        .expect(StatusCode::UNAUTHORIZED);
    Call::get("/api/orgs/acme/repos")
        .with_session(&rob_t)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    // The tombstone is short-lived: a re-added member works again.
    sqlx::query("UPDATE removed_members SET removed_at = now() - interval '1 hour'")
        .execute(h.db.pool())
        .await
        .unwrap();
    Call::get("/api/orgs/acme/repos")
        .with_session(&bob_t)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    // A deleted org is refused for everyone, permanently.
    let (header, body) = sign("org.deleted", serde_json::json!({ "org_id": org }));
    deliver(&h, Some(&header), body)
        .await
        .expect(StatusCode::OK);
    for t in [&rob_t, &bob_t] {
        Call::get("/api/orgs/acme/repos")
            .with_session(t)
            .send(&h.router)
            .await
            .expect(StatusCode::UNAUTHORIZED);
    }
}
