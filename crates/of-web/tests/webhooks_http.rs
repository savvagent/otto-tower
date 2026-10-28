use of_core::jobs::JobsExt;
use of_core::repos::ReposExt;

mod common;

use axum::body::Body;
use common::Harness;
use hmac::{Hmac, Mac};
use http::{Request, StatusCode};
use of_core::jobs::Tracker;
use of_core::repos::NewRepo;
use of_core::trackers::{delete_connection, upsert_binding, upsert_connection, Provider};

use otto_tenant::ids::{OrgId, UserId};
use sha2::Sha256;
use sqlx::PgPool;
use tower::ServiceExt;

type HmacSha256 = Hmac<Sha256>;

const GITHUB_SECRET: &str = "github-secret";
const JIRA_SECRET: &str = "jira-secret";
const GITHUB_ISSUES_FIXTURE: &[u8] =
    include_bytes!("../../of-trackers/tests/fixtures/github-issues.json");
const JIRA_FIXTURE: &[u8] = include_bytes!("../../of-trackers/tests/fixtures/jira-automation.json");
const UNKNOWN_GITHUB_INSTALLATION_FIXTURE: &[u8] = br#"{
  "action":"opened",
  "installation":{"id":999999},
  "repository":{"full_name":"acme/api"},
  "issue":{"id":7001,"number":17,"title":"Implement webhook ingest","body":"Wire webhook verification into the tracker sync flow.","state":"open","labels":[{"name":"bug"},{"name":"trackers"}]}
}"#;

/// An id standing in for a platform record (orgs and users are the platform's).
struct Fx<T> {
    id: T,
}

async fn harness(pool: PgPool) -> Harness {
    common::harness_with(pool, |config| {
        config.github_app_webhook_secret = Some(GITHUB_SECRET.into());
    })
    .await
}

#[sqlx::test(migrations = "../of-core/migrations")]
async fn github_webhooks_are_public_and_acknowledged_when_verified(pool: PgPool) {
    let h = harness(pool).await;
    let org = Fx {
        id: otto_tenant::ids::OrgId::new(),
    };

    let mut tx = h.db.begin(org.id).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some("Acme API".into()),
            remotes: vec!["git@github.com:acme/api.git".into()],
            created_by: Some(otto_tenant::ids::UserId::new()),
            ..Default::default()
        })
        .await
        .unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "123456", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        repo.id,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let reply = post_webhook(
        &h,
        "/webhooks/github",
        GITHUB_ISSUES_FIXTURE,
        &[
            ("x-github-event", "issues"),
            (
                "x-hub-signature-256",
                &format!("sha256={}", github_signature(GITHUB_ISSUES_FIXTURE)),
            ),
        ],
    )
    .await;

    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(body_json(reply).await["ok"], serde_json::Value::Bool(true));
}

#[sqlx::test(migrations = "../of-core/migrations")]
async fn github_rejection_and_unknown_connection_share_the_same_public_404(pool: PgPool) {
    let h = harness(pool).await;
    let org = Fx {
        id: otto_tenant::ids::OrgId::new(),
    };

    let mut tx = h.db.begin(org.id).await.unwrap();
    upsert_connection(&mut tx, Provider::Github, "123456", None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let invalid_signature = post_webhook(
        &h,
        "/webhooks/github",
        GITHUB_ISSUES_FIXTURE,
        &[
            ("x-github-event", "issues"),
            ("x-hub-signature-256", "sha256=deadbeef"),
        ],
    )
    .await;
    assert_eq!(invalid_signature.status(), StatusCode::NOT_FOUND);

    let unknown_installation = post_webhook(
        &h,
        "/webhooks/github",
        UNKNOWN_GITHUB_INSTALLATION_FIXTURE,
        &[
            ("x-github-event", "issues"),
            (
                "x-hub-signature-256",
                &format!(
                    "sha256={}",
                    github_signature(UNKNOWN_GITHUB_INSTALLATION_FIXTURE)
                ),
            ),
        ],
    )
    .await;

    assert_eq!(unknown_installation.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_text(invalid_signature).await,
        body_text(unknown_installation).await
    );
}

#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_labelled_issue_webhook_creates_one_job_and_a_stale_redelivery_does_not_duplicate_it(
    pool: PgPool,
) {
    let h = harness(pool).await;
    let org = Fx {
        id: otto_tenant::ids::OrgId::new(),
    };

    let mut tx = h.db.begin(org.id).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some("Acme API".into()),
            remotes: vec!["git@github.com:acme/api.git".into()],
            created_by: Some(otto_tenant::ids::UserId::new()),
            ..Default::default()
        })
        .await
        .unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "123456", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        repo.id,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // The delivery record is forgotten before the redelivery, as it is after the
    // retention window, so it is the job's revision guard that drops it.
    for i in 0..2 {
        if i == 1 {
            forget_deliveries(&h).await;
        }
        let reply = post_webhook(
            &h,
            "/webhooks/github",
            GITHUB_ISSUES_FIXTURE,
            &[
                ("x-github-event", "issues"),
                (
                    "x-hub-signature-256",
                    &format!("sha256={}", github_signature(GITHUB_ISSUES_FIXTURE)),
                ),
            ],
        )
        .await;
        assert_eq!(reply.status(), StatusCode::OK);
    }

    let mut tx = h.db.begin(org.id).await.unwrap();
    let job = tx
        .get_job_by_ticket_for_repo(repo.id, Tracker::Github, "acme/api#17")
        .await
        .unwrap()
        .expect("synced job");
    let jobs = tx
        .list_jobs(&of_core::jobs::JobFilter {
            repo_id: Some(repo.id),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(job.title, "Implement webhook ingest");
    assert_eq!(job.remote_revision.as_deref(), Some("2026-09-03T18:00:00Z"));
    assert_eq!(jobs.len(), 1);
}

/// The fixture GitHub installation, and a second one for a second org.
const INSTALLATION: u64 = 123456;
const OTHER_INSTALLATION: u64 = 654321;

/// Register a repo in `org` bound to `installation`'s `full_name` with the
/// `trackers` label, so an opened issue there syncs in as a pending job.
async fn bind_github_repo(h: &Harness, org: OrgId, installation: u64, full_name: &str) {
    let mut tx = h.db.begin(org).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some("Acme API".into()),
            remotes: vec![format!("git@github.com:{full_name}.git")],
            created_by: Some(UserId::new()),
            ..Default::default()
        })
        .await
        .unwrap();
    let connection = upsert_connection(
        &mut tx,
        Provider::Github,
        &installation.to_string(),
        None,
        None,
    )
    .await
    .unwrap();
    upsert_binding(
        &mut tx,
        repo.id,
        Some(connection.id),
        Provider::Github,
        full_name,
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// A GitHub `issues` event for issue `number` in `full_name`.
fn github_issue(
    installation: u64,
    full_name: &str,
    number: u64,
    action: &str,
    state_reason: Option<&str>,
    updated_at: &str,
) -> Vec<u8> {
    let state = if action == "closed" { "closed" } else { "open" };
    let state_reason = state_reason.map_or("null".to_string(), |r| format!("\"{r}\""));
    format!(
        r#"{{
  "action":"{action}",
  "installation":{{"id":{installation}}},
  "repository":{{"full_name":"{full_name}"}},
  "issue":{{"id":{id},"number":{number},"title":"Issue {number}","body":"x","state":"{state}","updated_at":"{updated_at}","state_reason":{state_reason},"labels":[{{"name":"trackers"}}]}}
}}"#,
        id = 7000 + number,
    )
    .into_bytes()
}

