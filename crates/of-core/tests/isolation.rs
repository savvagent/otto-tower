//! Tenant isolation.
//!
//! This file is the evidence for the product's central claim: one org cannot
//! see or touch another's data. It exercises the guarantee through the real
//! `of-core` API, against a real Postgres, with RLS enabled — the same path
//! production takes.
//!
//! Every one of these tests failed to isolate at least once during development,
//! when the pinned transaction did not yet `SET LOCAL ROLE`: Postgres exempts
//! superusers and table owners from their own row-level security policies, and
//! the connecting user is both in local development and in these very tests. A
//! policy that is not exercised by a test running as the deploying role is not
//! a policy, it is a comment.

use of_core::jobs::JobsExt;
use of_core::leases::LeasesExt;
use of_core::messages::MessagesExt;
use of_core::repos::ReposExt;
mod common;

use common::{db, job, tenant};
use of_core::ids::JobId;
use of_core::jobs::JobFilter;
use of_core::messages::{ChannelRef, InboxQuery, NewMessage};
use of_core::repos::{RepoPatch, RepoRef};
use of_core::teams::TeamScope;
use of_core::trackers::{resolve_binding, upsert_binding, upsert_connection, Provider};
use otto_tenant::ids::OrgId;
use otto_tenant::ids::TeamId;
use sqlx::PgPool;

#[sqlx::test]
async fn repos_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let seen = tx.list_repos(true, None).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "acme api");

    let mut tx = db.begin(b.org).await.unwrap();
    let seen = tx.list_repos(true, None).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "globex api");
}

/// The same remote registered by two orgs must resolve to each org's own repo.
/// Two customers working in the same open-source repository is normal, and
/// neither may learn the other exists.
#[sqlx::test]
async fn identical_remotes_resolve_per_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:shared/lib.git").await;
    let b = tenant(&db, "globex", "https://github.com/shared/lib").await;

    let r = RepoRef {
        remote: Some("git@github.com:shared/lib.git".into()),
        ..Default::default()
    };

    let mut tx = db.begin(a.org).await.unwrap();
    let got = tx.resolve_repo(&r).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(got.id, a.repo);

    let mut tx = db.begin(b.org).await.unwrap();
    let got = tx.resolve_repo(&r).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(got.id, b.repo);
}

#[sqlx::test]
async fn jobs_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let secret = tx.add_job(job(&a, "acme secret work")).await.unwrap();
    tx.commit().await.unwrap();

    // B lists: sees nothing.
    let mut tx = db.begin(b.org).await.unwrap();
    let seen = tx.list_jobs(&JobFilter::default()).await.unwrap();
    assert!(seen.is_empty(), "org B saw org A's jobs: {seen:?}");

    // B fetches A's job id directly: not found, not "forbidden" — B must not
    // even learn the id exists.
    let direct = tx.get_job(&secret.id).await;
    tx.commit().await.unwrap();
    assert!(direct.is_err(), "org B read org A's job by id");
}

