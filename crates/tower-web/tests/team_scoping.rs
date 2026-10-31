//! Team scoping on the console: who may see which team's repos and jobs.
//!
//! A row with a null `team_id` is org-wide. A row with a `team_id` is for that
//! team's members and for org owners and admins. Membership is the platform's,
//! so these run against the mock platform's `member_teams`.

mod common;

use common::{add_member, harness, onboard, org_with_owner, Account, Call, Harness};
use http::StatusCode;
use savvagent_resource::Role;
use savvagent_tenant::ids::OrgId;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower_core::jobs::JobsExt;
use tower_core::leases::LeasesExt;
use uuid::Uuid;

struct World {
    h: Harness,
    org: OrgId,
    rob: Account,
    /// On team `platform` only.
    alice: Account,
    /// On team `growth` only.
    bob: Account,
    /// On no team.
    carol: Account,
}

/// An org with two teams, one org-wide repo and one repo per team, each with one
/// job. `rob` owns the org.
async fn world(pool: PgPool) -> World {
    let h = harness(pool).await;
    let rob = onboard(&h, "rob@acme.test").await;
    let alice = onboard(&h, "alice@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;
    let carol = onboard(&h, "carol@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    for who in [&alice, &bob, &carol] {
        add_member(&h, org, who.user, Role::Member).await;
    }

    let (platform_team, growth_team) = (Uuid::new_v4(), Uuid::new_v4());
    h.platform
        .add_team(org.as_uuid(), platform_team, "platform", "Platform");
    h.platform
        .add_team(org.as_uuid(), growth_team, "growth", "Growth");
    h.platform
        .add_team_member(org.as_uuid(), alice.user.as_uuid(), platform_team);
    h.platform
        .add_team_member(org.as_uuid(), bob.user.as_uuid(), growth_team);

    for (slug, team) in [
        ("shared", None),
        ("platform-repo", Some(platform_team)),
        ("growth-repo", Some(growth_team)),
    ] {
        let reply = Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(json!({ "slug": slug, "teamId": team }))
            .send(&h.router)
            .await;
        reply.expect(StatusCode::CREATED);
        let repo = reply.body["id"].as_str().unwrap().parse().unwrap();
        let mut tx = h.db.begin(org).await.unwrap();
        tx.add_job(tower_core::jobs::NewJob {
            repo_id: repo,
            title: format!("job in {slug}"),
            created_by: Some(rob.user),
            ..Default::default()
        })
        .await
        .unwrap();
        tx.acquire_lease(repo, "branch:main", rob.user, None, None, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    World {
        h,
        org,
        rob,
        alice,
        bob,
        carol,
    }
}

async fn get(w: &World, who: &Account, uri: &str) -> common::Reply {
    Call::get(uri)
        .with_session(&who.session)
        .send(&w.h.router)
        .await
}

fn slugs(body: &Value) -> Vec<String> {
    let mut v: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["slug"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn titles(body: &Value) -> Vec<String> {
    let mut v: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn repos_are_listed_to_org_wide_plus_the_callers_teams(pool: PgPool) {
    let w = world(pool).await;

    let all = ["growth-repo", "platform-repo", "shared"];
    let r = get(&w, &w.rob, "/api/orgs/acme/repos").await;
    r.expect(StatusCode::OK);
    assert_eq!(slugs(&r.body), all, "an owner sees everything");

    let r = get(&w, &w.alice, "/api/orgs/acme/repos").await;
    r.expect(StatusCode::OK);
    assert_eq!(slugs(&r.body), ["platform-repo", "shared"]);

    let r = get(&w, &w.bob, "/api/orgs/acme/repos").await;
    r.expect(StatusCode::OK);
    assert_eq!(slugs(&r.body), ["growth-repo", "shared"]);

    let r = get(&w, &w.carol, "/api/orgs/acme/repos").await;
    r.expect(StatusCode::OK);
    assert_eq!(slugs(&r.body), ["shared"], "no team: org-wide only");
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_hidden_repo_is_not_found_and_not_named_anywhere(pool: PgPool) {
    let w = world(pool).await;

    for uri in [
        "/api/orgs/acme/repos/platform-repo",
        "/api/orgs/acme/repos/platform-repo/leases",
    ] {
        get(&w, &w.alice, uri).await.expect(StatusCode::OK);
        get(&w, &w.rob, uri).await.expect(StatusCode::OK);
        // Exactly what an unregistered slug says: the caller's own input
        // echoed back, and the repos they may see.
        let r = get(&w, &w.bob, uri).await;
        r.expect(StatusCode::NOT_FOUND);
        assert!(
            r.text
                .contains("Registered repos: \\\"growth-repo\\\", \\\"shared\\\"."),
            "{}",
            r.text
        );
    }

    let r = get(&w, &w.bob, "/api/orgs/acme/repos/nope").await;
    r.expect(StatusCode::NOT_FOUND);
    let r = get(&w, &w.bob, "/api/orgs/acme/jobs?repo=platform-repo").await;
    r.expect(StatusCode::NOT_FOUND);
    assert!(r
        .text
        .contains("Registered repos: \\\"growth-repo\\\", \\\"shared\\\"."));
    let r = get(&w, &w.rob, "/api/orgs/acme/jobs?repo=nope").await;
    assert!(
        r.text
            .contains("\\\"growth-repo\\\", \\\"platform-repo\\\", \\\"shared\\\""),
        "{}",
        r.text
    );
}

/// A remote already owned by a hidden repo must not name that repo: not when a
/// member registers a repo with it, and not when one adds it to a repo they
/// can see. An owner is told which repo it is.
///
/// Today both writes are admin-only, so this test can only observe the admin
/// gate: the member is refused `403` before the conflict is found. The member's
/// status is pinned to exactly that so the test fails, on purpose, if the gate
/// is ever relaxed, and whoever relaxes it has to decide what a team-limited
/// caller should see here. The redaction itself (`TeamScope::redact_foreign_ids`)
/// is pinned by the unit tests in `tower-core` and in `tower-web/src/error.rs`.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_taken_remote_never_names_a_hidden_repo(pool: PgPool) {
    let w = world(pool).await;
    let remote = "git@github.com:acme/platform.git";
    Call::patch("/api/orgs/acme/repos/platform-repo")
        .with_session(&w.rob.session)
        .json(json!({ "addRemotes": [remote] }))
        .send(&w.h.router)
        .await
        .expect(StatusCode::OK);

    let register = |who: &Account| {
        Call::post("/api/orgs/acme/repos")
            .with_session(&who.session)
            .json(json!({ "slug": "copycat", "remotes": [remote] }))
    };
    let add_to_shared = |who: &Account| {
        Call::patch("/api/orgs/acme/repos/shared")
            .with_session(&who.session)
            .json(json!({ "addRemotes": [remote] }))
    };

    for call in [register(&w.bob), add_to_shared(&w.bob)] {
        let r = call.send(&w.h.router).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text);
        assert!(!r.text.contains("platform-repo"), "{}", r.text);
    }

    for call in [register(&w.rob), add_to_shared(&w.rob)] {
        let r = call.send(&w.h.router).await;
        r.expect(StatusCode::CONFLICT);
        assert_eq!(r.body["error"]["code"], "remote_taken", "{}", r.text);
        assert!(r.text.contains("platform-repo"), "{}", r.text);
    }
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn jobs_are_scoped_to_org_wide_plus_the_callers_teams(pool: PgPool) {
    let w = world(pool).await;

    let r = get(&w, &w.rob, "/api/orgs/acme/jobs").await;
    r.expect(StatusCode::OK);
    assert_eq!(
        titles(&r.body),
        [
            "job in growth-repo",
            "job in platform-repo",
            "job in shared"
        ]
    );

    let r = get(&w, &w.alice, "/api/orgs/acme/jobs").await;
    assert_eq!(titles(&r.body), ["job in platform-repo", "job in shared"]);
    let r = get(&w, &w.bob, "/api/orgs/acme/jobs").await;
    assert_eq!(titles(&r.body), ["job in growth-repo", "job in shared"]);
    let r = get(&w, &w.carol, "/api/orgs/acme/jobs").await;
    assert_eq!(titles(&r.body), ["job in shared"]);

    // Asking for a team you are not on is not a way in.
    let r = get(&w, &w.bob, "/api/orgs/acme/jobs?team=platform").await;
    r.expect(StatusCode::OK);
    assert!(r.body.as_array().unwrap().is_empty());

    // The limit applies after the team filter.
    let r = get(&w, &w.carol, "/api/orgs/acme/jobs?limit=1").await;
    assert_eq!(titles(&r.body), ["job in shared"]);

    // Counters follow the same rule.
    let stats = get(&w, &w.rob, "/api/orgs/acme/jobs/stats").await;
    assert_eq!(stats.body["pending"], 3);
    let stats = get(&w, &w.alice, "/api/orgs/acme/jobs/stats").await;
    assert_eq!(stats.body["pending"], 2);
    assert_eq!(stats.body["total"], 2);
    let stats = get(&w, &w.carol, "/api/orgs/acme/jobs/stats").await;
    assert_eq!(stats.body["pending"], 1);
    let stats = get(&w, &w.carol, "/api/orgs/acme/jobs/stats?repo=platform-repo").await;
    stats.expect(StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_hidden_job_is_not_found(pool: PgPool) {
    let w = world(pool).await;
    let all = get(&w, &w.rob, "/api/orgs/acme/jobs").await;
    let id_of = |title: &str| -> String {
        all.body
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["title"] == title)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let platform_job = format!("/api/orgs/acme/jobs/{}", id_of("job in platform-repo"));
    let shared_job = format!("/api/orgs/acme/jobs/{}", id_of("job in shared"));

    get(&w, &w.alice, &platform_job)
        .await
        .expect(StatusCode::OK);
    get(&w, &w.rob, &platform_job).await.expect(StatusCode::OK);
    get(&w, &w.bob, &platform_job)
        .await
        .expect(StatusCode::NOT_FOUND);
    get(&w, &w.carol, &platform_job)
        .await
        .expect(StatusCode::NOT_FOUND);
    for who in [&w.alice, &w.bob, &w.carol] {
        get(&w, who, &shared_job).await.expect(StatusCode::OK);
    }
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_deleted_team_matches_nobody(pool: PgPool) {
    let w = world(pool).await;
    let dave = onboard(&w.h, "dave@acme.test").await;
    add_member(&w.h, w.org, dave.user, Role::Member).await;

    // Dave is on a team that the platform then deletes: its repo keeps the
    // dangling id, the platform's answer no longer lists the team, and so the
    // repo is visible to administrators only.
    let ghost = Uuid::new_v4();
    w.h.platform
        .add_team(w.org.as_uuid(), ghost, "ghost", "Ghost");
    w.h.platform
        .add_team_member(w.org.as_uuid(), dave.user.as_uuid(), ghost);
    Call::post("/api/orgs/acme/repos")
        .with_session(&w.rob.session)
        .json(json!({ "slug": "ghost-repo", "teamId": ghost }))
        .send(&w.h.router)
        .await
        .expect(StatusCode::CREATED);
    w.h.platform.remove_team(w.org.as_uuid(), ghost);

    let r = get(&w, &dave, "/api/orgs/acme/repos").await;
    assert_eq!(slugs(&r.body), ["shared"]);
    get(&w, &dave, "/api/orgs/acme/repos/ghost-repo")
        .await
        .expect(StatusCode::NOT_FOUND);
    let r = get(&w, &w.rob, "/api/orgs/acme/repos").await;
    assert!(slugs(&r.body).contains(&"ghost-repo".to_string()));
}

/// A platform that cannot say which teams someone is on is a `503`, never "all
/// teams" and never "no filter".
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_platform_outage_is_503_never_a_leak(pool: PgPool) {
    let w = world(pool).await;
    let erin = onboard(&w.h, "erin@acme.test").await;
    add_member(&w.h, w.org, erin.user, Role::Member).await;

    // Warm the caches that identify erin (introspection, membership) with a call
    // that fails before it needs her teams, so only the team lookup is left to
    // fail.
    get(&w, &erin, "/api/orgs/acme/jobs?status=bogus")
        .await
        .expect(StatusCode::BAD_REQUEST);

    w.h.platform.set_down(true);
    for uri in [
        "/api/orgs/acme/repos",
        "/api/orgs/acme/repos/platform-repo",
        "/api/orgs/acme/repos/platform-repo/leases",
        "/api/orgs/acme/jobs",
        "/api/orgs/acme/jobs/stats",
    ] {
        let r = get(&w, &erin, uri).await;
        r.expect(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.error_code(), Some("platform_unavailable"), "{uri}");
        assert!(
            !r.text.contains("job in") && !r.text.contains("platform-repo"),
            "{uri} leaked: {}",
            r.text
        );
    }
    w.h.platform.set_down(false);
    get(&w, &erin, "/api/orgs/acme/jobs")
        .await
        .expect(StatusCode::OK);
}

/// A job with no team of its own, in a repo scoped to a team afterwards, is that
/// team's work: hidden from list, detail and counters; and a dependency on a
/// hidden job is not listed back to someone who could not open it.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_teamless_job_in_a_team_repo_is_hidden_and_so_are_dependency_ids(pool: PgPool) {
    use tower_core::repos::{NewRepo, RepoPatch, ReposExt};
    let w = world(pool).await;
    let client = w.h.platform.client();
    let team = {
        let t = client
            .team_by_slug(w.org.as_uuid(), "platform")
            .await
            .unwrap()
            .unwrap();
        tower_core::teams::VerifiedTeam::verify(&client, w.org, t.id.into())
            .await
            .unwrap()
    };

    let (late_job, shared_job) = {
        let mut tx = w.h.db.begin(w.org).await.unwrap();
        let repo = tx
            .register_repo(NewRepo {
                slug: "late".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let late = tx
            .add_job(tower_core::jobs::NewJob {
                repo_id: repo.id,
                title: "job in late".into(),
                created_by: Some(w.rob.user),
                ..Default::default()
            })
            .await
            .unwrap();
        tx.update_repo(
            repo.id,
            RepoPatch {
                team_id: Some(Some(team)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let shared = tx
            .list_jobs(&tower_core::jobs::JobFilter::default())
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.title == "job in shared")
            .unwrap();
        // The shared job waits on the team's job.
        tx.set_dependencies(&shared.id, std::slice::from_ref(&late.id), &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        (late.id.to_string(), shared.id.to_string())
    };

    for who in [&w.bob, &w.carol] {
        let r = get(&w, who, "/api/orgs/acme/jobs").await;
        assert!(!titles(&r.body).contains(&"job in late".to_string()));
        get(&w, who, &format!("/api/orgs/acme/jobs/{late_job}"))
            .await
            .expect(StatusCode::NOT_FOUND);
        let r = get(&w, who, &format!("/api/orgs/acme/jobs/{shared_job}")).await;
        r.expect(StatusCode::OK);
        assert_eq!(
            r.body["dependsOn"],
            json!([]),
            "a hidden dependency's id is not handed out"
        );
    }
    // Counters agree with the list: rob 4, alice 3, carol 1.
    let total = |r: common::Reply| r.body["total"].as_i64().unwrap();
    assert_eq!(total(get(&w, &w.rob, "/api/orgs/acme/jobs/stats").await), 4);
    assert_eq!(
        total(get(&w, &w.alice, "/api/orgs/acme/jobs/stats").await),
        3
    );
    assert_eq!(
        total(get(&w, &w.carol, "/api/orgs/acme/jobs/stats").await),
        1
    );

    let r = get(&w, &w.rob, &format!("/api/orgs/acme/jobs/{shared_job}")).await;
    assert_eq!(r.body["dependsOn"], json!([late_job]));
}

/// A lease on a visible repo does not name a job the caller cannot see.
#[sqlx::test(migrations = "../tower-core/migrations")]
async fn a_lease_does_not_name_a_hidden_job(pool: PgPool) {
    let w = world(pool).await;
    let all = get(&w, &w.rob, "/api/orgs/acme/jobs").await;
    let hidden = all
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["title"] == "job in platform-repo")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    {
        let shared = get(&w, &w.rob, "/api/orgs/acme/repos/shared").await;
        let repo = shared.body["id"].as_str().unwrap().parse().unwrap();
        let mut tx = w.h.db.begin(w.org).await.unwrap();
        tx.acquire_lease(
            repo,
            "branch:job",
            w.rob.user,
            None,
            Some(&hidden.clone().into()),
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let job_of = |r: &common::Reply| {
        r.body
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["resource"] == "branch:job")
            .unwrap()["jobId"]
            .clone()
    };
    let uri = "/api/orgs/acme/repos/shared/leases";
    assert_eq!(job_of(&get(&w, &w.rob, uri).await), json!(hidden));
    assert_eq!(job_of(&get(&w, &w.alice, uri).await), json!(hidden));
    assert_eq!(job_of(&get(&w, &w.carol, uri).await), Value::Null);
}

#[sqlx::test(migrations = "../tower-core/migrations")]
async fn registering_a_repo_with_a_bad_slug_is_a_400(pool: PgPool) {
    let w = world(pool).await;
    let r = Call::post("/api/orgs/acme/repos")
        .with_session(&w.rob.session)
        .json(json!({ "slug": "has space" }))
        .send(&w.h.router)
        .await;
    r.expect(StatusCode::BAD_REQUEST);
    assert_eq!(r.body["error"]["code"], "invalid_repo_slug", "{:?}", r.body);
}