async fn post_github_issue(h: &Harness, body: &[u8]) -> http::Response<Body> {
    post_webhook(
        h,
        "/webhooks/github",
        body,
        &[
            ("x-github-event", "issues"),
            (
                "x-hub-signature-256",
                &format!("sha256={}", github_signature(body)),
            ),
        ],
    )
    .await
}

/// Open issue `number` in `full_name`, which syncs it in as a pending job, and
/// return that job.
async fn open_issue(
    h: &Harness,
    org: OrgId,
    installation: u64,
    full_name: &str,
    number: u64,
) -> of_core::jobs::Job {
    let opened = github_issue(
        installation,
        full_name,
        number,
        "opened",
        None,
        "2026-09-03T18:00:00Z",
    );
    assert_eq!(post_github_issue(h, &opened).await.status(), StatusCode::OK);
    job_for(h, org, full_name, number).await
}

/// Close issue `number` as `state_reason`, asserting the delivery succeeded.
async fn close_issue(h: &Harness, installation: u64, full_name: &str, number: u64, reason: &str) {
    let closed = github_issue(
        installation,
        full_name,
        number,
        "closed",
        Some(reason),
        "2026-09-04T09:00:00Z",
    );
    assert_eq!(post_github_issue(h, &closed).await.status(), StatusCode::OK);
}

async fn job_for(h: &Harness, org: OrgId, full_name: &str, number: u64) -> of_core::jobs::Job {
    let mut tx = h.db.begin(org).await.unwrap();
    let repo = tx.get_repo_by_slug("api").await.unwrap().expect("repo");
    let job = tx
        .get_job_by_ticket_for_repo(repo.id, Tracker::Github, &format!("{full_name}#{number}"))
        .await
        .unwrap()
        .expect("job");
    tx.commit().await.unwrap();
    job
}

/// Claim `job` for `agent`, as `claim_jobs` would.
async fn claim(h: &Harness, org: OrgId, job: &of_core::jobs::Job, agent: UserId) {
    let mut tx = h.db.begin(org).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&job.id), agent, None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// One usage row recording a completed job.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct CompletionRow {
    tool: String,
    billable: bool,
    meter: Option<String>,
    quantity: Option<i64>,
    user_id: Option<uuid::Uuid>,
}

/// Every usage row recording a completed job, in the order written.
async fn completion_rows(pool: &PgPool) -> Vec<CompletionRow> {
    sqlx::query_as(
        "SELECT tool, billable, meter, quantity, user_id FROM usage_outbox \
         WHERE tool IN ('complete_job', 'inbound_ticket_close') ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn billed_close() -> CompletionRow {
    CompletionRow {
        tool: "inbound_ticket_close".into(),
        billable: true,
        meter: Some("otto-tower/jobs_completed".into()),
        quantity: Some(1),
        user_id: None,
    }
}

fn unbilled_close() -> CompletionRow {
    CompletionRow {
        tool: "inbound_ticket_close".into(),
        billable: false,
        meter: None,
        quantity: None,
        user_id: None,
    }
}

/// An agent claimed the job, and the ticket closed as done: that is a
/// completed job, billed once however often the close is delivered.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn an_inbound_close_of_a_claimed_job_bills_it_exactly_once(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    claim(&h, org, &job, UserId::new()).await;

    // The redelivery is dropped by the delivery dedupe; with its record
    // forgotten (as after the retention window) it is dropped as stale. Either
    // way the close records once.
    for i in 0..3 {
        if i == 2 {
            forget_deliveries(&h).await;
        }
        close_issue(&h, INSTALLATION, "acme/api", 17, "completed").await;
    }

    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.status,
        of_core::jobs::Status::Completed
    );
    assert_eq!(completion_rows(&pool).await, vec![billed_close()]);
}

