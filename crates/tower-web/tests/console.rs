//! The console API, end to end against a real Postgres.
//!
//! These drive the assembled router, so a test failing here means a real client
//! would have failed the same way. The properties under test are the ones a
//! unit test cannot reach: that the onboarding sequence actually completes, that
//! an org you are not in is indistinguishable from one that does not exist, that
//! a role boundary holds at the HTTP edge, and that removing someone actually
//! disconnects their agents.

use tower_core::jobs::JobsExt;
use tower_core::leases::LeasesExt;
mod common;

use common::{add_member, harness, harness_with_trackers, onboard, org_with_owner, Call};
use http::StatusCode;
use savvagent_resource::Role;
use sqlx::PgPool;

// ------------------------------------------------------------------ queue

/// Enqueue through `tower-core`, because the console cannot.
///
/// That asymmetry is the point of the queue routes: a job is created and
/// completed by the agent doing the work, over MCP. Reaching past the API to
/// set up this fixture is not a shortcut around a route that exists — it is the
/// only way to reach the state, and a test that could enqueue over HTTP would
/// be evidence of a route that should not be there.
async fn enqueue(
    h: &common::Harness,
    org: savvagent_tenant::ids::OrgId,
    repo: tower_core::ids::RepoId,
    title: &str,
    created_by: savvagent_tenant::ids::UserId,
) -> tower_core::jobs::Job {
    let mut tx = h.db.begin(org).await.unwrap();
    let job = tx
        .add_job(tower_core::jobs::NewJob {
            repo_id: repo,
            title: title.into(),
            created_by: Some(created_by),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    job
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_queue_view_lists_filters_and_counts(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let sam = onboard(&h, "sam@acme.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, acme, sam.user, Role::Member).await;

    let api: tower_core::ids::RepoId = {
        let created = Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(serde_json::json!({ "slug": "api" }))
            .send(&h.router)
            .await;
        created.expect(StatusCode::CREATED);
        created.body["id"].as_str().unwrap().parse().unwrap()
    };
    let web: tower_core::ids::RepoId = {
        let created = Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(serde_json::json!({ "slug": "web" }))
            .send(&h.router)
            .await;
        created.expect(StatusCode::CREATED);
        created.body["id"].as_str().unwrap().parse().unwrap()
    };

    let first = enqueue(&h, acme, api, "rewrite the resolver", rob.user).await;
    enqueue(&h, acme, api, "add a lease test", rob.user).await;
    enqueue(&h, acme, web, "fix the meter", sam.user).await;

    // Claiming one moves it off `pending`, which is what makes the status
    // filter and the counters worth asserting separately.
    {
        let mut tx = h.db.begin(acme).await.unwrap();
        tx.claim_jobs(
            std::slice::from_ref(&first.id),
            rob.user,
            Some("claude-code"),
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    let all = Call::get("/api/orgs/acme/jobs")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    all.expect(StatusCode::OK);
    assert_eq!(
        all.body.as_array().unwrap().len(),
        3,
        "any member sees the whole org's queue"
    );

    let pending = Call::get("/api/orgs/acme/jobs?status=pending")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    pending.expect(StatusCode::OK);
    assert_eq!(pending.body.as_array().unwrap().len(), 2);

    let in_repo = Call::get("/api/orgs/acme/jobs?repo=web")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    in_repo.expect(StatusCode::OK);
    assert_eq!(in_repo.body.as_array().unwrap().len(), 1);
    assert_eq!(in_repo.body[0]["title"], "fix the meter");

    let mine = Call::get("/api/orgs/acme/jobs?mine=true")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    mine.expect(StatusCode::OK);
    assert_eq!(
        mine.body.as_array().unwrap().len(),
        1,
        "`mine` is the caller, not the org"
    );

    let stats = Call::get("/api/orgs/acme/jobs/stats")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    stats.expect(StatusCode::OK);
    assert_eq!(stats.body["total"], 3);
    assert_eq!(stats.body["pending"], 2);
    assert_eq!(stats.body["inProgress"], 1);
    assert_eq!(stats.body["blocked"], 0);

    // `/jobs/stats` and `/jobs/{job}` share a prefix; a router that resolved
    // the literal to the parameter would answer this with a 404 for a job
    // called "stats".
    let one = Call::get(format!("/api/orgs/acme/jobs/{}", first.id))
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    one.expect(StatusCode::OK);
    assert_eq!(one.body["title"], "rewrite the resolver");
    assert_eq!(one.body["status"], "in-progress");
    assert_eq!(one.body["claimedByLabel"], "claude-code");
    assert_eq!(one.body["dependsOn"].as_array().unwrap().len(), 0);

    let repo_stats = Call::get("/api/orgs/acme/jobs/stats?repo=api")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    repo_stats.expect(StatusCode::OK);
    assert_eq!(repo_stats.body["total"], 2);
}

/// `active` is a `tower-core` state the console never sets — it only has to
/// read it back correctly through the same read-only routes every other
/// status already goes through.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn an_active_job_is_visible_in_the_queue_and_its_stats(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;

    let api: tower_core::ids::RepoId = {
        let created = Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(serde_json::json!({ "slug": "api" }))
            .send(&h.router)
            .await;
        created.expect(StatusCode::CREATED);
        created.body["id"].as_str().unwrap().parse().unwrap()
    };

    let job = enqueue(&h, acme, api, "activate then check", rob.user).await;
    {
        let mut tx = h.db.begin(acme).await.unwrap();
        tx.claim_jobs(
            std::slice::from_ref(&job.id),
            rob.user,
            Some("agent-one"),
            None,
        )
        .await
        .unwrap();
        tx.activate_job(&job.id).await.unwrap();
        tx.commit().await.unwrap();
    }

    let active = Call::get("/api/orgs/acme/jobs?status=active")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    active.expect(StatusCode::OK);
    let active_jobs = active.body.as_array().unwrap();
    assert_eq!(active_jobs.len(), 1);
    assert_eq!(active_jobs[0]["id"], job.id.to_string());

    let stats = Call::get("/api/orgs/acme/jobs/stats")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    stats.expect(StatusCode::OK);
    assert_eq!(stats.body["active"], 1);
    assert_eq!(stats.body["inProgress"], 0);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_job_detail_names_what_it_is_waiting_for(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;

    let created = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await;
    created.expect(StatusCode::CREATED);
    let api: tower_core::ids::RepoId = created.body["id"].as_str().unwrap().parse().unwrap();

    let blocker = enqueue(&h, acme, api, "land the migration", rob.user).await;
    let blocked = {
        let mut tx = h.db.begin(acme).await.unwrap();
        let job = tx
            .add_job(tower_core::jobs::NewJob {
                repo_id: api,
                title: "use the new column".into(),
                depends_on: vec![blocker.id.clone()],
                created_by: Some(rob.user),
                ..Default::default()
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();
        job
    };

    let detail = Call::get(format!("/api/orgs/acme/jobs/{}", blocked.id))
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    detail.expect(StatusCode::OK);
    assert_eq!(detail.body["dependsOn"][0], blocker.id.as_str());

    // A blocked job is still pending. The overview counts it in both, and the
    // difference is the whole reason `blocked` is reported at all: two pending
    // jobs where one cannot start is not the same queue as two that can.
    let stats = Call::get("/api/orgs/acme/jobs/stats")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    stats.expect(StatusCode::OK);
    assert_eq!(stats.body["pending"], 2);
    assert_eq!(stats.body["blocked"], 1);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn an_unknown_repo_filter_names_the_registered_slugs(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    // Not an empty list. A filter that silently matched nothing would render a
    // queue that looks quiet rather than one that was never asked about.
    let missed = Call::get("/api/orgs/acme/jobs?repo=apo")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    missed.expect(StatusCode::NOT_FOUND);
    assert!(
        missed.text.contains("api"),
        "the error should name what is registered: {}",
        missed.text
    );

    let bad_status = Call::get("/api/orgs/acme/jobs?status=done")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    bad_status.expect(StatusCode::BAD_REQUEST);
    assert!(
        bad_status.text.contains("completed"),
        "an unknown status should list the valid ones: {}",
        bad_status.text
    );
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn one_orgs_queue_is_invisible_to_another(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let mal = onboard(&h, "mal@other.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;
    org_with_owner(&h, "other", &mal).await;

    let created = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await;
    created.expect(StatusCode::CREATED);
    let api: tower_core::ids::RepoId = created.body["id"].as_str().unwrap().parse().unwrap();
    let job = enqueue(&h, acme, api, "acme's secret roadmap", rob.user).await;

    // 404, not 403: a 403 on a real slug and a 404 on a fake one turns any
    // signed-in account into a directory of who uses the product.
    for uri in [
        "/api/orgs/acme/jobs".to_string(),
        "/api/orgs/acme/jobs/stats".to_string(),
        format!("/api/orgs/acme/jobs/{}", job.id),
    ] {
        let refused = Call::get(&uri)
            .with_session(&mal.session)
            .send(&h.router)
            .await;
        refused.expect(StatusCode::NOT_FOUND);
        assert!(
            !refused.text.contains("secret roadmap"),
            "{uri} leaked another org's job"
        );
    }

    // And the same job id, asked for from an org that has one of its own, is
    // that org's job or nothing — ids are per-org counters, so `job-1` exists
    // in both and must not cross.
    let elsewhere = Call::get("/api/orgs/other/jobs/job-1")
        .with_session(&mal.session)
        .send(&h.router)
        .await;
    elsewhere.expect(StatusCode::NOT_FOUND);
}

// ----------------------------------------------------------------- leases

/// Leases, like jobs, are written by an agent over MCP, never by the console
/// — there is no route to create one here, so seeding one for this fixture
/// means reaching past the API into `tower-core`, the same way `enqueue` does
/// for jobs above.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_lease_route_reports_the_resource_field(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;

    let api: tower_core::ids::RepoId = {
        let created = Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(serde_json::json!({ "slug": "api" }))
            .send(&h.router)
            .await;
        created.expect(StatusCode::CREATED);
        created.body["id"].as_str().unwrap().parse().unwrap()
    };

    {
        let mut tx = h.db.begin(acme).await.unwrap();
        tx.acquire_lease(
            api,
            "src/main.rs",
            rob.user,
            Some("claude-code"),
            None,
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    let leases = Call::get("/api/orgs/acme/repos/api/leases")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    leases.expect(StatusCode::OK);
    let all = leases.body.as_array().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0]["resource"], "src/main.rs",
        "the console reads tower_core::leases::Lease verbatim, so its wire shape \
         must carry `resource`, not `branch`: {}",
        leases.body
    );
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn repos_list_reports_lease_presence_and_never_another_orgs(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let mallory = onboard(&h, "mallory@evil.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;
    org_with_owner(&h, "evil", &mallory).await;

    let leased: tower_core::ids::RepoId = {
        let created = Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(serde_json::json!({ "slug": "api" }))
            .send(&h.router)
            .await;
        created.expect(StatusCode::CREATED);
        created.body["id"].as_str().unwrap().parse().unwrap()
    };
    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "quiet" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);
    Call::post("/api/orgs/evil/repos")
        .with_session(&mallory.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    {
        let mut tx = h.db.begin(acme).await.unwrap();
        tx.acquire_lease(
            leased,
            "src/main.rs",
            rob.user,
            Some("claude-code"),
            None,
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    let acme_list = Call::get("/api/orgs/acme/repos?includeLeaseStatus=true")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    acme_list.expect(StatusCode::OK);
    let by_slug = |body: &serde_json::Value, slug: &str| {
        body.as_array()
            .unwrap()
            .iter()
            .find(|r| r["slug"] == slug)
            .unwrap_or_else(|| panic!("no repo named {slug} in {body}"))
            .clone()
    };
    assert_eq!(by_slug(&acme_list.body, "api")["hasActiveLease"], true);
    assert_eq!(by_slug(&acme_list.body, "quiet")["hasActiveLease"], false);

    // Without the flag, the field is omitted entirely rather than computed
    // and reported `false` — the whole point of gating it is skipping the
    // extra org-wide lease read for a caller that never asked.
    let acme_default = Call::get("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    acme_default.expect(StatusCode::OK);
    assert!(
        by_slug(&acme_default.body, "api")
            .get("hasActiveLease")
            .is_none(),
        "hasActiveLease must be omitted, not computed, without ?includeLeaseStatus=true: {}",
        acme_default.body
    );

    let evil_list = Call::get("/api/orgs/evil/repos?includeLeaseStatus=true")
        .with_session(&mallory.session)
        .send(&h.router)
        .await;
    evil_list.expect(StatusCode::OK);
    assert_eq!(
        by_slug(&evil_list.body, "api")["hasActiveLease"],
        false,
        "evil's own unleased 'api' repo must never report acme's active lease"
    );
}

// ---------------------------------------------------------------- openapi

/// The document is served, describes the routes that exist, and needs no
/// credential — a client generator cannot log in.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_openapi_document_is_public_and_describes_the_surface(pool: PgPool) {
    let h = harness(pool).await;
    let doc = Call::get("/api/openapi.json").send(&h.router).await;
    doc.expect(StatusCode::OK);

    assert_eq!(doc.body["openapi"], "3.1.0");
    assert!(doc.body["paths"]["/api/orgs/{org}/repos"]["post"].is_object());
    assert_eq!(
        doc.body["components"]["securitySchemes"]["platformBearer"]["scheme"],
        "bearer"
    );
    // Identity is the platform's: none of it is described here.
    for path in doc.body["paths"].as_object().unwrap().keys() {
        assert!(
            !path.starts_with("/oauth")
                && !path.starts_with("/api/auth")
                && !path.starts_with("/api/me"),
            "{path} is identity surface"
        );
    }
}

/// The OpenAPI document's version matches the workspace version. This pins
/// existing-correct behavior: `openapi.rs` uses `env!("CARGO_PKG_VERSION")`,
/// which in turn resolves to the `tower-web` Cargo.toml's version field — set to
/// `version.workspace = true`, so it reads from the workspace root.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_openapi_document_version_matches_workspace_version(pool: PgPool) {
    let h = harness(pool).await;
    let doc = Call::get("/api/openapi.json").send(&h.router).await;
    doc.expect(StatusCode::OK);

    assert_eq!(
        doc.body["info"]["version"],
        env!("CARGO_PKG_VERSION"),
        "OpenAPI document version must match the workspace version"
    );
}

/// The console displays `web/package.json`'s `version` in its footer
/// (`web/src/lib/version.ts`); this proves that value is actually the
/// workspace version and not something that has drifted from it.
/// `include_str!` rather than `std::fs::read_to_string` so a missing or
/// unparseable file fails the build, not a passing test that never ran.
#[test]
fn the_console_and_the_server_agree_on_the_version() {
    const PACKAGE_JSON: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/package.json"
    ));
    let package: serde_json::Value = serde_json::from_str(PACKAGE_JSON).expect("web/package.json");
    assert_eq!(
        package["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "web/package.json and the workspace version disagree — the console footer would \
         display a version the server was not built from"
    );
}

/// Routes are mounted from the same list the document is rendered from, so
/// anything described has to actually answer. This catches the failure the
/// catalog exists to prevent — a documented endpoint that is not mounted.
///
/// A handler's own `404` ("no such repo") is a pass; the router's is not. They
/// are told apart by the body: every failure this crate produces carries the
/// `{"error": {...}}` envelope, and a route that does not exist produces an
/// empty one.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn every_documented_get_is_actually_mounted(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let doc = Call::get("/api/openapi.json").send(&h.router).await;
    let paths = doc.body["paths"].as_object().unwrap().clone();
    let mut checked = 0;

    for (path, operations) in paths {
        if operations.get("get").is_none() {
            continue;
        }
        let concrete = path
            .replace("{org}", "acme")
            .replace("{team}", "platform")
            .replace("{repo}", "api")
            .replace("{user}", &rob.user.to_string())
            .replace("{id}", &uuid::Uuid::nil().to_string());

        let reply = Call::get(&concrete)
            .with_session(&rob.session)
            .send(&h.router)
            .await;
        checked += 1;

        assert_ne!(
            reply.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "GET {concrete} is documented but the path serves other methods only"
        );
        if reply.status == StatusCode::NOT_FOUND {
            assert!(
                reply.error_code().is_some(),
                "GET {concrete} is documented but not mounted — the 404 came from \
                 the router, not from a handler (body was {:?})",
                reply.text
            );
        }
    }

    assert!(
        checked >= 9,
        "only {checked} GETs were checked; the document looks empty"
    );
}

// --------------------------------------------------------------- trackers

/// Tracker setup grants a repo the ability to move a customer's tickets, so
/// every route that writes one is admin-only. The read is a member read for the
/// same reason `GET /repos` is: it describes a repo the member can already see.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn only_an_admin_can_connect_a_tracker_or_bind_a_repo(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;

    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Member).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    // A member reads a repo's bindings.
    Call::get("/api/orgs/acme/repos/api/tracker-bindings")
        .with_session(&bob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    for call in [
        Call::get("/api/orgs/acme/tracker-connections"),
        Call::post("/api/orgs/acme/tracker-connections/github")
            .json(serde_json::json!({ "code": "x", "installationId": 17 })),
        Call::delete("/api/orgs/acme/tracker-connections/github"),
        Call::put("/api/orgs/acme/repos/api/tracker-bindings/github")
            .json(serde_json::json!({ "externalRef": "acme/api" })),
        Call::delete("/api/orgs/acme/repos/api/tracker-bindings/github"),
    ] {
        call.with_session(&bob.session)
            .send(&h.router)
            .await
            .expect(StatusCode::FORBIDDEN);
    }
}

/// The same "an org you are not in is a 404, never a 403" rule the rest of the
/// console keeps. A tracker route is a particularly bad place to break it: the
/// binding names the customer's JIRA project key.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn another_orgs_tracker_routes_are_invisible(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let mallory = onboard(&h, "mallory@evil.test").await;

    org_with_owner(&h, "acme", &rob).await;
    org_with_owner(&h, "evil", &mallory).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    for (real, imaginary) in [
        (
            Call::get("/api/orgs/acme/tracker-connections"),
            Call::get("/api/orgs/no-such-org/tracker-connections"),
        ),
        (
            Call::delete("/api/orgs/acme/tracker-connections/github"),
            Call::delete("/api/orgs/no-such-org/tracker-connections/github"),
        ),
        (
            Call::get("/api/orgs/acme/repos/api/tracker-bindings"),
            Call::get("/api/orgs/no-such-org/repos/api/tracker-bindings"),
        ),
        (
            Call::put("/api/orgs/acme/repos/api/tracker-bindings/github")
                .json(serde_json::json!({ "externalRef": "acme/api" })),
            Call::put("/api/orgs/no-such-org/repos/api/tracker-bindings/github")
                .json(serde_json::json!({ "externalRef": "acme/api" })),
        ),
    ] {
        let real = real.with_session(&mallory.session).send(&h.router).await;
        let imaginary = imaginary
            .with_session(&mallory.session)
            .send(&h.router)
            .await;

        real.expect(StatusCode::NOT_FOUND);
        assert_eq!(
            real.status, imaginary.status,
            "a real org you are not in must answer like one that does not exist"
        );
        assert_eq!(real.error_code(), imaginary.error_code());
    }
}

/// A binding that can never match an inbound event is a configuration error
/// worth catching where it is typed, not at 3am when the label appears to do
/// nothing. `owner/repo` is what webhook ingest matches `repository.full_name`
/// against; a project key is what it matches `fields.project.key` against.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_binding_that_could_never_match_an_event_is_refused(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    for (provider, bad) in [
        ("github", "acme"),
        ("github", "acme/api/extra"),
        ("github", "acme/"),
        ("jira", "not a key"),
        ("jira", "acme-123"),
    ] {
        let refused = Call::put(format!(
            "/api/orgs/acme/repos/api/tracker-bindings/{provider}"
        ))
        .with_session(&rob.session)
        .json(serde_json::json!({ "externalRef": bad }))
        .send(&h.router)
        .await;
        refused.expect(StatusCode::BAD_REQUEST);
        assert!(
            !refused.text.is_empty(),
            "{provider} binding {bad:?} was refused with no explanation"
        );
    }

    // And the shapes that can match are accepted, and round-trip.
    Call::put("/api/orgs/acme/repos/api/tracker-bindings/github")
        .with_session(&rob.session)
        .json(serde_json::json!({ "externalRef": "acme/api", "triggerLabel": "agent" }))
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
    Call::put("/api/orgs/acme/repos/api/tracker-bindings/jira")
        .with_session(&rob.session)
        .json(serde_json::json!({ "externalRef": "ACME" }))
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    let listed = Call::get("/api/orgs/acme/repos/api/tracker-bindings")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);
    assert!(listed.text.contains("acme/api"), "{}", listed.text);
    assert!(listed.text.contains("ACME"), "{}", listed.text);
    assert!(
        listed.text.contains("agent"),
        "the trigger label is what inbound sync watches for: {}",
        listed.text
    );
    // The JIRA binding named no label, so it gets the default for new bindings.
    assert!(
        listed.text.contains("otto-tower"),
        "a binding with no label defaults to otto-tower: {}",
        listed.text
    );

    Call::delete("/api/orgs/acme/repos/api/tracker-bindings/jira")
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);

    let after = Call::get("/api/orgs/acme/repos/api/tracker-bindings")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    assert!(!after.text.contains("ACME"), "{}", after.text);
}

/// Ciphertext is not a secret in the sense that leaking it grants access, but a
/// console `GET` handing every admin's browser the sealed JIRA refresh token is
/// gratuitous exposure of exactly the material `OF_ENCRYPTION_KEY` exists to
/// protect. The listing is a view type for this reason, not the domain row.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_connection_listing_never_carries_stored_secrets(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;

    let mut tx = h.db.begin(org).await.expect("tx");
    tower_core::trackers::upsert_connection(
        &mut tx,
        tower_core::trackers::Provider::Jira,
        "site-1",
        Some(&common::cipher().seal(b"jira-refresh-token").expect("seal")),
        None,
    )
    .await
    .expect("connection");
    tx.commit().await.expect("commit");

    let listed = Call::get("/api/orgs/acme/tracker-connections")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);

    assert!(
        listed.text.contains("site-1"),
        "the external id is what the admin needs to see: {}",
        listed.text
    );
    assert!(
        !listed.text.contains("encrypted"),
        "the console listing carried stored ciphertext: {}",
        listed.text
    );
    assert!(
        listed.text.contains("hasCredentials"),
        "the page still needs to know a credential is stored: {}",
        listed.text
    );
}

/// A deployment with no GitHub OAuth client cannot finish a connect flow, and
/// says so instead of walking an admin through an install they then have to
/// undo by hand. The test harness configures no provider, which is exactly the
/// deployment shape being asserted.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_deployment_that_cannot_connect_a_provider_says_so(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let listed = Call::get("/api/orgs/acme/tracker-connections")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);
    assert!(
        listed.text.contains("\"configured\":false"),
        "an unconfigured deployment must not advertise a connect flow: {}",
        listed.text
    );

    let refused = Call::post("/api/orgs/acme/tracker-connections/github")
        .with_session(&rob.session)
        .json(serde_json::json!({ "code": "x", "installationId": 17 }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);
    assert!(
        refused.text.contains("not configured"),
        "the refusal should name the deployment's gap: {}",
        refused.text
    );
}

/// GitHub's connect flow needs an installation id and JIRA's does not have one.
/// A GitHub request without it is refused rather than defaulted, because the
/// value is what the whole verification is about — and refused *before* any
/// call to GitHub, which is what lets this test run without a network.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn connecting_github_without_an_installation_id_is_refused(pool: PgPool) {
    let h = harness_with_trackers(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    // A configured deployment advertises both flows, with the provider URLs
    // built server-side so no App slug or client id is baked into the bundle.
    let listed = Call::get("/api/orgs/acme/tracker-connections")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);
    assert!(
        listed
            .text
            .contains("github.com/apps/otto-tower/installations/new"),
        "the install link is built from the configured slug: {}",
        listed.text
    );
    assert!(
        listed.text.contains("offline_access"),
        "without offline_access JIRA returns no refresh token and the connection dies \
         an hour after it is made: {}",
        listed.text
    );

    let refused = Call::post("/api/orgs/acme/tracker-connections/github")
        .with_session(&rob.session)
        .json(serde_json::json!({ "code": "x" }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);
    assert!(
        refused.text.contains("installationId"),
        "the refusal should name the missing field: {}",
        refused.text
    );
}

/// An unknown provider names the two that exist rather than 404ing into
/// silence — the same rule the MCP errors follow.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn an_unknown_provider_names_the_ones_that_exist(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let refused = Call::delete("/api/orgs/acme/tracker-connections/linear")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);
    assert!(
        refused.text.contains("github") && refused.text.contains("jira"),
        "the refusal should list the valid providers: {}",
        refused.text
    );
}

/// The console API no longer writes the free-form `repos.tracker_binding` blob
/// — `tracker_bindings` rows replaced it, and they are what webhook ingest and
/// the sync engine actually read.
///
/// **Dropping the field must not turn it into a `400`.** An unknown field is
/// not an error in this API, and making it one for this field alone would be a
/// second breaking change on top of the first: a client still sending it would
/// go from "stored somewhere nothing reads" to "cannot register a repo at all".
/// It is ignored, and the repo is created.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn the_console_ignores_the_retired_tracker_binding_field(pool: PgPool) {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let created = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({
            "slug": "api",
            "trackerBinding": { "jira": "ACME" }
        }))
        .send(&h.router)
        .await;
    created.expect(StatusCode::CREATED);
    assert!(
        !created.text.contains("ACME"),
        "the retired field was stored rather than ignored: {}",
        created.text
    );

    let patched = Call::patch("/api/orgs/acme/repos/api")
        .with_session(&rob.session)
        .json(serde_json::json!({
            "name": "API",
            "trackerBinding": { "jira": "ACME" }
        }))
        .send(&h.router)
        .await;
    patched.expect(StatusCode::OK);
    assert!(
        patched.text.contains("API") && !patched.text.contains("ACME"),
        "the update applied the wrong half: {}",
        patched.text
    );
}