#[sqlx::test]
async fn cross_org_mutation_is_refused(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let target = tx.add_job(job(&a, "acme work")).await.unwrap();
    tx.commit().await.unwrap();

    // A second job, claimed and carrying a live cancellation request, so the
    // cross-org `cancel_job` assertion below actually proves guard 1.
    // `cancel_job` requires in-progress/active status *and* a cancellation
    // request on file; `target` (pending, nothing requested) satisfies
    // neither, so calling `cancel_job` on it fails on status grounds alone —
    // it would fail identically for org A. `claimed` is put in the one state
    // where an in-org call would actually succeed, so its failure here can
    // only be attributed to the org-scoping predicate finding no row.
    let mut tx = db.begin(a.org).await.unwrap();
    let claimed = tx.add_job(job(&a, "acme in-flight work")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&claimed.id), a.user, None, None)
        .await
        .unwrap();
    let claimed = tx
        .request_cancel(&claimed.id, a.user, Some("stop"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(claimed.status, of_core::jobs::Status::InProgress);
    assert!(claimed.cancel_requested_at.is_some());

    // Every mutating verb, from the wrong org.
    let mut tx = db.begin(b.org).await.unwrap();
    assert!(tx
        .update_job(&target.id, Some("pwned"), None, None, None)
        .await
        .is_err());
    assert!(tx.delete_job(&target.id).await.is_err());
    assert!(tx
        .claim_jobs(std::slice::from_ref(&target.id), b.user, None, None)
        .await
        .is_err());
    assert!(tx.repend_job(&target.id).await.is_err());
    assert!(tx.request_cancel(&target.id, b.user, None).await.is_err());
    assert!(tx
        .cancel_job(&claimed.id, b.user, None, None)
        .await
        .is_err());
    // The GH#65 claim-expiry additions: `claimed` is a real, live claim held
    // by A in A's org, so a call that reached the row would succeed. From B's
    // pinned transaction the row must not even be found — guard 1's org_id
    // predicate, not the claimer check inside `ensure_claim_held`, is what
    // has to refuse these.
    assert!(tx
        .renew_claim(&claimed.id, b.user, None, None)
        .await
        .is_err());
    assert!(tx
        .complete_job(&claimed.id, b.user, Some("pwned"), None)
        .await
        .is_err());
    assert!(tx
        .fail_job(&claimed.id, b.user, Some("pwned"), None)
        .await
        .is_err());
    let _ = tx.rollback().await;

    // A's jobs are untouched — including `claimed`'s in-progress status and
    // its cancellation request, which the failed cross-org `cancel_job` call
    // must not have been able to finalize.
    let mut tx = db.begin(a.org).await.unwrap();
    let after = tx.get_job(&target.id).await.unwrap();
    let after_claimed = tx.get_job(&claimed.id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(after.title, "acme work");
    assert_eq!(after.status, of_core::jobs::Status::Pending);
    assert_eq!(after_claimed.status, of_core::jobs::Status::InProgress);
    assert!(after_claimed.cancel_requested_at.is_some());
    assert_eq!(after_claimed.cancel_reason.as_deref(), Some("stop"));
    assert_eq!(
        after_claimed.claimed_by,
        Some(a.user),
        "B's cross-org renew/complete/fail attempts must not have touched the claim"
    );
}

/// The idempotency-key unique index is `(org_id, idempotency_key)`, not a
/// bare `idempotency_key` — the same literal key string reused by two
/// different orgs, for two genuinely different jobs, must not collide.
///
/// This deliberately does **not** compare the two returned job ids for
/// inequality: `JobId` is a per-org sequential counter (`job_ids_are_dense_
/// and_per_org` in `tests/queue.rs`), so both of these calls legitimately
/// produce `job-1` in their own org — an id match here is expected and
/// proves nothing about isolation either way. The actual proof is that
/// **both `add_job` calls succeed** despite differing payloads under the
/// same key. If the index (or `find_replayed_job`'s `SELECT`) were missing
/// its `org_id` predicate, one of two things would happen instead, and this
/// test would catch either: org B's insert could hit a global-uniqueness
/// constraint and be rejected outright, or it could wrongly converge onto
/// org A's row via the SAVEPOINT unique-violation recovery path — and since
/// the payloads differ, that convergence would fail with
/// `idempotency_key_conflict` rather than succeeding. The `find_replayed_job`
/// follow-up calls additionally prove each org resolves its *own* content
/// back, not the other org's, for the identical key.
#[sqlx::test]
async fn idempotency_keys_do_not_cross_org_boundaries(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut a_new = job(&a, "acme work");
    a_new.idempotency_key = Some("shared-key".into());
    let mut tx = db.begin(a.org).await.unwrap();
    let a_job = tx.add_job(a_new.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let mut b_new = job(&b, "globex work");
    b_new.idempotency_key = Some("shared-key".into());
    let mut tx = db.begin(b.org).await.unwrap();
    let b_job = tx.add_job(b_new.clone()).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(a_job.title, "acme work");
    assert_eq!(b_job.title, "globex work");

    // Each org resolves the shared key back to its own content, not the
    // other org's — the find_replayed_job fast path, not the insert path
    // exercised above.
    let mut tx = db.begin(a.org).await.unwrap();
    let replayed = tx.find_replayed_job(&a_new).await.unwrap().unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(replayed.id, a_job.id);
    assert_eq!(replayed.title, "acme work");

    let mut tx = db.begin(b.org).await.unwrap();
    let replayed = tx.find_replayed_job(&b_new).await.unwrap().unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(replayed.id, b_job.id);
    assert_eq!(replayed.title, "globex work");
}

/// The identical proof for `send_message`'s idempotency key. `Message::id`
/// is a plain `i64` (not per-org sequential the way `JobId` is), so unlike
/// the job version above an id comparison is meaningful here too — but the
/// `find_replayed_message` round trip is kept for the same reason: it is the
/// part that would actually catch a missing `org_id` predicate in the
/// SELECT, not the insert succeeding twice with different payloads.
#[sqlx::test]
async fn message_idempotency_keys_do_not_cross_org_boundaries(pool: PgPool) {
    use of_core::messages::NewMessage;

    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let a_new = NewMessage {
        body: "acme note".into(),
        idempotency_key: Some("shared-key".into()),
        repo_id: Some(a.repo),
        ..Default::default()
    };
    let mut tx = db.begin(a.org).await.unwrap();
    let a_msg = tx.send_message(a.user, a_new.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let b_new = NewMessage {
        body: "globex note".into(),
        idempotency_key: Some("shared-key".into()),
        repo_id: Some(b.repo),
        ..Default::default()
    };
    let mut tx = db.begin(b.org).await.unwrap();
    let b_msg = tx.send_message(b.user, b_new.clone()).await.unwrap();
    tx.commit().await.unwrap();

    assert_ne!(a_msg.id, b_msg.id);
    assert_eq!(a_msg.body, "acme note");
    assert_eq!(b_msg.body, "globex note");

    let mut tx = db.begin(a.org).await.unwrap();
    let replayed = tx
        .find_replayed_message(a.user, &a_new)
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(replayed.id, a_msg.id);
    assert_eq!(replayed.body, "acme note");

    let mut tx = db.begin(b.org).await.unwrap();
    let replayed = tx
        .find_replayed_message(b.user, &b_new)
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(replayed.id, b_msg.id);
    assert_eq!(replayed.body, "globex note");
}

/// The tracker-sync accessors added for the two-way sync engine (Task 4) are
/// fresh SQL entry points on `jobs`, so guard 1 (the explicit `org_id`
/// predicate) needs its own proof here — the happy-path tests next to these
/// functions only exercise repo-scoping and status semantics within a single
/// org.
#[sqlx::test]
async fn cross_org_ticket_sync_accessors_are_refused(pool: PgPool) {
    use of_core::jobs::Tracker;

    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let acme_job = tx
        .create_from_ticket(
            a.repo,
            Tracker::Github,
            "acme/api#42",
            "acme ticket work",
            Some("body"),
            Some("2026-09-03T12:00:00Z"),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // B, operating under its own pinned org, cannot reach A's ticket-linked
    // job by any of the new accessors.
    let mut tx = db.begin(b.org).await.unwrap();
    let by_ticket = tx
        .get_job_by_ticket_for_repo(a.repo, Tracker::Github, "acme/api#42")
        .await
        .unwrap();
    assert!(
        by_ticket.is_none(),
        "org B resolved org A's ticket-linked job: {by_ticket:?}"
    );
    // B cannot even create against A's repo id — the repo does not resolve
    // for B, so this fails closed rather than silently creating on the wrong
    // side.
    assert!(tx
        .create_from_ticket(a.repo, Tracker::Github, "acme/api#42", "pwned", None, None,)
        .await
        .is_err());
    assert!(tx
        .update_from_ticket(&acme_job.id, "pwned", None, Some("2099-01-01T00:00:00Z"))
        .await
        .is_err());
    assert!(tx
        .close_from_ticket(
            &acme_job.id,
            of_core::jobs::Status::Completed,
            None,
            None,
            None,
        )
        .await
        .is_err());
    assert!(tx
        .set_remote_revision(&acme_job.id, "2099-01-01T00:00:00Z")
        .await
        .is_err());
    assert!(tx
        .link_ticket(&acme_job.id, Tracker::Github, "globex/api#99")
        .await
        .is_err());
    let _ = tx.rollback().await;

    // A's job is exactly as it was.
    let mut tx = db.begin(a.org).await.unwrap();
    let after = tx.get_job(&acme_job.id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(after.title, "acme ticket work");
    assert_eq!(after.status, of_core::jobs::Status::Pending);
    assert_eq!(
        after.remote_revision.as_deref(),
        Some("2026-09-03T12:00:00Z")
    );
    assert_eq!(after.ticket_ref.as_deref(), Some("acme/api#42"));
}

/// `update_repo` from the wrong org must not reach the row, and must not be
/// able to steal a remote either — re-pointing a remote is how you would divert
/// another tenant's agents to your own queue without ever reading their data.
#[sqlx::test]
async fn cross_org_repo_updates_are_refused(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(b.org).await.unwrap();
    assert!(tx
        .update_repo(
            a.repo,
            RepoPatch {
                name: Some("pwned".into()),
                active: Some(false),
                add_remotes: vec!["git@github.com:globex/stolen.git".into()],
                ..Default::default()
            },
        )
        .await
        .is_err());
    let _ = tx.rollback().await;

    let mut tx = db.begin(a.org).await.unwrap();
    let after = tx.get_repo(a.repo).await.unwrap().unwrap();
    tx.commit().await.unwrap();
    assert_eq!(after.name, "acme api");
    assert!(after.active);
}

/// A job in org A cannot be made to depend on a job in org B, which would
/// otherwise leak B's completion state into A's `ready`/`blocked` answers.
#[sqlx::test]
async fn dependencies_cannot_cross_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(b.org).await.unwrap();
    let theirs = tx.add_job(job(&b, "globex work")).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    let mine = tx.add_job(job(&a, "acme work")).await.unwrap();
    let res = tx
        .set_dependencies(&mine.id, std::slice::from_ref(&theirs.id), &[])
        .await;
    let _ = tx.rollback().await;

    assert!(res.is_err(), "org A depended on org B's job");
}

/// `team` as a team of `t`'s org, verified against a mock platform that
/// knows it.
async fn verified_team(t: &common::Tenant, team: TeamId) -> of_core::teams::VerifiedTeam {
    let platform = of_testkit::MockPlatform::start().await;
    platform.add_org(t.org.as_uuid(), "org", "Org");
    platform.add_team(t.org.as_uuid(), team.as_uuid(), "team", "Team");
    of_core::teams::VerifiedTeam::verify(&platform.client(), t.org, team)
        .await
        .unwrap()
}

#[sqlx::test]
async fn messages_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let a_team = TeamId::from(uuid::Uuid::new_v4());
    let b_team = TeamId::from(uuid::Uuid::new_v4());

    let mut tx = db.begin(a.org).await.unwrap();
    let mut a_ids = Vec::new();
    for (repo_id, team_id) in [
        (Some(a.repo), None),
        (None, Some(verified_team(&a, a_team).await)),
    ] {
        a_ids.push(
            tx.send_message(
                a.user,
                NewMessage {
                    body: "internal acme plan".into(),
                    repo_id,
                    team_id,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .id,
        );
    }
    tx.commit().await.unwrap();

    // Org B reads its own repo's and team's channels, and then org A's repo's
    // and team's channels by id, as if it had learned them: only the org
    // predicate stands in the way.
    let mut tx = db.begin(b.org).await.unwrap();
    for channel in [
        ChannelRef::Repo(b.repo),
        ChannelRef::Team(b_team),
        ChannelRef::Repo(a.repo),
        ChannelRef::Team(a_team),
    ] {
        let q = InboxQuery {
            channel,
            unread_only: false,
            limit: 50,
            newest_first: false,
            visible_teams: None,
        };
        let seen = tx.inbox(b.user, &q).await.unwrap();
        let n = tx
            .unread_count_for(b.user, channel, &TeamScope::All)
            .await
            .unwrap();
        assert!(seen.is_empty(), "org B read org A's messages: {seen:?}");
        assert_eq!(n, 0);
        for up_to in &a_ids {
            assert_eq!(
                tx.ack_messages_for(b.user, channel, *up_to, &TeamScope::All)
                    .await
                    .unwrap_err()
                    .code(),
                "invalid_argument",
                "{channel:?}: org B acked org A's message {up_to}"
            );
        }
    }
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn leases_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    tx.acquire_lease(a.repo, "main", a.user, Some("agent-a"), None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    let seen = tx.list_leases(None).await.unwrap();
    tx.commit().await.unwrap();
    assert!(seen.is_empty(), "org B saw org A's leases: {seen:?}");
}

/// A transaction pinned to an org that owns nothing sees nothing — rather than,
/// say, everything. The fail-closed direction is the one that matters: a bug in
/// org resolution should produce an empty result a test will catch, never a
/// cross-tenant dump.
#[sqlx::test]
async fn unknown_org_sees_nothing(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    tx.add_job(job(&a, "acme work")).await.unwrap();
    tx.commit().await.unwrap();

    let nobody = OrgId::new();
    let mut tx = db.begin(nobody).await.unwrap();
    assert!(tx.list_repos(true, None).await.unwrap().is_empty());
    assert!(tx
        .list_jobs(&JobFilter::default())
        .await
        .unwrap()
        .is_empty());
    assert!(tx.list_leases(None).await.unwrap().is_empty());
    assert!(tx.get_job(&JobId::from("job-1")).await.is_err());
    tx.commit().await.unwrap();
}

/// **The test that actually exercises RLS.**
///
/// Every other test in this file passes on the strength of guard one — the
/// explicit `org_id = $1` predicate that every `of-core` query carries. Verified
/// the obvious way: with `SET LOCAL ROLE` deleted from `Db::begin`, all eight of
/// them still passed. They prove the predicates work, which is worth proving,
/// but they say nothing about the second guard.
///
/// Guard two exists for the query that *forgets* the predicate — the one a
/// future contributor writes at 5pm. So this test issues exactly that query:
/// raw SQL with no `org_id` filter at all, inside a pinned transaction. If RLS
/// is doing its job the result is scoped anyway. If it is not, this returns both
/// orgs' rows and fails, which is the regression the whole mechanism is for.
#[sqlx::test]
async fn rls_scopes_a_query_that_forgets_the_org_predicate(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    for (t, title) in [(&a, "acme work"), (&b, "globex work")] {
        let mut tx = db.begin(t.org).await.unwrap();
        tx.add_job(job(t, title)).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Deliberately unscoped: no WHERE org_id, no bind parameter, nothing.
    let mut tx = db.begin(a.org).await.unwrap();
    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM jobs ORDER BY title")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        titles,
        vec!["acme work".to_string()],
        "an unscoped query leaked across tenants — row-level security is not in effect"
    );

    // And the same from the other side, so a policy that happens to pin one
    // hard-coded org would still fail.
    let mut tx = db.begin(b.org).await.unwrap();
    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM jobs ORDER BY title")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(titles, vec!["globex work".to_string()]);
}

/// The same, for writes: an unscoped UPDATE inside a pinned transaction must not
/// reach another tenant's rows.
#[sqlx::test]
async fn rls_scopes_an_unscoped_update(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    for (t, title) in [(&a, "acme work"), (&b, "globex work")] {
        let mut tx = db.begin(t.org).await.unwrap();
        tx.add_job(job(t, title)).await.unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = db.begin(a.org).await.unwrap();
    let affected = sqlx::query("UPDATE jobs SET title = 'rewritten'")
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    tx.commit().await.unwrap();
    assert_eq!(
        affected, 1,
        "an unscoped UPDATE reached another tenant's rows"
    );

    let mut tx = db.begin(b.org).await.unwrap();
    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM jobs")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(titles, vec!["globex work".to_string()]);
}

/// `list_leases` joins a lease to its job by id, and job ids are per org: both
/// orgs here have a `job-1`. The real query pairs them on `org_id` too (guard
/// 1); this issues the join *without* it, pinned to org A as `otto_app`, so
/// only RLS on `jobs` (guard 2) keeps org B's `job-1` out of the pairing.
#[sqlx::test]
async fn rls_scopes_the_lease_job_join(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    for (t, ticket) in [(&a, "acme/api#1"), (&b, "globex/api#2")] {
        let mut tx = db.begin(t.org).await.unwrap();
        let j = tx
            .add_job(of_core::jobs::NewJob {
                ticket_ref: Some(ticket.into()),
                ..job(t, "work")
            })
            .await
            .unwrap();
        assert_eq!(j.id.as_str(), "job-1");
        tx.commit().await.unwrap();
    }
    // Both orgs lease their own `job-1`, so a join that matched on the id
    // alone would have a second lease and a second job to pair with.
    for t in [&a, &b] {
        let mut tx = db.begin(t.org).await.unwrap();
        tx.acquire_lease(
            t.repo,
            "branch:main",
            t.user,
            None,
            Some(&JobId::from("job-1")),
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(a.org.to_string())
        .execute(tx.conn())
        .await
        .unwrap();
    // Deliberately unscoped: no org_id anywhere, not even in the join.
    let paired: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT j.ticket_ref FROM repo_leases l LEFT JOIN jobs j ON j.id = l.job_id \
         ORDER BY j.ticket_ref",
    )
    .fetch_all(tx.conn())
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        paired,
        vec![Some("acme/api#1".to_string())],
        "an unscoped lease-job join paired org A's lease with org B's job"
    );

    let mut tx = db.begin(a.org).await.unwrap();
    let leases = tx.list_leases(None).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].job_ticket_ref.as_deref(), Some("acme/api#1"));

    let mut tx = db.begin(b.org).await.unwrap();
    let leases = tx.list_leases(None).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].job_ticket_ref.as_deref(), Some("globex/api#2"));
}

#[sqlx::test]
async fn rls_scopes_tracker_connections(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "installation-1", None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(b.org.to_string())
        .execute(tx.conn())
        .await
        .unwrap();

    let seen: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tracker_connections WHERE provider = 'github'")
            .fetch_one(tx.conn())
            .await
            .unwrap();
    let updated = sqlx::query("UPDATE tracker_connections SET external_id = 'pwned'")
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    let deleted = sqlx::query("DELETE FROM tracker_connections")
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    tx.commit().await.unwrap();

    assert_eq!(seen, 0, "org B saw org A's tracker connection");
    assert_eq!(updated, 0, "org B updated org A's tracker connection");
    assert_eq!(deleted, 0, "org B deleted org A's tracker connection");

    let mut tx = db.begin(a.org).await.unwrap();
    let after = of_core::trackers::get_connection(&mut tx, Provider::Github)
        .await
        .unwrap()
        .expect("connection survived");
    tx.commit().await.unwrap();
    assert_eq!(after.id, connection.id);
    assert_eq!(after.external_id, "installation-1");
}

#[sqlx::test]
async fn rls_scopes_tracker_bindings(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "installation-1", None, None)
        .await
        .unwrap();
    let binding = upsert_binding(
        &mut tx,
        a.repo,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "otto-factory",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(b.org.to_string())
        .execute(tx.conn())
        .await
        .unwrap();

    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM tracker_bindings")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    let updated = sqlx::query("UPDATE tracker_bindings SET external_ref = 'pwned'")
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    let deleted = sqlx::query("DELETE FROM tracker_bindings")
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    tx.commit().await.unwrap();

    assert_eq!(seen, 0, "org B saw org A's tracker binding");
    assert_eq!(updated, 0, "org B updated org A's tracker binding");
    assert_eq!(deleted, 0, "org B deleted org A's tracker binding");

    let mut tx = db.begin(a.org).await.unwrap();
    let after = resolve_binding(&mut tx, a.repo, Provider::Github)
        .await
        .unwrap()
        .expect("binding survived");
    tx.commit().await.unwrap();
    assert_eq!(after.id, binding.id);
    assert_eq!(after.external_ref, "acme/api");
}

/// **What 0020_rename_trigger_label_default.sql got wrong, reproduced.**
///
/// That migration relabels `tracker_bindings.trigger_label` with a bare
/// `UPDATE ... WHERE trigger_label = 'dark-factory'`. `Db::migrate` runs every
/// migration on the raw pool (`db.rs`) — never `Db::begin`, so never `SET
/// LOCAL ROLE otto_app` and never `set_config('app.org_id', …)`. On this
/// deployment's actual connecting role (a superuser, confirmed against
/// `docs/deploy/fly.md`), that is invisible in the opposite direction from
/// what this test demonstrates: a superuser bypasses RLS outright, so the
/// statement would touch *every* org's matching rows, not none. `SET LOCAL
/// ROLE otto_app` below drops to a role RLS actually binds — `otto_app` owns
/// nothing, so it needs no `FORCE` to lose the exemption a table owner would
/// otherwise get; the FORCE-RLS-fallback deployment shape hits the same zero
/// for a related but distinct reason (that role *is* the owner, which is what
/// `FORCE` binds — see `CLAUDE.md`), and this test, run from a superuser
/// connection, can only exercise the non-owner path. Either way
/// `current_org()` stays NULL for the statement's entire lifetime, so
/// `org_id = current_org()` is never true and the UPDATE silently matches
/// zero rows, for every tenant, forever.
///
/// `savvagent/otto-factory#70`. No corrective data migration accompanies this
/// test — `docs/specs/2026-09-10-migration-org-scoped-writes-design.md`
/// (Premise corrections, dated 2026-09-10) records that this deployment's own
/// `tracker_bindings` was empty at investigation time, and that migrations
/// there currently run as the superuser described above. Neither fact is
/// re-verified by this test; it guards the deployment shape where the second
/// one stops being true.
#[sqlx::test]
async fn rls_scopes_a_migration_style_update_with_no_org_context(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    // Seed a row the way a pre-0020 database would have had one: written
    // directly on the pool (bypassing `Tx`, standing in for data a migration
    // — not application code — would be rewriting).
    sqlx::query(
        "INSERT INTO tracker_bindings (org_id, repo_id, provider, external_ref, trigger_label) \
         VALUES ($1, $2, 'github', 'acme/api', 'dark-factory')",
    )
    .bind(a.org)
    .bind(a.repo)
    .execute(db.pool())
    .await
    .unwrap();

    // `otto_app` owns nothing here, so it needs no `FORCE` to be bound by RLS —
    // a non-owner grantee role is never exempt. Neither superuser nor
    // BYPASSRLS, and no `app.org_id` ever set — `Db::migrate` pins no org
    // automatically; a migration that needs one sets it itself (CLAUDE.md's
    // per-org loop), which this bare `UPDATE` never did.
    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();

    // 0020's own statement, verbatim.
    let updated = sqlx::query(
        "UPDATE tracker_bindings SET trigger_label = 'otto-factory' \
         WHERE trigger_label = 'dark-factory'",
    )
    .execute(tx.conn())
    .await
    .unwrap()
    .rows_affected();
    tx.commit().await.unwrap();

    assert_eq!(
        updated, 0,
        "a bare UPDATE against an RLS-active tenant table with no app.org_id \
         set is exactly the failure this test exists to keep visible — if \
         this starts affecting rows, something about the deployment's \
         isolation shape changed and every migration written under the old \
         assumption needs re-auditing"
    );

    // Positive control: the row is still there, still stale. The zero above
    // is the missing org context, not a missing or already-touched row —
    // the distinction CLAUDE.md's own guard-1/guard-2 split insists a test
    // in this family has to make.
    let mut tx = db.begin(a.org).await.unwrap();
    let label: String = sqlx::query_scalar("SELECT trigger_label FROM tracker_bindings")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    assert_eq!(
        label, "dark-factory",
        "the row 0020 should have relabelled must still be there, untouched, \
         for the zero above to mean what this test claims it means"
    );

    // And the same statement, with org context supplied, does relabel it —
    // proving the statement is capable of matching at all, so the zero above
    // is attributable to the missing `app.org_id`, not to a typo in the seed
    // or the UPDATE's own WHERE clause.
    let updated = sqlx::query(
        "UPDATE tracker_bindings SET trigger_label = 'otto-factory' \
         WHERE trigger_label = 'dark-factory'",
    )
    .execute(tx.conn())
    .await
    .unwrap()
    .rows_affected();
    tx.commit().await.unwrap();
    assert_eq!(
        updated, 1,
        "the same statement, org-scoped, should have matched the seeded row"
    );
}

// `audit_global_on_refuses_a_pinned_connection` used to live here: it proved
// that passing a pinned `Tx`'s connection to `Db::audit_global_on` was
// rejected at runtime by `audit_events_append`'s `WITH CHECK`. Since
// `savvagent/otto-factory#133`, `audit_global_on` takes `&mut
// otto_tenant::db::Unpinned` instead of `E: sqlx::PgExecutor<'e>`, and `Tx::conn()`
// has no way to produce one — the misuse this test caught is now a compile
// error, which every `cargo build`/`cargo test` run already proves on every
// commit. That type-level guarantee is provenance only, though: it says
// nothing about a transaction that started unpinned and was pinned by hand
// afterwards, nor about the database policy `audit_global_on` used to rely on
// entirely. The two tests below replace it, one per remaining gap.

/// The DB-level fact `audit_global_on_refuses_a_pinned_connection` used to
/// prove, kept independent of which Rust API reaches `audit_events`: a
/// pinned transaction's `WITH CHECK (current_org() IS NULL OR org_id =
/// current_org())` must still reject a `NULL`-org row when written by raw
/// SQL, not just when written through `Db::audit_global_on`. If a future
/// migration ever loosens this policy (e.g. to admit `org_id IS NULL`
/// unconditionally), this is the test that notices — independent of any
/// Rust-level type change.
#[sqlx::test]
async fn a_pinned_transaction_cannot_append_a_null_org_audit_row(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let err = sqlx::query(AUDIT_INSERT)
        .bind(Option::<OrgId>::None)
        .execute(tx.conn())
        .await
        .expect_err("a pinned transaction wrote a NULL-org audit row");
    assert!(
        err.to_string().contains("row-level security"),
        "expected an RLS refusal, got: {err}"
    );
}

/// `Db::audit_global_on`'s own runtime guard: an `Unpinned` transaction that
/// has been pinned by hand since `begin_unpinned` opened it — the one thing
/// the type itself cannot see, since `Unpinned::conn()` hands out the same
/// bare `&mut PgConnection` a pinned transaction's `conn()` would. This is
/// the case the type-level fix alone does not close, and the one that
/// matters most on this deployment's actual (RLS-bypassed) shape, where the
/// database itself would not have refused the write.
#[sqlx::test]
async fn audit_global_on_refuses_a_transaction_pinned_after_it_was_opened(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(a.org.to_string())
        .execute(tx.conn())
        .await
        .unwrap();

    let err =
        otto_tenant::Db::audit_global_on(&mut tx, otto_tenant::audit::Entry::new("test.misuse"))
            .await
            .expect_err(
                "audit_global_on must refuse a transaction pinned by hand after it was opened",
            );
    assert_eq!(err.code(), "invalid_argument", "got: {err}");
}

// ---------------------------------------------------------------------------
// The guard on the guard.
//
// Everything above proves isolation holds *in this test database*. These prove
// the server can tell whether it holds in some *other* database — which is a
// different question, and the one a deployment gets wrong. Row-level security
// is the guard the environment can switch off: identical migrations isolate
// under one connecting role and not at all under another.
// ---------------------------------------------------------------------------

/// The happy path, and the reason it is not trivial: `#[sqlx::test]` connects as
/// the role Postgres was initialised with, which is a superuser. Isolation here
/// is real only because `SET LOCAL ROLE otto_app` drops out of it, so a passing
/// assertion is evidence the role was genuinely assumed.
#[sqlx::test]
async fn isolation_verifies_on_a_healthy_database(pool: PgPool) {
    let db = db(pool);
    let report = db
        .verify_tenant_isolation()
        .await
        .expect("a freshly migrated database must enforce isolation");

    assert!(
        report.tenant_role_assumed,
        "the test database can create otto_app, so it must have been assumed — \
         a false here means begin() is no longer dropping out of the superuser"
    );
    assert_eq!(report.effective_role, "otto_app");
    assert!(!report.role_is_superuser && !report.role_bypasses_rls);
    assert!(
        report.tables.len() >= 13,
        "expected every tenant table from 0007_rls.sql (the factory's own), got {:?}",
        report.tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    assert!(
        report.tables.iter().all(|t| t.rls_forced),
        "every tenant table must be FORCE ROW LEVEL SECURITY — that is what \
         carries isolation on a deployment that cannot create otto_app"
    );
}

/// Turning RLS off on one table must fail the check, naming that table. This is
/// the shape of a future migration that adds a tenant table and forgets the
/// `ENABLE ROW LEVEL SECURITY` half.
#[sqlx::test]
async fn isolation_refuses_a_table_with_row_level_security_disabled(pool: PgPool) {
    sqlx::query("ALTER TABLE jobs DISABLE ROW LEVEL SECURITY")
        .execute(&pool)
        .await
        .unwrap();

    let db = db(pool);
    let err = db
        .verify_tenant_isolation()
        .await
        .expect_err("a tenant table with RLS off is not isolated");

    let message = err.to_string();
    assert!(message.contains("jobs"), "{message}");
    assert!(message.contains("not enabled"), "{message}");
    assert_eq!(err.code(), "isolation_not_enforced");
}

/// A database that never ran `0007_rls.sql` must not read as healthy. Dropping
/// every policy is the same end state, and it is what a partially-applied
/// migration leaves behind.
#[sqlx::test]
async fn isolation_refuses_a_database_with_no_policies(pool: PgPool) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' \
           AND EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid)",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for t in &tables {
        sqlx::query(&format!("DROP POLICY {t}_tenant_isolation ON {t}"))
            .execute(&pool)
            .await
            .unwrap();
    }

    let db = db(pool);
    let err = db
        .verify_tenant_isolation()
        .await
        .expect_err("a database with no tenant policies is not isolated");
    assert!(
        err.to_string()
            .contains("no table carries a tenant-isolation policy"),
        "{err}"
    );
}

/// The verification must not leave the session pinned to `otto_app`, or the
/// control plane — which runs unpinned, as the connecting role — would silently
/// lose the privileges it needs on the very next checkout from the pool.
#[sqlx::test]
async fn verifying_isolation_does_not_poison_the_pool(pool: PgPool) {
    let db = db(pool);
    db.verify_tenant_isolation().await.unwrap();

    let mut tx = db.begin_unpinned().await.unwrap();
    let role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_ne!(
        role, "otto_app",
        "verification leaked SET ROLE into the pool"
    );

    // And the ordinary path still isolates afterwards.
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let mut tx = db.begin(a.org).await.unwrap();
    assert_eq!(tx.list_repos(true, None).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
    let mut tx = db.begin(b.org).await.unwrap();
    assert_eq!(tx.list_repos(true, None).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
}

// ---------------------------------------------------------------------------
// The audit trail under row-level security.
//
// These tests all issue `SET LOCAL ROLE otto_app` by hand, which is not how the
// rest of the suite works and is the point: `#[sqlx::test]` connects as the
// superuser Postgres was initialised with, and a superuser bypasses row-level
// security even on a FORCE'd table. A test of an audit *policy* written the
// ordinary way would pass against no policy at all.
//
// Dropping to `otto_app` makes the policies apply, which is the same thing that
// happens on a deployment connecting as a non-superuser schema owner — the
// shape where `audit_events` previously rejected every login's audit row.
// ---------------------------------------------------------------------------

const AUDIT_INSERT: &str = "INSERT INTO audit_events (org_id, actor_label, action, detail) \
                            VALUES ($1, 'someone', 'auth.login.succeeded', '{}')";

/// `Db::audit_global` writes with a NULL org from no transaction at all. Under
/// FORCE RLS that is a row the tenant policy cannot describe, so the append
/// policy has to admit the unpinned case explicitly — or every login, TOTP
/// enrolment and email verification fails to leave a trace.
#[sqlx::test]
async fn the_control_plane_can_append_audit_rows_with_no_org(pool: PgPool) {
    let db = db(pool);
    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();

    sqlx::query(AUDIT_INSERT)
        .bind(Option::<OrgId>::None)
        .execute(tx.conn())
        .await
        .expect("the control plane must be able to record a login");
    tx.commit().await.unwrap();
}

/// The same, for an org-scoped event written before a tenant transaction exists
/// — `Db::audit_for_org`, which signup uses.
#[sqlx::test]
async fn the_control_plane_can_append_an_org_scoped_audit_row(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query(AUDIT_INSERT)
        .bind(a.org)
        .execute(tx.conn())
        .await
        .expect("signup must be able to record an org event");
    tx.commit().await.unwrap();
}

/// A pinned transaction may not forge a row for a different org. This is the
/// half the unpinned carve-out must not have opened up.
#[sqlx::test]
async fn a_pinned_transaction_cannot_forge_another_orgs_audit_row(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let err = sqlx::query(AUDIT_INSERT)
        .bind(b.org)
        .execute(tx.conn())
        .await
        .expect_err("a pinned transaction wrote another org's audit row");
    assert!(
        err.to_string().contains("row-level security"),
        "expected an RLS refusal, got: {err}"
    );
}

/// **Append-only.** Nothing may rewrite an audit row — not the tenant role, not
/// the control plane, not the table's owner. Expressed as the absence of an
/// UPDATE policy, so a command that matches no policy touches no rows.
#[sqlx::test]
async fn audit_rows_cannot_be_rewritten(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query(AUDIT_INSERT)
        .bind(a.org)
        .execute(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // From inside the org that owns the row, which is the strongest position a
    // request can reach.
    let mut tx = db.begin(a.org).await.unwrap();
    let rewritten = sqlx::query("UPDATE audit_events SET action = 'tampered'")
        .execute(tx.conn())
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    tx.commit().await.unwrap();
    assert_eq!(rewritten, 0, "an audit row was rewritten");

    let mut tx = db.begin(a.org).await.unwrap();
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        actions.iter().all(|a| a != "tampered"),
        "audit trail was modified: {actions:?}"
    );
}

/// Erasing is likewise unreachable from a request. Retention is a control-plane
/// job, and the policy says so by allowing DELETE only when no org is pinned.
#[sqlx::test]
async fn audit_rows_cannot_be_erased_from_a_request(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query(AUDIT_INSERT)
        .bind(a.org)
        .execute(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    let erased = sqlx::query("DELETE FROM audit_events")
        .execute(tx.conn())
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    tx.commit().await.unwrap();
    assert_eq!(erased, 0, "a request erased an audit row");

    let mut tx = db.begin(a.org).await.unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(remaining, 1);
}

// ------------------------------------------------------ enterprise OIDC SSO
//
// idp_connections, claimed_domains, and orgs.enforce_sso all carry no
// org_id-based RLS policy at all (CLAUDE.md's own bootstrap-before-org-known
// list; orgs is the tenant itself, not tenant-scoped data). These are guard-1
// tests, not rls_scopes_* ones -- there is no policy to probe here, only the
// explicit org_id/id predicate every statement in idp.rs/domains.rs/orgs.rs
// carries. Matches resolve_connection_org's existing test comment convention
// in tests/trackers.rs.

/// The team-scoped helpers added for team visibility, each handed another org's
/// ids directly. Org B's repo carries a team, and org A's restricted scope names
/// that same team id, so a match on team alone would cross the org line: only
/// the explicit `org_id` predicate (or RLS) stands in the way.
#[sqlx::test]
async fn team_scoped_helpers_do_not_cross_orgs(pool: PgPool) {
    use of_core::repos::Hold;

    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let team = TeamId::from(uuid::Uuid::new_v4());

    // Org B: a team-scoped repo with a job, a dependent, a lease, and a message.
    let mut tx = db.begin(b.org).await.unwrap();
    sqlx::query("UPDATE repos SET team_id = $2 WHERE org_id = $1 AND id = $3")
        .bind(b.org)
        .bind(team)
        .bind(b.repo)
        .execute(tx.conn())
        .await
        .unwrap();
    let b_job = tx.add_job(job(&b, "globex work")).await.unwrap();
    tx.add_job(of_core::jobs::NewJob {
        depends_on: vec![b_job.id.clone()],
        ..job(&b, "globex follow-up")
    })
    .await
    .unwrap();
    let b_lease = tx
        .acquire_lease(b.repo, "main", b.user, None, None, None)
        .await
        .unwrap();
    let b_msg = tx
        .send_message(
            b.user,
            NewMessage {
                body: "globex plan".into(),
                repo_id: Some(b.repo),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let b_team_msg = tx
        .send_message(
            b.user,
            NewMessage {
                body: "globex team plan".into(),
                team_id: Some(verified_team(&b, team).await),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let restricted = TeamScope::Teams([team].into_iter().collect());
    for scope in [TeamScope::All, restricted] {
        let mut tx = db.begin(a.org).await.unwrap();

        assert!(matches!(
            tx.get_job_visible(&b_job.id, &scope).await,
            Err(of_core::Error::JobNotFound(_))
        ));
        assert!(matches!(
            tx.get_job_visible_held(&b_job.id, &scope).await,
            Err(of_core::Error::JobNotFound(_))
        ));
        assert!(tx
            .visible_job_ids(std::slice::from_ref(&b_job.id), &scope)
            .await
            .unwrap()
            .is_empty());
        assert!(!tx.has_hidden_dependents(&b_job.id, &scope).await.unwrap());
        assert!(!tx.hidden_repo_ids(&scope).await.unwrap().contains(&b.repo));

        let repos = tx.list_repos_visible(true, None, &scope).await.unwrap();
        assert_eq!(
            repos.iter().map(|r| r.id).collect::<Vec<_>>(),
            [a.repo],
            "org A listed org B's repo"
        );
        let b_remote = RepoRef {
            remote: Some("git@github.com:globex/api.git".into()),
            ..Default::default()
        };
        assert!(matches!(
            tx.resolve_repo_visible(&b_remote, &scope).await,
            Err(of_core::Error::RepoUnresolved { .. })
        ));
        assert!(matches!(
            tx.resolve_repo_visible_held(&b_remote, &scope, Hold::Shared)
                .await,
            Err(of_core::Error::RepoUnresolved { .. })
        ));
        if !scope.is_all() {
            assert!(!tx
                .hold_repo_visible(b.repo, &scope, Hold::Shared)
                .await
                .unwrap());
        }

        assert_eq!(tx.lease_repo_id(b_lease.id).await.unwrap(), None);

        assert!(!tx.message_readable(b_msg.id, &scope).await.unwrap());
        let parent = tx.reply_parent(b_msg.id, &scope).await;
        assert!(matches!(parent, Err(of_core::Error::Invalid(_))));
        // Org A's own channel, and org B's repo's and team's channels handed
        // over by id.
        for channel in [
            ChannelRef::Repo(a.repo),
            ChannelRef::Repo(b.repo),
            ChannelRef::Team(team),
        ] {
            assert_eq!(
                tx.unread_count_for(a.user, channel, &scope).await.unwrap(),
                0
            );
            for up_to in [b_msg.id, b_team_msg.id] {
                assert_eq!(
                    tx.ack_messages_for(a.user, channel, up_to, &scope)
                        .await
                        .unwrap_err()
                        .code(),
                    "invalid_argument",
                    "{channel:?}: org A acked org B's message {up_to}"
                );
            }
            assert_eq!(
                tx.ack_messages_for(a.user, channel, 0, &scope)
                    .await
                    .unwrap(),
                0
            );
        }
        assert_eq!(
            tx.stats_for_teams(None, &[team]).await.unwrap().total,
            0,
            "org A counted org B's jobs"
        );

        tx.commit().await.unwrap();
    }
}