/// The platform is asked for the org's standing only when the answer is used:
/// a verified close to `completed` of a claimed job. Opening, updating or
/// failing a ticket, and closing a job nobody claimed, never ask.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn the_standing_is_looked_up_only_for_a_close_of_a_claimed_job(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;

    open_issue(&h, org, INSTALLATION, "acme/api", 1).await;
    assert_eq!(h.platform.status_calls(), 0, "an open asks nothing");
    // Never claimed: completed unbilled, without a lookup.
    close_issue(&h, INSTALLATION, "acme/api", 1, "completed").await;
    assert_eq!(h.platform.status_calls(), 0, "a never-claimed close");
    // Claimed, closed as not planned: fails the job, no completion.
    let dropped = open_issue(&h, org, INSTALLATION, "acme/api", 2).await;
    claim(&h, org, &dropped, UserId::new()).await;
    close_issue(&h, INSTALLATION, "acme/api", 2, "not_planned").await;
    assert_eq!(h.platform.status_calls(), 0, "a not-planned close");

    // Claimed and closed as done: the one case that asks, once.
    let claimed = open_issue(&h, org, INSTALLATION, "acme/api", 3).await;
    claim(&h, org, &claimed, UserId::new()).await;
    close_issue(&h, INSTALLATION, "acme/api", 3, "completed").await;
    assert_eq!(h.platform.status_calls(), 1);
    assert_eq!(
        completion_rows(&pool).await,
        vec![unbilled_close(), billed_close()]
    );
}

/// A request that fails verification never reaches the platform, whichever
/// provider it names: an unauthenticated caller cannot spend lookups.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn an_unverified_request_makes_no_status_lookup(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    let sealed_secret = h.cipher.seal(JIRA_SECRET.as_bytes()).unwrap();
    let mut tx = h.db.begin(org).await.unwrap();
    upsert_connection(
        &mut tx,
        Provider::Jira,
        "cloud-123",
        None,
        Some(&sealed_secret),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let wrong = post_webhook(
        &h,
        "/webhooks/jira?site=cloud-123",
        JIRA_FIXTURE,
        &[("x-of-webhook-secret", "wrong-secret")],
    )
    .await;
    let missing = post_webhook(&h, "/webhooks/jira?site=cloud-123", JIRA_FIXTURE, &[]).await;
    let forged_github = post_webhook(
        &h,
        "/webhooks/github",
        b"{}",
        &[("x-hub-signature-256", "sha256=00")],
    )
    .await;

    assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(forged_github.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.platform.status_calls(), 0);
}

/// The connection a request was verified against is the one it is applied
/// under. If it is replaced while the standing is being looked up, the
/// verification no longer stands: the event is dropped, not applied.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_connection_replaced_during_the_standing_lookup_drops_the_event(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    claim(&h, org, &job, UserId::new()).await;
    h.platform.set_status_delay_ms(800);

    let closed = github_issue(
        INSTALLATION,
        "acme/api",
        17,
        "closed",
        Some("completed"),
        "2026-09-04T09:00:00Z",
    );
    let request = post_github_issue(&h, &closed);
    let replace = async {
        // Wait for pass 1 to reach the lookup, then swap the connection (a
        // new row, so a new id) and point the repo binding at it.
        while h.platform.status_calls() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut tx = h.db.begin(org).await.unwrap();
        let repo = tx.get_repo_by_slug("api").await.unwrap().expect("repo");
        delete_connection(&mut tx, Provider::Github).await.unwrap();
        let connection = upsert_connection(
            &mut tx,
            Provider::Github,
            &INSTALLATION.to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        upsert_binding(
            &mut tx,
            repo.id,
            Some(connection.id),
            Provider::Github,
            "acme/api",
            "trackers",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    };
    let (response, ()) = tokio::join!(request, replace);

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.status,
        of_core::jobs::Status::InProgress,
        "the close must not be applied under a connection it was not verified against"
    );
    assert_eq!(completion_rows(&pool).await, vec![]);
}

/// Billing follows claim history, not the status at the close: a job an
/// agent claimed and then handed back (`repend_job`, or a claim left to
/// expire) was worked, so closing its ticket from `pending` is still billed.
/// Otherwise an agent could repend and close to complete work unbilled.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn an_inbound_close_of_a_job_claimed_then_repended_is_billed(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    claim(&h, org, &job, UserId::new()).await;
    let mut tx = h.db.begin(org).await.unwrap();
    tx.repend_job(&job.id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.status,
        of_core::jobs::Status::Pending
    );

    close_issue(&h, INSTALLATION, "acme/api", 17, "completed").await;

    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.status,
        of_core::jobs::Status::Completed
    );
    assert_eq!(completion_rows(&pool).await, vec![billed_close()]);
}

/// Nobody ever claimed a pending job (`attempts` is 0), so closing its
/// ticket (by anyone, as anything but `not_planned`) completes it without
/// billing the org.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn an_inbound_close_of_a_never_claimed_job_completes_it_unbilled(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    for i in 0..2 {
        if i == 1 {
            forget_deliveries(&h).await;
        }
        close_issue(&h, INSTALLATION, "acme/api", 17, "completed").await;
    }

    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.status,
        of_core::jobs::Status::Completed
    );
    assert_eq!(completion_rows(&pool).await, vec![unbilled_close()]);
}

/// The budget's hard stop holds on the tracker path too: an agent refused at
/// `complete_job` cannot get the job billed by closing its ticket instead.
/// The close still applies (the ticket is closed); it is recorded unbilled.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_blocked_orgs_inbound_close_completes_the_job_unbilled(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    h.platform
        .set_budget(org.as_uuid(), Some(of_testkit::tripped_budget()));
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    claim(&h, org, &job, UserId::new()).await;

    close_issue(&h, INSTALLATION, "acme/api", 17, "completed").await;

    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.status,
        of_core::jobs::Status::Completed
    );
    assert_eq!(completion_rows(&pool).await, vec![unbilled_close()]);
}

#[sqlx::test(migrations = "../of-core/migrations")]
async fn an_inbound_close_as_not_planned_fails_the_job_and_records_no_completion(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    claim(&h, org, &job, UserId::new()).await;

    close_issue(&h, INSTALLATION, "acme/api", 17, "not_planned").await;

    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.status,
        of_core::jobs::Status::Failed
    );
    assert!(completion_rows(&pool).await.is_empty());
}

/// A claimed job whose issue is closed as a duplicate was not delivered under
/// that issue, so it must not be billed as a completion.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn an_inbound_close_as_duplicate_fails_the_job_and_records_no_completion(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    claim(&h, org, &job, UserId::new()).await;

    close_issue(&h, INSTALLATION, "acme/api", 17, "duplicate").await;

    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.status,
        of_core::jobs::Status::Failed
    );
    assert!(completion_rows(&pool).await.is_empty());
}

/// Complete `job` the way `complete_job` does: warm the org's standing, then
/// charge and complete in one transaction. `false` if it was refused (and so
/// wrote nothing).
async fn complete_as_agent(
    h: &Harness,
    meter: &of_billing::Meter,
    org: OrgId,
    job: &of_core::jobs::Job,
    agent: UserId,
) -> bool {
    let warmed = meter.warm(org).await;
    let mut tx = h.db.begin(org).await.unwrap();
    if meter
        .charge_completion(&mut tx, agent, &warmed)
        .await
        .is_err()
    {
        return false;
    }
    tx.complete_job(&job.id, agent, None, None).await.unwrap();
    tx.commit().await.unwrap();
    true
}

/// A job already completed through `complete_job` was recorded by that call.
/// A later close event (newer `updated_at`, so not stale) finds a terminal
/// job, is a no-op, and must not write a second row.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_later_close_of_a_job_completed_by_complete_job_records_nothing_more(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    let meter = of_billing::Meter::new(h.platform.client(), "https://x.test");
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    let agent = UserId::new();
    claim(&h, org, &job, agent).await;
    assert!(complete_as_agent(&h, &meter, org, &job, agent).await);

    close_issue(&h, INSTALLATION, "acme/api", 17, "completed").await;

    let rows = completion_rows(&pool).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].tool, "complete_job");
    assert!(rows[0].billable);
}

/// The parity guard: however a job reaches `completed`, it writes exactly one
/// completion row, so `org_counters.jobs_completed_total` always equals the
/// number of completion rows. A path that completes a job without recording
/// it (or records one twice) breaks this.
///
/// It covers the scenarios listed below, driving `complete_job`'s path through
/// the same `of-billing` call the tool makes and the tracker-close path through
/// the webhook. It cannot see a new completion path added in `of-core` on its
/// own: such a path trips it only once a scenario here exercises it, so add one
/// when adding the path.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn every_completion_writes_exactly_one_completion_row(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let meter = of_billing::Meter::new(h.platform.client(), "https://x.test");
    let agent = UserId::new();

    let open = OrgId::new();
    bind_github_repo(&h, open, INSTALLATION, "acme/api").await;
    // complete_job, then a later close of the same ticket: one billed row.
    let done = open_issue(&h, open, INSTALLATION, "acme/api", 1).await;
    claim(&h, open, &done, agent).await;
    assert!(complete_as_agent(&h, &meter, open, &done, agent).await);
    close_issue(&h, INSTALLATION, "acme/api", 1, "completed").await;
    // A claimed job closed as done: billed.
    let claimed = open_issue(&h, open, INSTALLATION, "acme/api", 2).await;
    claim(&h, open, &claimed, agent).await;
    close_issue(&h, INSTALLATION, "acme/api", 2, "completed").await;
    // A never-claimed job closed: completed, unbilled.
    open_issue(&h, open, INSTALLATION, "acme/api", 3).await;
    close_issue(&h, INSTALLATION, "acme/api", 3, "completed").await;
    // Closed as not planned: failed, no completion.
    let dropped = open_issue(&h, open, INSTALLATION, "acme/api", 4).await;
    claim(&h, open, &dropped, agent).await;
    close_issue(&h, INSTALLATION, "acme/api", 4, "not_planned").await;

    let blocked = OrgId::new();
    h.platform
        .set_budget(blocked.as_uuid(), Some(of_testkit::tripped_budget()));
    bind_github_repo(&h, blocked, OTHER_INSTALLATION, "acme/web").await;
    // complete_job is refused outright: nothing completes, nothing written.
    let refused = open_issue(&h, blocked, OTHER_INSTALLATION, "acme/web", 5).await;
    claim(&h, blocked, &refused, agent).await;
    assert!(!complete_as_agent(&h, &meter, blocked, &refused, agent).await);
    // The same job closed in the tracker instead: completed, unbilled.
    close_issue(&h, OTHER_INSTALLATION, "acme/web", 5, "completed").await;

    let mut completed = 0;
    for org in [open, blocked] {
        let mut tx = h.db.begin(org).await.unwrap();
        completed += tx.stats(None).await.unwrap().completed;
        tx.commit().await.unwrap();
    }
    let rows = completion_rows(&pool).await;
    assert_eq!(completed, 4);
    assert_eq!(rows.len() as i64, completed, "{rows:?}");
    assert_eq!(
        rows.iter().filter(|r| r.billable).count(),
        2,
        "only complete_job and the claimed close are billed: {rows:?}"
    );
    assert!(
        rows.iter()
            .all(|r| r.billable == (r.meter.as_deref() == Some("otto-tower/jobs_completed"))),
        "a row is billed exactly when it carries the meter: {rows:?}"
    );
}

#[sqlx::test(migrations = "../of-core/migrations")]
async fn jira_webhooks_require_the_site_and_secret_but_acknowledge_a_valid_request(pool: PgPool) {
    let h = harness(pool).await;
    let org = Fx {
        id: otto_tenant::ids::OrgId::new(),
    };
    let sealed_secret = h.cipher.seal(JIRA_SECRET.as_bytes()).unwrap();

    let mut tx = h.db.begin(org.id).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some("Globex API".into()),
            remotes: vec!["git@github.com:globex/api.git".into()],
            created_by: Some(otto_tenant::ids::UserId::new()),
            ..Default::default()
        })
        .await
        .unwrap();
    let connection = upsert_connection(
        &mut tx,
        Provider::Jira,
        "cloud-123",
        None,
        Some(&sealed_secret),
    )
    .await
    .unwrap();
    upsert_binding(
        &mut tx,
        repo.id,
        Some(connection.id),
        Provider::Jira,
        "DF",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let ok = post_webhook(
        &h,
        "/webhooks/jira?site=cloud-123",
        JIRA_FIXTURE,
        &[("x-of-webhook-secret", JIRA_SECRET)],
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);

    let wrong_secret = post_webhook(
        &h,
        "/webhooks/jira?site=cloud-123",
        JIRA_FIXTURE,
        &[("x-of-webhook-secret", "wrong-secret")],
    )
    .await;
    let unknown_site = post_webhook(
        &h,
        "/webhooks/jira?site=cloud-999",
        JIRA_FIXTURE,
        &[("x-of-webhook-secret", JIRA_SECRET)],
    )
    .await;
    let missing_site = post_webhook(
        &h,
        "/webhooks/jira",
        JIRA_FIXTURE,
        &[("x-of-webhook-secret", JIRA_SECRET)],
    )
    .await;

    assert_eq!(wrong_secret.status(), StatusCode::NOT_FOUND);
    assert_eq!(unknown_site.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing_site.status(), StatusCode::NOT_FOUND);

    let wrong_body = body_text(wrong_secret).await;
    assert_eq!(wrong_body, body_text(unknown_site).await);
    assert_eq!(wrong_body, body_text(missing_site).await);
}

#[sqlx::test(migrations = "../of-core/migrations")]
async fn the_openapi_document_mentions_the_webhook_route(pool: PgPool) {
    let h = harness(pool).await;
    let doc = common::Call::get("/api/openapi.json").send(&h.router).await;
    doc.expect(StatusCode::OK);
    assert!(doc.body["paths"]["/webhooks/{provider}"]["post"].is_object());
}

async fn post_webhook(
    h: &Harness,
    uri: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> http::Response<Body> {
    let mut builder = Request::builder().method("POST").uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    h.router
        .clone()
        .oneshot(builder.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap()
}

async fn body_json(response: http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_text(response: http::Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
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

/// A GitHub `issue_comment` event on issue `number`, written by a bot when
/// `bot`, on an issue carrying `labels`.
fn github_comment(
    installation: u64,
    full_name: &str,
    number: u64,
    labels: &[&str],
    bot: bool,
    updated_at: &str,
) -> Vec<u8> {
    let user_type = if bot { "Bot" } else { "User" };
    let labels = labels
        .iter()
        .map(|l| format!(r#"{{"name":"{l}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{
  "action":"created",
  "installation":{{"id":{installation}}},
  "repository":{{"full_name":"{full_name}"}},
  "issue":{{"id":{id},"number":{number},"title":"Issue {number}","body":"x","state":"open","updated_at":"{updated_at}","state_reason":null,"labels":[{labels}]}},
  "comment":{{"id":9001,"body":"Also handle the empty case.","user":{{"login":"someone","type":"{user_type}"}}}}
}}"#,
        id = 7000 + number,
    )
    .into_bytes()
}

async fn post_github_comment(h: &Harness, body: &[u8]) -> http::Response<Body> {
    post_webhook(
        h,
        "/webhooks/github",
        body,
        &[
            ("x-github-event", "issue_comment"),
            (
                "x-hub-signature-256",
                &format!("sha256={}", github_signature(body)),
            ),
        ],
    )
    .await
}

/// A moment after the job was created: the fixtures' fixed 2026-09 dates are
/// older than any job a test creates, so they would never count as a change.
fn after_now() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A person commenting on a bound issue marks its live job stale, without
/// rewriting the job. The job's revision stays the issue's last applied edit
/// (see `a_late_issue_edit_older_than_a_comment_is_still_applied`), and a
/// redelivery of the same comment changes nothing more.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_human_comment_marks_the_issues_job_stale(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let before = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    assert!(!before.ticket_stale);

    let at = after_now();
    let comment = github_comment(INSTALLATION, "acme/api", 17, &["trackers"], false, &at);
    // The redelivery's record is forgotten first (as after the retention
    // window), so it is `inbound_decision`'s own guard that drops it.
    for i in 0..2 {
        if i == 1 {
            forget_deliveries(&h).await;
        }
        assert_eq!(
            post_github_comment(&h, &comment).await.status(),
            StatusCode::OK
        );
    }

    let after = job_for(&h, org, "acme/api", 17).await;
    assert!(after.ticket_stale);
    assert_eq!(
        after
            .ticket_changed_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        Some(at.clone())
    );
    assert_eq!(after.remote_revision, before.remote_revision);
    assert_eq!(after.title, before.title);
    assert_eq!(after.description, before.description);
}

/// A job relinked to another issue drops the old issue's mark, so a comment on
/// the new issue marks it even when that comment is earlier than the old mark
/// (which the comment guard would otherwise have dropped as already counted).
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_comment_on_a_relinked_jobs_new_issue_marks_it_whatever_the_old_mark(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    let (t1, t2) = (hours_from_now(1), hours_from_now(2));

    let old = github_comment(INSTALLATION, "acme/api", 17, &["trackers"], false, &t2);
    assert_eq!(post_github_comment(&h, &old).await.status(), StatusCode::OK);
    assert!(job_for(&h, org, "acme/api", 17).await.ticket_stale);

    let mut tx = h.db.begin(org).await.unwrap();
    let relinked = tx
        .link_ticket(&job.id, Tracker::Github, "acme/api#18")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(!relinked.ticket_stale);
    assert_eq!(relinked.ticket_changed_at, None);

    let new = github_comment(INSTALLATION, "acme/api", 18, &["trackers"], false, &t1);
    assert_eq!(post_github_comment(&h, &new).await.status(), StatusCode::OK);

    let after = job_for(&h, org, "acme/api", 18).await;
    assert_eq!(after.id, job.id);
    assert!(after.ticket_stale);
    assert_eq!(
        after
            .ticket_changed_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        Some(t1)
    );
}

/// An `issues` `edited` event for issue `number`, with `body` as the issue's
/// new body.
fn github_issue_edit(
    installation: u64,
    full_name: &str,
    number: u64,
    body: &str,
    updated_at: &str,
) -> Vec<u8> {
    let raw = github_issue(installation, full_name, number, "edited", None, updated_at);
    let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    v["issue"]["body"] = body.into();
    serde_json::to_vec(&v).unwrap()
}

fn hours_from_now(hours: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::hours(hours))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// An issue edit after a comment rewrites the description but keeps the
/// comment's mark: the comment is still not in the description, and only
/// `update_job` with a title or description says the issue was re-read.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn an_issue_edit_after_a_comment_rewrites_the_job_and_keeps_it_stale(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    let comment = github_comment(
        INSTALLATION,
        "acme/api",
        17,
        &["trackers"],
        false,
        &hours_from_now(1),
    );
    assert_eq!(
        post_github_comment(&h, &comment).await.status(),
        StatusCode::OK
    );
    let edit = github_issue_edit(INSTALLATION, "acme/api", 17, "new body", &hours_from_now(2));
    assert_eq!(post_github_issue(&h, &edit).await.status(), StatusCode::OK);

    let job = job_for(&h, org, "acme/api", 17).await;
    assert_eq!(job.description.as_deref(), Some("new body"));
    assert!(
        job.ticket_stale,
        "the edit must not clear the comment's mark"
    );
}

/// GitHub can deliver an issue edit after a comment made later than it. The
/// comment does not advance the job's revision, so the echo check does not
/// mistake the earlier edit for a stale event and drop it.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_late_issue_edit_older_than_a_comment_is_still_applied(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    let (t1, t2) = (hours_from_now(1), hours_from_now(2));
    let comment = github_comment(INSTALLATION, "acme/api", 17, &["trackers"], false, &t2);
    assert_eq!(
        post_github_comment(&h, &comment).await.status(),
        StatusCode::OK
    );
    let edit = github_issue_edit(INSTALLATION, "acme/api", 17, "edited at t1", &t1);
    assert_eq!(post_github_issue(&h, &edit).await.status(), StatusCode::OK);

    let job = job_for(&h, org, "acme/api", 17).await;
    assert_eq!(job.description.as_deref(), Some("edited at t1"));
    assert_eq!(job.remote_revision.as_deref(), Some(t1.as_str()));
    assert!(job.ticket_stale);
}

/// GitHub can also deliver a comment after an issue edit made later than it.
/// The edit moved the job's revision past the comment, but the comment is not
/// in the edited body either, so it still marks the job; its redelivery
/// changes nothing more.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_comment_delivered_after_a_later_issue_edit_still_marks_the_job(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    let (t1, t2) = (hours_from_now(1), hours_from_now(2));
    let edit = github_issue_edit(INSTALLATION, "acme/api", 17, "edited at t2", &t2);
    assert_eq!(post_github_issue(&h, &edit).await.status(), StatusCode::OK);
    let comment = github_comment(INSTALLATION, "acme/api", 17, &["trackers"], false, &t1);
    // The redelivery's record is forgotten first (as after the retention
    // window), so it is `inbound_decision`'s own guard that drops it.
    for i in 0..2 {
        if i == 1 {
            forget_deliveries(&h).await;
        }
        assert_eq!(
            post_github_comment(&h, &comment).await.status(),
            StatusCode::OK
        );
    }

    let job = job_for(&h, org, "acme/api", 17).await;
    assert!(job.ticket_stale);
    assert_eq!(
        job.ticket_changed_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        Some(t1)
    );
    assert_eq!(job.description.as_deref(), Some("edited at t2"));
    assert_eq!(job.remote_revision.as_deref(), Some(t2.as_str()));
}

/// A bot's comment (otto-tower's own write-back among them) is not a change
/// to the work.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_bot_comment_does_not_mark_the_job_stale(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    let comment = github_comment(
        INSTALLATION,
        "acme/api",
        17,
        &["trackers"],
        true,
        &after_now(),
    );
    assert_eq!(
        post_github_comment(&h, &comment).await.status(),
        StatusCode::OK
    );

    let job = job_for(&h, org, "acme/api", 17).await;
    assert!(!job.ticket_stale);
    assert_eq!(job.ticket_changed_at, None);
}

/// An issue without the binding's trigger label gets no webhook signal at all
/// (spec §5, "What the marker cannot see"): `sync_ticket` is its only refresh.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_comment_on_an_issue_without_the_trigger_label_is_not_seen(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    let comment = github_comment(INSTALLATION, "acme/api", 17, &["bug"], false, &after_now());
    assert_eq!(
        post_github_comment(&h, &comment).await.status(),
        StatusCode::OK
    );

    let job = job_for(&h, org, "acme/api", 17).await;
    assert!(!job.ticket_stale);
    assert_eq!(job.ticket_changed_at, None);
}

/// A comment on an issue with no job yet puts nothing on the queue.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_comment_on_an_issue_with_no_job_creates_none(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;

    let comment = github_comment(
        INSTALLATION,
        "acme/api",
        18,
        &["trackers"],
        false,
        &after_now(),
    );
    assert_eq!(
        post_github_comment(&h, &comment).await.status(),
        StatusCode::OK
    );

    let mut tx = h.db.begin(org).await.unwrap();
    let repo = tx.get_repo_by_slug("api").await.unwrap().expect("repo");
    let job = tx
        .get_job_by_ticket_for_repo(repo.id, Tracker::Github, "acme/api#18")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(job.is_none());
}

/// A GitHub comment event like [`post_github_comment`]'s, sent with an
/// `X-GitHub-Delivery` id.
async fn post_github_comment_as(h: &Harness, body: &[u8], delivery: &str) -> http::Response<Body> {
    post_webhook(
        h,
        "/webhooks/github",
        body,
        &[
            ("x-github-event", "issue_comment"),
            ("x-github-delivery", delivery),
            (
                "x-hub-signature-256",
                &format!("sha256={}", github_signature(body)),
            ),
        ],
    )
    .await
}

/// Forget every recorded webhook delivery, as the sweep does after the retention
/// window, so a redelivery reaches the job's own guards.
async fn forget_deliveries(h: &Harness) {
    sqlx::query("DELETE FROM tracker_deliveries")
        .execute(h.db.pool())
        .await
        .unwrap();
}

/// How many deliveries `org` has recorded.
async fn deliveries_recorded(h: &Harness, org: OrgId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM tracker_deliveries WHERE org_id = $1")
        .bind(org)
        .fetch_one(h.db.pool())
        .await
        .unwrap()
}

/// Clear a job's `ticket_changed_at`, the state a relink can leave it in, so
/// that `inbound_decision`'s own guard would let a repeated comment through and
/// only the delivery dedupe can be what drops it.
async fn forget_ticket_change(h: &Harness, org: OrgId, job: &of_core::jobs::Job) {
    let mut tx = h.db.begin(org).await.unwrap();
    sqlx::query("UPDATE jobs SET ticket_changed_at = NULL WHERE org_id = $1 AND id = $2")
        .bind(org)
        .bind(&job.id)
        .execute(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// GitHub's own redelivery reuses both the id and the body: it is acknowledged
/// and does nothing. The id is unsigned, so it is never a key on its own: a
/// different body carrying a seen id is a new delivery and applies.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_repeated_delivery_is_acknowledged_without_effect_but_a_reused_id_is_not_a_key(
    pool: PgPool,
) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    let first = github_comment(
        INSTALLATION,
        "acme/api",
        17,
        &["trackers"],
        false,
        &after_now(),
    );
    let reply = post_github_comment_as(&h, &first, "delivery-x").await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert!(job_for(&h, org, "acme/api", 17).await.ticket_stale);

    forget_ticket_change(&h, org, &job).await;
    let reply = post_github_comment_as(&h, &first, "delivery-x").await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(body_json(reply).await["ok"], serde_json::Value::Bool(true));
    assert_eq!(
        job_for(&h, org, "acme/api", 17).await.ticket_changed_at,
        None,
        "GitHub's redelivery applied again"
    );

    let later = hours_from_now(2);
    let other = github_comment(INSTALLATION, "acme/api", 17, &["trackers"], false, &later);
    let reply = post_github_comment_as(&h, &other, "delivery-x").await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(
        job_for(&h, org, "acme/api", 17)
            .await
            .ticket_changed_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        Some(later),
        "an unsigned delivery id suppressed a different signed body"
    );
}

/// A replay: the captured body and signature sent again under a fresh delivery
/// id (the header is not signed). The body digest makes it a repeat.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_replayed_body_under_a_new_delivery_id_is_acknowledged_without_effect(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    let comment = github_comment(
        INSTALLATION,
        "acme/api",
        17,
        &["trackers"],
        false,
        &after_now(),
    );
    let reply = post_github_comment_as(&h, &comment, "delivery-1").await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert!(job_for(&h, org, "acme/api", 17).await.ticket_stale);

    forget_ticket_change(&h, org, &job).await;
    for replay in [Some("delivery-2"), None] {
        let reply = match replay {
            Some(id) => post_github_comment_as(&h, &comment, id).await,
            None => post_github_comment(&h, &comment).await,
        };
        assert_eq!(reply.status(), StatusCode::OK);
        let after = job_for(&h, org, "acme/api", 17).await;
        assert_eq!(after.ticket_changed_at, None, "a replayed body applied");
    }

    // Control: a different body under a fresh id applies.
    let later = hours_from_now(2);
    let other = github_comment(INSTALLATION, "acme/api", 17, &["trackers"], false, &later);
    let reply = post_github_comment_as(&h, &other, "delivery-3").await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(
        job_for(&h, org, "acme/api", 17)
            .await
            .ticket_changed_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        Some(later)
    );
}

/// `X-GitHub-Event` is not signed. A comment's signed body sent first under
/// `issues` is rejected (its shape does not match the event) and recorded
/// nowhere, so it cannot use up the genuine `issue_comment` delivery, which
/// still marks the job.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_body_sent_under_the_wrong_event_is_rejected_and_does_not_pre_empt_the_genuine_delivery(
    pool: PgPool,
) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    let recorded = deliveries_recorded(&h, org).await;

    let comment = github_comment(
        INSTALLATION,
        "acme/api",
        17,
        &["trackers"],
        false,
        &after_now(),
    );
    let mislabelled = post_github_issue(&h, &comment).await;
    assert_eq!(mislabelled.status(), StatusCode::NOT_FOUND);
    assert!(!job_for(&h, org, "acme/api", 17).await.ticket_stale);
    assert_eq!(deliveries_recorded(&h, org).await, recorded);

    let genuine = post_github_comment_as(&h, &comment, "delivery-1").await;
    assert_eq!(genuine.status(), StatusCode::OK);
    assert!(
        job_for(&h, org, "acme/api", 17).await.ticket_stale,
        "the genuine comment delivery was dropped as a repeat"
    );
}

/// A close of a claimed job is applied in a second pass, after its first
/// pass's transaction (and the delivery record in it) was dropped for the
/// standing lookup. The second pass records the delivery again, so the close
/// is remembered like any other event.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_billed_close_records_its_delivery_in_the_second_pass(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    let job = open_issue(&h, org, INSTALLATION, "acme/api", 17).await;
    claim(&h, org, &job, UserId::new()).await;
    assert_eq!(deliveries_recorded(&h, org).await, 1, "the open");

    close_issue(&h, INSTALLATION, "acme/api", 17, "completed").await;
    assert_eq!(completion_rows(&pool).await, vec![billed_close()]);
    assert_eq!(
        deliveries_recorded(&h, org).await,
        2,
        "the billed close was not recorded"
    );
}

/// GitHub never sends a malformed delivery id; one is rejected with the same
/// uniform `404` as any other rejected webhook, and has no effect.
#[sqlx::test(migrations = "../of-core/migrations")]
async fn a_malformed_delivery_id_is_rejected(pool: PgPool) {
    let h = harness(pool).await;
    let org = OrgId::new();
    bind_github_repo(&h, org, INSTALLATION, "acme/api").await;
    open_issue(&h, org, INSTALLATION, "acme/api", 17).await;

    let comment = github_comment(
        INSTALLATION,
        "acme/api",
        17,
        &["trackers"],
        false,
        &after_now(),
    );
    let too_long = "x".repeat(129);
    let reply = post_github_comment_as(&h, &comment, &too_long).await;
    assert_eq!(reply.status(), StatusCode::NOT_FOUND);
    assert!(!job_for(&h, org, "acme/api", 17).await.ticket_stale);
}
