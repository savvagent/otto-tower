use tower_core::jobs::JobsExt;
use tower_core::repos::ReposExt;
mod common;

use common::{db, job, tenant};
use sqlx::PgPool;
use tower_core::error::Error;
use tower_core::jobs::Status;
use tower_core::jobs::Tracker;
use tower_core::repos::NewRepo;

#[sqlx::test]
async fn get_job_by_ticket_for_repo_does_not_cross_repos(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let web = tx
        .register_repo(NewRepo {
            slug: "web".into(),
            remotes: vec!["git@github.com:acme/web.git".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    tx.create_from_ticket(
        t.repo,
        Tracker::Github,
        "acme/api#17",
        "api work",
        Some("api body"),
        Some("2026-09-03T12:00:00Z"),
    )
    .await
    .unwrap();
    let web_job = tx
        .create_from_ticket(
            web.id,
            Tracker::Github,
            "acme/api#17",
            "web work",
            Some("web body"),
            Some("2026-09-03T13:00:00Z"),
        )
        .await
        .unwrap();

    let resolved = tx
        .get_job_by_ticket_for_repo(web.id, Tracker::Github, "acme/api#17")
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(resolved.id, web_job.id);
    assert_eq!(resolved.repo_id, web.id);
    assert_eq!(resolved.title, "web work");
}

#[sqlx::test]
async fn create_from_ticket_sets_tracker_fields(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let job = tx
        .create_from_ticket(
            t.repo,
            Tracker::Jira,
            "ENG-7",
            "Sync this ticket",
            Some("body"),
            Some("2026-09-03T12:00:00Z"),
        )
        .await
        .unwrap();
    let fetched = tx.get_job(&job.id).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(fetched.status, Status::Pending);
    assert_eq!(fetched.tracker, Some(Tracker::Jira));
    assert_eq!(fetched.ticket_ref.as_deref(), Some("ENG-7"));
    assert_eq!(
        fetched.remote_revision.as_deref(),
        Some("2026-09-03T12:00:00Z")
    );
    assert_eq!(fetched.metadata, serde_json::json!({}));
}

#[sqlx::test]
async fn link_ticket_sets_tracker_and_ticket_ref(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx.add_job(job(&t, "handmade job")).await.unwrap();
    assert_eq!(created.tracker, None);
    assert_eq!(created.ticket_ref, None);

    let linked = tx
        .link_ticket(&created.id, Tracker::Github, "acme/api#42")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(linked.tracker, Some(Tracker::Github));
    assert_eq!(linked.ticket_ref.as_deref(), Some("acme/api#42"));
}

#[sqlx::test]
async fn link_ticket_clears_stale_remote_revision_on_relink(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#1",
            "first ticket",
            None,
            Some("etag-for-ticket-1"),
        )
        .await
        .unwrap();
    assert_eq!(
        created.remote_revision.as_deref(),
        Some("etag-for-ticket-1")
    );

    let relinked = tx
        .link_ticket(&created.id, Tracker::Github, "acme/api#2")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(relinked.ticket_ref.as_deref(), Some("acme/api#2"));
    assert_eq!(relinked.remote_revision, None);
}

#[sqlx::test]
async fn link_ticket_on_a_ticket_another_live_job_holds_returns_ticket_already_linked(
    pool: PgPool,
) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let holder = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#7",
            "already linked",
            None,
            None,
        )
        .await
        .unwrap();
    let other = tx.add_job(job(&t, "wants the same ticket")).await.unwrap();

    let err = tx
        .link_ticket(&other.id, Tracker::Github, "acme/api#7")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    match err {
        Error::TicketAlreadyLinked { ticket_ref, job } => {
            assert_eq!(ticket_ref, "acme/api#7");
            assert_eq!(job, holder.id);
        }
        other => panic!("expected TicketAlreadyLinked, got {other:?}"),
    }
}

/// `repend_job` can revive an older terminal job without touching its
/// `created_at`, so "newest row for this ticket_ref" is not always "the live
/// row a second writer just lost a unique-violation against." Set up exactly
/// that shape — an older job revived live, and a newer-but-terminal job with
/// the same ticket_ref sitting alongside it — and confirm the conflict names
/// the actual live holder, not the newer historical one.
#[sqlx::test]
async fn link_ticket_conflict_names_the_live_holder_not_a_newer_terminal_job(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let older = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#9",
            "older job",
            None,
            None,
        )
        .await
        .unwrap();
    let claimed = tx
        .claim_jobs(std::slice::from_ref(&older.id), t.user, None, None)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.id == older.id)
        .expect("older job claimable");
    tx.complete_job(&claimed.id, t.user, Some("done"), None)
        .await
        .unwrap();

    // Created after `older` finished, so it has a later created_at and is
    // free to reuse the same ticket_ref — the unique index only blocks live
    // rows.
    let newer_terminal = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#9",
            "newer but will terminate",
            None,
            None,
        )
        .await
        .unwrap();
    let claimed_newer = tx
        .claim_jobs(std::slice::from_ref(&newer_terminal.id), t.user, None, None)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.id == newer_terminal.id)
        .expect("newer job claimable");
    tx.fail_job(&claimed_newer.id, t.user, Some("nope"), None)
        .await
        .unwrap();

    // Revives `older` in place — created_at is unchanged, so it stays the
    // earlier of the two rows sharing this ticket_ref, but it is once again
    // the only *live* one.
    tx.repend_job(&older.id).await.unwrap();

    let contender = tx.add_job(job(&t, "wants the same ticket")).await.unwrap();
    let err = tx
        .link_ticket(&contender.id, Tracker::Github, "acme/api#9")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    match err {
        Error::TicketAlreadyLinked { ticket_ref, job } => {
            assert_eq!(ticket_ref, "acme/api#9");
            assert_eq!(
                job, older.id,
                "must name the live holder, not the newer terminal job"
            );
        }
        other => panic!("expected TicketAlreadyLinked, got {other:?}"),
    }
}

#[sqlx::test]
async fn link_ticket_rejects_a_blank_ticket_ref(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx.add_job(job(&t, "handmade job")).await.unwrap();

    let err = tx
        .link_ticket(&created.id, Tracker::Github, "   ")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    assert!(matches!(err, Error::Invalid(_)), "got {err:?}");
}

/// Two concurrent webhook deliveries for the same freshly-labelled issue can
/// each run inbound_decision, each see "no existing job" for the ticket_ref,
/// and each call create_from_ticket — the exact race
/// 0015_jobs_ticket_ref_uniqueness.sql's partial unique index exists to
/// close. This drives two real, separately-connected transactions through
/// that race (one blocks on the other's uncommitted insert, then loses the
/// unique check once it commits) and asserts the loser converges on the
/// winner's job instead of erroring or creating a second job.
#[sqlx::test]
async fn concurrent_create_from_ticket_converges_on_one_job(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let db_a = db.clone();
    let db_b = db.clone();
    let repo = t.repo;
    let org = t.org;

    let (first, second) = tokio::join!(
        async move {
            let mut tx = db_a.begin(org).await.unwrap();
            let job = tx
                .create_from_ticket(
                    repo,
                    Tracker::Github,
                    "acme/api#99",
                    "first delivery",
                    None,
                    None,
                )
                .await
                .unwrap();
            // Hold the row uncommitted briefly so the second delivery below
            // genuinely races against an in-flight insert, not an
            // already-committed one.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tx.commit().await.unwrap();
            job
        },
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let mut tx = db_b.begin(org).await.unwrap();
            let job = tx
                .create_from_ticket(
                    repo,
                    Tracker::Github,
                    "acme/api#99",
                    "second delivery",
                    None,
                    None,
                )
                .await
                .unwrap();
            tx.commit().await.unwrap();
            job
        }
    );

    assert_eq!(
        first.id, second.id,
        "concurrent deliveries for the same ticket produced two different jobs"
    );

    let mut tx = db.begin(t.org).await.unwrap();
    let all = tx
        .list_jobs(&tower_core::jobs::JobFilter {
            repo_id: Some(t.repo),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let matching: Vec<_> = all
        .into_iter()
        .filter(|j| j.ticket_ref.as_deref() == Some("acme/api#99"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one job for the racing ticket, found {matching:?}"
    );
}

/// The identical race shape for a brand-new idempotency key, driven across
/// two real connections rather than sequential calls on one `Tx` — the
/// SAVEPOINT/unique-violation-recovery path this mirrors
/// (`Tx::add_job`) is reasoned about, in its own comments, as behaving
/// exactly like `create_from_ticket`'s; this is the same proof the
/// precedent test above gives that reasoning; see
/// `concurrent_create_from_ticket_converges_on_one_job`.
#[sqlx::test]
async fn concurrent_add_job_idempotency_converges_on_one_job(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let db_a = db.clone();
    let db_b = db.clone();
    let repo = t.repo;
    let org = t.org;
    let user = t.user;

    let new = |repo, user| tower_core::jobs::NewJob {
        repo_id: repo,
        title: "wire up the health endpoint".into(),
        created_by: Some(user),
        idempotency_key: Some("concurrent-retry".into()),
        ..Default::default()
    };

    let (first, second) = tokio::join!(
        async move {
            let mut tx = db_a.begin(org).await.unwrap();
            let job = tx.add_job(new(repo, user)).await.unwrap();
            // Hold the row uncommitted briefly so the second call below
            // genuinely races against an in-flight insert, not an
            // already-committed one.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tx.commit().await.unwrap();
            job
        },
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let mut tx = db_b.begin(org).await.unwrap();
            let job = tx.add_job(new(repo, user)).await.unwrap();
            tx.commit().await.unwrap();
            job
        }
    );

    assert_eq!(
        first.id, second.id,
        "two concurrent calls racing the same brand-new idempotency key produced two jobs"
    );

    let mut tx = db.begin(t.org).await.unwrap();
    let all = tx
        .list_jobs(&tower_core::jobs::JobFilter {
            repo_id: Some(t.repo),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(all.len(), 1, "the race must not leave a duplicate row");
}

/// Proves the database-level premise every `RaceLost` `ok_or_else` site
/// depends on: once a concurrently-inserted winner row has been deleted and
/// committed by a second connection, a `SAVEPOINT` / unique-violation /
/// `ROLLBACK TO SAVEPOINT` / recovery-`SELECT` sequence shaped exactly like
/// `add_job`'s own really does find no row afterward — Postgres does not
/// resurrect it for a re-query in the same transaction, and MVCC visibility
/// is not somehow racing the rollback.
///
/// **This does not call `add_job` itself**, and that is a deliberate,
/// documented gap, not an oversight: the real lost-race branch needs the
/// winner row deleted-and-committed strictly between `add_job`'s own
/// SAVEPOINT-violation and its immediately-following recovery `SELECT` —
/// two `await`s inside one async function call with no externally
/// triggerable yield point in between. Nothing outside `add_job` can
/// reliably signal "delete now" at that exact point without adding a
/// test-only instrumentation hook to production code, which the design
/// spec's Risks section rules out as disproportionate for a bug-fix-sized
/// change. So instead of driving `add_job` as a black box, this test
/// reproduces its identical SQL sequence by hand, via `Tx::conn()`, on a
/// `Tx` the test itself fully controls — which lets it inject the delete at
/// the exact point that matters, deterministically, with no timing
/// dependency at all.
///
/// What this proves: the one fact all four `RaceLost` sites' correctness
/// rests on, which no existing test exercises. What it does *not* prove:
/// that `add_job` (or `link_ticket`/`create_from_ticket`/`send_message`)
/// itself actually reaches that `ok_or_else` arm under real concurrency —
/// only that, if one of them does, the recovery `SELECT` behaves the way
/// the `RaceLost` construction assumes it does.
#[sqlx::test]
async fn a_deleted_and_committed_winner_row_is_invisible_to_the_recovery_select(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let key = "race-lost-db-proof";

    // The "winner": a real, committed job holding the idempotency key the
    // manual insert below will collide on — created through `add_job`
    // itself, so its row is shaped exactly the way the real race's winner
    // would be.
    let mut tx = db.begin(t.org).await.unwrap();
    let winner = tx
        .add_job(tower_core::jobs::NewJob {
            idempotency_key: Some(key.into()),
            ..job(&t, "winner")
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Reproduce add_job's own SAVEPOINT / insert / violation / rollback /
    // recovery shape by hand, on a fresh Tx this test fully controls.
    let mut tx2 = db.begin(t.org).await.unwrap();
    sqlx::query("SAVEPOINT race_probe")
        .execute(tx2.conn())
        .await
        .unwrap();

    let colliding_insert = sqlx::query(
        "INSERT INTO jobs (id, org_id, repo_id, title, idempotency_key, \
         idempotency_payload_hash) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(tower_core::ids::JobId::from_seq(999_999))
    .bind(t.org)
    .bind(t.repo)
    .bind("loser")
    .bind(key)
    .bind(vec![0u8; 32])
    .execute(tx2.conn())
    .await;

    match colliding_insert {
        Err(sqlx::Error::Database(ref db_err)) if db_err.is_unique_violation() => {}
        other => {
            panic!("expected the colliding insert to hit a unique violation, got {other:?}")
        }
    }

    sqlx::query("ROLLBACK TO SAVEPOINT race_probe")
        .execute(tx2.conn())
        .await
        .unwrap();
    sqlx::query("RELEASE SAVEPOINT race_probe")
        .execute(tx2.conn())
        .await
        .unwrap();

    // Delete-and-commit the winner from an independent third connection —
    // exactly the "concurrent delete" add_job's own doc comment names as
    // the scenario the RaceLost sites exist for. A third Tx, not tx2 or the
    // first tx, because the delete must actually commit for tx2's later
    // read to see it gone; deleting inside tx2 itself would prove nothing
    // about a *concurrent* delete.
    let mut tx3 = db.begin(t.org).await.unwrap();
    sqlx::query("DELETE FROM jobs WHERE org_id = $1 AND id = $2")
        .bind(t.org)
        .bind(&winner.id)
        .execute(tx3.conn())
        .await
        .unwrap();
    tx3.commit().await.unwrap();

    // The exact recovery SELECT add_job runs after its own rollback. With
    // the winner now deleted and committed, this must find nothing — the
    // precondition every `RaceLost` construction site assumes.
    let recovered: Option<(tower_core::ids::JobId, Vec<u8>)> = sqlx::query_as(
        "SELECT id, idempotency_payload_hash FROM jobs WHERE org_id = $1 AND idempotency_key = $2",
    )
    .bind(t.org)
    .bind(key)
    .fetch_optional(tx2.conn())
    .await
    .unwrap();

    assert!(
        recovered.is_none(),
        "the deleted-and-committed winner row was still visible to the recovery SELECT"
    );

    tx2.commit().await.unwrap();
}

#[sqlx::test]
async fn close_from_ticket_allows_pending_and_in_progress(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let pending = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#17",
            "pending",
            None,
            None,
        )
        .await
        .unwrap();
    let completed = tx
        .close_from_ticket(
            &pending.id,
            Status::Completed,
            Some("remote complete"),
            None,
            None,
        )
        .await
        .unwrap();

    let in_progress = tx.add_job(job(&t, "claimed first")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&in_progress.id), t.user, None, None)
        .await
        .unwrap();
    let failed = tx
        .close_from_ticket(
            &in_progress.id,
            Status::Failed,
            None,
            Some("remote failed"),
            None,
        )
        .await
        .unwrap();
    // `close_from_ticket` is a second path into a terminal status, distinct
    // from `finalize` — the org-wide stats counters have to track it too.
    let stats = tx.stats(None).await.unwrap();

    tx.commit().await.unwrap();

    assert_eq!(completed.status, Status::Completed);
    assert_eq!(completed.result.as_deref(), Some("remote complete"));
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.error.as_deref(), Some("remote failed"));
    assert_eq!(stats.completed, 1);
    assert_eq!(stats.failed, 1);
}

#[sqlx::test]
async fn close_from_ticket_rejects_terminal_jobs(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let job = tx.add_job(job(&t, "done already")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&job.id), t.user, None, None)
        .await
        .unwrap();
    tx.complete_job(&job.id, t.user, Some("done"), None)
        .await
        .unwrap();

    let err = tx
        .close_from_ticket(&job.id, Status::Failed, None, Some("too late"), None)
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();

    assert_eq!(err.code(), "wrong_status");
}

#[sqlx::test]
async fn close_from_ticket_allows_active(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let active = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#18",
            "will be active",
            None,
            None,
        )
        .await
        .unwrap();
    tx.claim_jobs(std::slice::from_ref(&active.id), t.user, None, None)
        .await
        .unwrap();
    tx.activate_job(&active.id).await.unwrap();

    let completed = tx
        .close_from_ticket(
            &active.id,
            Status::Completed,
            Some("remote complete"),
            None,
            None,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(completed.status, Status::Completed);
}

/// An `active` job is still the *live* holder of its `ticket_ref` for
/// `get_live_job_by_ticket_for_repo` and the partial unique index behind it
/// (`jobs_org_repo_tracker_ticket_open_idx`) — mirrors
/// `link_ticket_on_a_ticket_another_live_job_holds_returns_ticket_already_linked`,
/// but the holder has moved past `in-progress` into `active` before the
/// second job tries to link the same ref.
#[sqlx::test]
async fn link_ticket_on_a_ticket_an_active_job_holds_returns_ticket_already_linked(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let holder = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#19",
            "already linked and active",
            None,
            None,
        )
        .await
        .unwrap();
    tx.claim_jobs(std::slice::from_ref(&holder.id), t.user, None, None)
        .await
        .unwrap();
    tx.activate_job(&holder.id).await.unwrap();

    let other = tx.add_job(job(&t, "wants the same ticket")).await.unwrap();
    let err = tx
        .link_ticket(&other.id, Tracker::Github, "acme/api#19")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    match err {
        Error::TicketAlreadyLinked { ticket_ref, job } => {
            assert_eq!(ticket_ref, "acme/api#19");
            assert_eq!(job, holder.id);
        }
        other => panic!("expected TicketAlreadyLinked, got {other:?}"),
    }
}

/// `close_from_ticket`'s `remote_revision` param folds the revision stamp
/// into the same UPDATE as the status transition (mirroring
/// `update_from_ticket`'s COALESCE), so a ticket close can never leave the
/// job's status and revision written by two separate statements that could
/// drift apart.
#[sqlx::test]
async fn close_from_ticket_stamps_remote_revision_in_the_same_statement(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#99",
            "closes with revision",
            None,
            Some("2026-09-03T12:00:00Z"),
        )
        .await
        .unwrap();

    let closed = tx
        .close_from_ticket(
            &created.id,
            Status::Completed,
            Some("done"),
            None,
            Some("2026-09-04T00:00:00Z"),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(closed.status, Status::Completed);
    assert_eq!(
        closed.remote_revision.as_deref(),
        Some("2026-09-04T00:00:00Z")
    );
}

/// A `None` remote_revision on close must not clear the previously-recorded
/// one — the same self-correcting COALESCE semantics as `update_from_ticket`.
#[sqlx::test]
async fn close_from_ticket_with_no_revision_preserves_the_existing_one(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#100",
            "closes without a new revision",
            None,
            Some("2026-09-03T12:00:00Z"),
        )
        .await
        .unwrap();

    let closed = tx
        .close_from_ticket(&created.id, Status::Completed, None, None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        closed.remote_revision.as_deref(),
        Some("2026-09-03T12:00:00Z")
    );
}

// ------------------------------------------------- issue references as job ids

use savvagent_tenant::ids::TeamId;
use tower_core::ids::{JobId, RepoId};
use tower_core::jobs::{JobFilter, NewJob};
use tower_core::leases::LeasesExt;
use tower_core::teams::TeamScope;
use tower_core::ticket_ref::{JobRef, TicketQuery};

/// A job in `repo` linked (by `add_job`, no tracker) to `ticket`, in a
/// transaction of its own so successive jobs get distinct `created_at`s.
async fn ticketed(
    db: &savvagent_tenant::Db,
    t: &common::Tenant,
    repo: RepoId,
    title: &str,
    ticket: &str,
) -> JobId {
    let mut tx = db.begin(t.org).await.unwrap();
    let j = tx
        .add_job(NewJob {
            repo_id: repo,
            ticket_ref: Some(ticket.into()),
            ..job(t, title)
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    j.id
}

/// A second repo in `t`'s org, scoped to `team` when one is given. The team is
/// written directly: verifying it would need the platform, and visibility only
/// reads the column.
async fn second_repo(
    db: &savvagent_tenant::Db,
    t: &common::Tenant,
    slug: &str,
    team: Option<TeamId>,
) -> RepoId {
    let mut tx = db.begin(t.org).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: slug.into(),
            remotes: vec![format!("git@github.com:acme/{slug}.git")],
            ..Default::default()
        })
        .await
        .unwrap();
    if let Some(team) = team {
        sqlx::query("UPDATE repos SET team_id = $2 WHERE org_id = $1 AND id = $3")
            .bind(t.org)
            .bind(team)
            .bind(repo.id)
            .execute(tx.conn())
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    repo.id
}

fn query(raw: &str) -> TicketQuery {
    match JobRef::parse(raw).unwrap() {
        JobRef::Ticket(q) => q,
        JobRef::Id(id) => panic!("{raw:?} parsed as {id}"),
    }
}

#[sqlx::test]
async fn resolve_job_ref_matches_short_full_any_case_and_leading_zeros(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let id = ticketed(&db, &t, t.repo, "the issue", "acme/api#928").await;
    // Shares the digits but not the number: must not make `#928` ambiguous.
    ticketed(&db, &t, t.repo, "a longer number", "acme/api#1928").await;
    // A bare `#N`, as a caller could type it into add_job.
    let bare = ticketed(&db, &t, t.repo, "bare", "#77").await;

    let mut tx = db.begin(t.org).await.unwrap();
    for raw in ["#928", " #928 ", "#0928", "acme/api#928", "ACME/Api#928"] {
        assert_eq!(
            tx.resolve_job_ref(raw, &TeamScope::All).await.unwrap(),
            id,
            "{raw:?}"
        );
    }
    assert_eq!(
        tx.resolve_job_ref("#77", &TeamScope::All).await.unwrap(),
        bare
    );
    let err = tx.resolve_job_ref("#x", &TeamScope::All).await.unwrap_err();
    assert_eq!(err.code(), "invalid_argument");
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn resolve_job_ref_with_no_match_is_job_not_found(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    ticketed(&db, &t, t.repo, "other", "acme/api#1").await;

    let mut tx = db.begin(t.org).await.unwrap();
    for raw in ["#928", "acme/api#928", "acme/web#1"] {
        let err = tx.resolve_job_ref(raw, &TeamScope::All).await.unwrap_err();
        assert!(matches!(err, Error::NoJobForTicket(_)), "{raw:?}: {err:?}");
        assert_eq!(err.code(), "job_not_found");
    }
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn resolve_job_ref_with_two_matches_is_ambiguous_naming_both(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let web = second_repo(&db, &t, "web", None).await;
    let older = ticketed(&db, &t, t.repo, "api", "acme/api#5").await;
    let newer = ticketed(&db, &t, web, "web", "acme/web#5").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx.resolve_job_ref("#5", &TeamScope::All).await.unwrap_err();
    assert_eq!(err.code(), "ambiguous_ticket_ref");
    assert!(!err.retriable());
    match &err {
        Error::AmbiguousTicket { jobs, more, .. } => {
            assert_eq!(jobs, &vec![newer.clone(), older.clone()]);
            assert!(!more);
        }
        other => panic!("{other:?}"),
    }
    let msg = err.to_string();
    assert!(
        msg.contains(newer.as_str()) && msg.contains(older.as_str()),
        "{msg}"
    );

    // The full form picks one.
    assert_eq!(
        tx.resolve_job_ref("acme/api#5", &TeamScope::All)
            .await
            .unwrap(),
        older
    );
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn resolve_job_ref_lists_ten_and_says_more(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let mut tx = db.begin(t.org).await.unwrap();
    for i in 0..12 {
        tx.add_job(NewJob {
            ticket_ref: Some("acme/api#5".into()),
            ..job(&t, &format!("try {i}"))
        })
        .await
        .unwrap();
    }
    match tx.resolve_job_ref("#5", &TeamScope::All).await {
        Err(Error::AmbiguousTicket { jobs, more, .. }) => {
            assert_eq!(jobs.len(), 10);
            assert!(more);
            // Same transaction, same `created_at`: the number breaks the tie.
            assert_eq!(jobs[0].as_str(), "job-12");
            assert_eq!(jobs[9].as_str(), "job-3");
        }
        other => panic!("{other:?}"),
    }
    tx.commit().await.unwrap();
}

/// A job id is not looked up here, so whatever the caller does next raises the
/// error it always did for an unknown id.
#[sqlx::test]
async fn resolve_job_ref_passes_a_job_id_through_untouched(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let id = tx.resolve_job_ref("job-7", &TeamScope::All).await.unwrap();
    assert_eq!(id, JobId::from("job-7"));
    assert!(matches!(tx.get_job(&id).await, Err(Error::JobNotFound(_))));
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn resolve_job_refs_resolves_in_order_and_stops_at_the_first_error(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let a = ticketed(&db, &t, t.repo, "a", "acme/api#1").await;
    let b = ticketed(&db, &t, t.repo, "b", "acme/api#2").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let got = tx
        .resolve_job_refs(
            &["#2".into(), "job-1".into(), "acme/api#1".into()],
            &TeamScope::All,
        )
        .await
        .unwrap();
    assert_eq!(got, vec![b, a.clone(), a]);
    let err = tx
        .resolve_job_refs(&["#1".into(), "#3".into(), "#".into()], &TeamScope::All)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "job_not_found", "{err}");
    tx.commit().await.unwrap();
}

/// Org B's job carries the same issue; org A must neither resolve to it nor
/// count it toward ambiguity. Guard 1 (the `org_id` predicate) and guard 2
/// (RLS) each stop it on their own.
#[sqlx::test]
async fn resolve_job_ref_never_crosses_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    // Org B's job numbers run ahead, so its id cannot coincide with A's.
    ticketed(&db, &b, b.repo, "padding", "globex/api#1").await;
    let theirs = ticketed(&db, &b, b.repo, "theirs", "acme/api#5").await;

    let mut tx = db.begin(a.org).await.unwrap();
    for raw in ["#5", "acme/api#5"] {
        let err = tx.resolve_job_ref(raw, &TeamScope::All).await.unwrap_err();
        assert_eq!(err.code(), "job_not_found", "{raw:?}: {err}");
    }
    tx.commit().await.unwrap();

    let mine = ticketed(&db, &a, a.repo, "mine", "acme/api#5").await;
    assert_ne!(mine, theirs);
    let mut tx = db.begin(a.org).await.unwrap();
    for raw in ["#5", "acme/api#5"] {
        assert_eq!(
            tx.resolve_job_ref(raw, &TeamScope::All).await.unwrap(),
            mine,
            "{raw:?}"
        );
    }
    let listed = tx
        .list_jobs(&JobFilter {
            ticket: Some(query("#5")),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        listed.into_iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![mine]
    );
    tx.commit().await.unwrap();
}

/// A job on a repo of a team the caller is not on is neither resolved nor
/// counted: its existence is not revealed by an ambiguity it would cause.
#[sqlx::test]
async fn resolve_job_ref_ignores_jobs_the_scope_cannot_see(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let team = TeamId::from(uuid::Uuid::new_v4());
    let hidden_repo = second_repo(&db, &t, "web", Some(team)).await;
    let open = ticketed(&db, &t, t.repo, "open", "acme/api#5").await;
    let hidden = ticketed(&db, &t, hidden_repo, "hidden", "acme/web#5").await;

    let outsider = TeamScope::Teams(Default::default());
    let insider = TeamScope::Teams([team].into_iter().collect());
    let mut tx = db.begin(t.org).await.unwrap();
    assert_eq!(tx.resolve_job_ref("#5", &outsider).await.unwrap(), open);
    let err = tx
        .resolve_job_ref("acme/web#5", &outsider)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "job_not_found");
    assert!(!err.to_string().contains(hidden.as_str()), "{err}");

    for scope in [&insider, &TeamScope::All] {
        let err = tx.resolve_job_ref("#5", scope).await.unwrap_err();
        assert_eq!(err.code(), "ambiguous_ticket_ref", "{scope:?}");
    }
    assert_eq!(
        tx.resolve_job_ref("acme/web#5", &insider).await.unwrap(),
        hidden
    );
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn list_jobs_by_ticket_returns_that_issues_visible_jobs_newest_first(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let team = TeamId::from(uuid::Uuid::new_v4());
    let web = second_repo(&db, &t, "web", None).await;
    let hidden_repo = second_repo(&db, &t, "ops", Some(team)).await;
    let first = ticketed(&db, &t, t.repo, "first try", "acme/api#5").await;
    ticketed(&db, &t, t.repo, "another issue", "acme/api#6").await;
    let second = ticketed(&db, &t, t.repo, "second try", "Acme/API#5").await;
    let web_job = ticketed(&db, &t, web, "web", "acme/web#5").await;
    let hidden = ticketed(&db, &t, hidden_repo, "hidden", "acme/ops#5").await;
    ticketed(&db, &t, t.repo, "longer number", "acme/api#15").await;

    let ids = |jobs: Vec<tower_core::jobs::Job>| jobs.into_iter().map(|j| j.id).collect::<Vec<_>>();
    let mut tx = db.begin(t.org).await.unwrap();
    let all = tx
        .list_jobs(&JobFilter {
            ticket: Some(query("#5")),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        ids(all),
        vec![hidden, web_job.clone(), second.clone(), first.clone()]
    );

    let visible = tx
        .list_jobs(&JobFilter {
            ticket: Some(query("#5")),
            visible_teams: Some(vec![]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ids(visible), vec![web_job, second.clone(), first.clone()]);

    let full = tx
        .list_jobs(&JobFilter {
            ticket: Some(query("acme/api#5")),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ids(full), vec![second, first]);

    // No ticket filter: unchanged, every job.
    let unfiltered = tx.list_jobs(&JobFilter::default()).await.unwrap();
    assert_eq!(unfiltered.len(), 6);
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn list_leases_carries_the_jobs_ticket_ref(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let team = TeamId::from(uuid::Uuid::new_v4());
    let linked = ticketed(&db, &t, t.repo, "linked", "acme/api#928").await;
    let mut tx = db.begin(t.org).await.unwrap();
    // A job on the org-wide repo, but of a team: hidden from an outsider.
    sqlx::query("UPDATE jobs SET team_id = $2 WHERE org_id = $1 AND id = $3")
        .bind(t.org)
        .bind(team)
        .bind(&linked)
        .execute(tx.conn())
        .await
        .unwrap();
    let unlinked = tx.add_job(job(&t, "unlinked")).await.unwrap().id;
    let with_job = tx
        .acquire_lease(t.repo, "branch:a", t.user, None, Some(&linked), None)
        .await
        .unwrap();
    // RETURNING carries no join: the column defaults.
    assert_eq!(with_job.job_ticket_ref, None);
    tx.acquire_lease(t.repo, "branch:b", t.user, None, Some(&unlinked), None)
        .await
        .unwrap();
    tx.acquire_lease(t.repo, "branch:c", t.user, None, None, None)
        .await
        .unwrap();

    let leases = tx.list_leases(Some(t.repo)).await.unwrap();
    let by_resource = |leases: &[tower_core::leases::Lease], r: &str| {
        let l = leases.iter().find(|l| l.resource == r).unwrap();
        (l.job_id.clone(), l.job_ticket_ref.clone())
    };
    assert_eq!(
        by_resource(&leases, "branch:a"),
        (Some(linked.0.clone()), Some("acme/api#928".into()))
    );
    assert_eq!(
        by_resource(&leases, "branch:b"),
        (Some(unlinked.0.clone()), None)
    );
    assert_eq!(by_resource(&leases, "branch:c"), (None, None));
    let a = leases.iter().find(|l| l.resource == "branch:a").unwrap();
    let json = serde_json::to_value(a).unwrap();
    assert_eq!(json["jobTicketRef"], "acme/api#928");

    let mut hidden = leases.clone();
    tower_core::leases::hide_unseen_jobs(
        &mut tx,
        &mut hidden,
        &TeamScope::Teams(Default::default()),
    )
    .await
    .unwrap();
    assert_eq!(by_resource(&hidden, "branch:a"), (None, None));
    assert_eq!(
        by_resource(&hidden, "branch:b"),
        (Some(unlinked.0.clone()), None)
    );

    let mut shown = leases.clone();
    tower_core::leases::hide_unseen_jobs(
        &mut tx,
        &mut shown,
        &TeamScope::Teams([team].into_iter().collect()),
    )
    .await
    .unwrap();
    assert_eq!(shown, leases);
    tx.commit().await.unwrap();
}

/// A job in its own committed transaction, so a later transaction's `now()`
/// is strictly after its `created_at`.
async fn fresh(
    db: &savvagent_tenant::Db,
    t: &common::Tenant,
    title: &str,
) -> tower_core::jobs::Job {
    let mut tx = db.begin(t.org).await.unwrap();
    let j = tx.add_job(job(t, title)).await.unwrap();
    tx.commit().await.unwrap();
    j
}

async fn note(
    db: &savvagent_tenant::Db,
    t: &common::Tenant,
    id: &JobId,
    at: chrono::DateTime<chrono::Utc>,
) {
    let mut tx = db.begin(t.org).await.unwrap();
    tx.note_ticket_changed(id, at).await.unwrap();
    tx.commit().await.unwrap();
}

async fn read(db: &savvagent_tenant::Db, t: &common::Tenant, id: &JobId) -> tower_core::jobs::Job {
    let mut tx = db.begin(t.org).await.unwrap();
    let j = tx.get_job(id).await.unwrap();
    tx.commit().await.unwrap();
    j
}

/// `now()` is the transaction's start, and two transactions can start in the
/// same microsecond; a short pause makes "a later write" unambiguous.
async fn tick() {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

#[sqlx::test]
async fn a_new_job_is_not_ticket_stale(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    assert!(!j.ticket_stale);
    assert_eq!(j.ticket_changed_at, None);
    let j = read(&db, &t, &j.id).await;
    assert!(!j.ticket_stale);
    assert_eq!(j.ticket_changed_at, None);
}

#[sqlx::test]
async fn a_change_after_creation_marks_the_job_stale(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    let at = chrono::Utc::now() + chrono::Duration::minutes(1);
    note(&db, &t, &j.id, at).await;
    let j = read(&db, &t, &j.id).await;
    assert!(j.ticket_stale);
    assert_eq!(
        j.ticket_changed_at.map(|t| t.timestamp_micros()),
        Some(at.timestamp_micros())
    );
}

#[sqlx::test]
async fn rewriting_the_description_or_title_clears_the_mark_and_metadata_does_not(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    for field in ["description", "title"] {
        let j = fresh(&db, &t, "work").await;
        // Just after creation and well before the rewrite below: a change the
        // rewrite has absorbed.
        note(
            &db,
            &t,
            &j.id,
            j.created_at + chrono::Duration::milliseconds(1),
        )
        .await;
        tick().await;

        let mut tx = db.begin(t.org).await.unwrap();
        let meta = serde_json::json!({"k": "v"});
        let after_meta = tx
            .update_job(&j.id, None, None, None, Some(&meta))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(after_meta.ticket_stale, "metadata alone must not clear it");

        let mut tx = db.begin(t.org).await.unwrap();
        let updated = match field {
            "description" => tx.update_job(&j.id, None, Some("new body"), None, None),
            _ => tx.update_job(&j.id, Some("new title"), None, None, None),
        }
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(!updated.ticket_stale, "{field} rewrite must clear it");
        assert!(!read(&db, &t, &j.id).await.ticket_stale, "{field}");
        // The foreign change is still recorded; only its staleness is gone.
        assert!(updated.ticket_changed_at.is_some());
    }
}

#[sqlx::test]
async fn a_change_older_than_the_job_does_not_mark_it(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    note(&db, &t, &j.id, j.created_at - chrono::Duration::minutes(5)).await;
    let j = read(&db, &t, &j.id).await;
    assert!(!j.ticket_stale);
    assert!(j.ticket_changed_at.is_some());
}

#[sqlx::test]
async fn note_ticket_changed_keeps_the_later_of_two_times(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    let later = chrono::Utc::now() + chrono::Duration::minutes(10);
    let earlier = later - chrono::Duration::minutes(5);
    note(&db, &t, &j.id, later).await;
    note(&db, &t, &j.id, earlier).await;
    let got = read(&db, &t, &j.id).await.ticket_changed_at.unwrap();
    assert_eq!(got.timestamp_micros(), later.timestamp_micros());
}

/// Org B naming org A's job id marks nothing in A: whether B has a job with
/// the same id (its own row is the one written) or none (`job_not_found`).
#[sqlx::test]
async fn note_ticket_changed_never_crosses_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let theirs = fresh(&db, &a, "a's work").await;
    let at = chrono::Utc::now() + chrono::Duration::minutes(1);

    let mut tx = db.begin(b.org).await.unwrap();
    let err = tx.note_ticket_changed(&theirs.id, at).await.unwrap_err();
    assert_eq!(err.code(), "job_not_found", "{err}");
    drop(tx);
    let a_job = read(&db, &a, &theirs.id).await;
    assert!(!a_job.ticket_stale);
    assert_eq!(a_job.ticket_changed_at, None);

    let mine = fresh(&db, &b, "b's work").await;
    assert_eq!(mine.id, theirs.id, "both orgs number from job-1");
    note(&db, &b, &mine.id, at).await;
    assert!(read(&db, &b, &mine.id).await.ticket_stale);
    let a_job = read(&db, &a, &theirs.id).await;
    assert!(!a_job.ticket_stale);
    assert_eq!(a_job.ticket_changed_at, None);
}

/// The webhook finds its job by an unlocked read of the ticket link, so its
/// mark is conditional: once the job is relinked, the old issue's change does
/// not land on it.
#[sqlx::test]
async fn a_conditional_mark_skips_a_job_relinked_in_between(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    let at = chrono::Utc::now() + chrono::Duration::minutes(1);

    let mut tx = db.begin(t.org).await.unwrap();
    tx.link_ticket(&j.id, Tracker::Github, "acme/api#1")
        .await
        .unwrap();
    tx.link_ticket(&j.id, Tracker::Github, "acme/api#2")
        .await
        .unwrap();
    let marked = tx
        .note_ticket_changed_if_linked(&j.id, Tracker::Github, "acme/api#1", at)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(!marked);
    assert_eq!(read(&db, &t, &j.id).await.ticket_changed_at, None);

    let mut tx = db.begin(t.org).await.unwrap();
    let marked = tx
        .note_ticket_changed_if_linked(&j.id, Tracker::Github, "acme/api#2", at)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(marked);
    assert!(read(&db, &t, &j.id).await.ticket_stale);
}

/// `link_ticket` to `ticket` and mark it at `at`, as a comment webhook would.
async fn link_and_mark(
    db: &savvagent_tenant::Db,
    t: &common::Tenant,
    id: &JobId,
    ticket: &str,
    at: chrono::DateTime<chrono::Utc>,
) {
    let mut tx = db.begin(t.org).await.unwrap();
    tx.link_ticket(id, Tracker::Github, ticket).await.unwrap();
    assert!(tx
        .note_ticket_changed_if_linked(id, Tracker::Github, ticket, at)
        .await
        .unwrap());
    tx.commit().await.unwrap();
}

/// The old ticket's mark describes a ticket the job no longer points at: a
/// relink clears it, and a change on the new ticket earlier than the old mark
/// still marks the job.
#[sqlx::test]
async fn relinking_to_another_ticket_clears_the_old_tickets_mark(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    let old_mark = chrono::Utc::now() + chrono::Duration::minutes(10);
    link_and_mark(&db, &t, &j.id, "acme/api#1", old_mark).await;
    assert!(read(&db, &t, &j.id).await.ticket_stale);

    let mut tx = db.begin(t.org).await.unwrap();
    let relinked = tx
        .link_ticket(&j.id, Tracker::Github, "acme/api#2")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(relinked.ticket_changed_at, None);
    assert!(!relinked.ticket_stale);
    let reread = read(&db, &t, &j.id).await;
    assert_eq!(reread.ticket_changed_at, None);
    assert!(!reread.ticket_stale);

    let new_change = old_mark - chrono::Duration::minutes(9);
    let mut tx = db.begin(t.org).await.unwrap();
    assert!(tx
        .note_ticket_changed_if_linked(&j.id, Tracker::Github, "acme/api#2", new_change)
        .await
        .unwrap());
    tx.commit().await.unwrap();
    let marked = read(&db, &t, &j.id).await;
    assert!(marked.ticket_stale);
    assert_eq!(
        marked.ticket_changed_at.map(|t| t.timestamp_micros()),
        Some(new_change.timestamp_micros())
    );
}

/// Relinking to the ticket the job already points at changes nothing the job
/// describes, so an unread change on that ticket stays marked.
#[sqlx::test]
async fn relinking_to_the_same_ticket_keeps_its_mark(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    let at = chrono::Utc::now() + chrono::Duration::minutes(10);
    link_and_mark(&db, &t, &j.id, "acme/api#1", at).await;

    let mut tx = db.begin(t.org).await.unwrap();
    let relinked = tx
        .link_ticket(&j.id, Tracker::Github, "acme/api#1")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(relinked.ticket_stale);
    let reread = read(&db, &t, &j.id).await;
    assert!(reread.ticket_stale);
    assert_eq!(
        reread.ticket_changed_at.map(|t| t.timestamp_micros()),
        Some(at.timestamp_micros())
    );
}

/// The ticket is the pair (tracker, ticket_ref): the same reference text on
/// another tracker names a different ticket, so the old tracker's mark goes.
#[sqlx::test]
async fn relinking_to_another_tracker_with_the_same_ref_clears_the_mark(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    let at = chrono::Utc::now() + chrono::Duration::minutes(10);
    link_and_mark(&db, &t, &j.id, "acme/api#1", at).await;

    let mut tx = db.begin(t.org).await.unwrap();
    let relinked = tx
        .link_ticket(&j.id, Tracker::Jira, "acme/api#1")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(relinked.ticket_changed_at, None);
    assert!(!relinked.ticket_stale);
    let reread = read(&db, &t, &j.id).await;
    assert_eq!(reread.tracker, Some(Tracker::Jira));
    assert_eq!(reread.ticket_changed_at, None);
}

/// A relink refused as ticket_already_linked changes nothing, the mark
/// included: the clear rides in the same statement the refusal rolls back.
#[sqlx::test]
async fn a_refused_relink_keeps_the_mark(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    let at = chrono::Utc::now() + chrono::Duration::minutes(10);
    link_and_mark(&db, &t, &j.id, "acme/api#1", at).await;

    let mut tx = db.begin(t.org).await.unwrap();
    tx.create_from_ticket(t.repo, Tracker::Github, "acme/api#2", "holder", None, None)
        .await
        .unwrap();
    let err = tx
        .link_ticket(&j.id, Tracker::Github, "acme/api#2")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();
    assert!(
        matches!(err, Error::TicketAlreadyLinked { .. }),
        "expected TicketAlreadyLinked, got {err:?}"
    );

    let reread = read(&db, &t, &j.id).await;
    assert_eq!(reread.ticket_ref.as_deref(), Some("acme/api#1"));
    assert!(reread.ticket_stale);
    assert_eq!(
        reread.ticket_changed_at.map(|t| t.timestamp_micros()),
        Some(at.timestamp_micros())
    );
}

/// A webhook issue edit rewrites the description but not `described_at`: a
/// comment noted before it is still not in the description, so the job stays
/// stale until `update_job` says the issue was re-read.
#[sqlx::test]
async fn update_from_ticket_keeps_an_earlier_mark(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let j = fresh(&db, &t, "work").await;
    note(
        &db,
        &t,
        &j.id,
        j.created_at + chrono::Duration::milliseconds(1),
    )
    .await;
    tick().await;

    let mut tx = db.begin(t.org).await.unwrap();
    let updated = tx
        .update_from_ticket(&j.id, "edited title", Some("edited body"), None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(updated.description.as_deref(), Some("edited body"));
    assert!(updated.ticket_stale);
    assert!(read(&db, &t, &j.id).await.ticket_stale);
}

// ------------------------------------------------- add_ticket_dependencies

use tower_core::jobs::{BlockerKind, BlockerOutcome};

fn outcome(kind: BlockerKind, ticket: &str, jobs: &[&JobId]) -> BlockerOutcome {
    BlockerOutcome {
        ticket_ref: ticket.into(),
        outcome: kind,
        jobs: jobs.iter().map(|j| (*j).clone()).collect(),
    }
}

/// Every outcome in one call, in order. A `cycle` checked before its insert
/// leaves the transaction usable, so the `added` after it still lands.
#[sqlx::test]
async fn add_ticket_dependencies_reports_each_reference(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let me = ticketed(&db, &t, t.repo, "me", "acme/api#1").await;
    let blocker = ticketed(&db, &t, t.repo, "blocker", "acme/api#2").await;
    let twin_a = ticketed(&db, &t, t.repo, "first try", "acme/api#3").await;
    let twin_b = ticketed(&db, &t, t.repo, "second try", "acme/api#3").await;
    let mut tx = db.begin(t.org).await.unwrap();
    let dependent = tx
        .add_job(NewJob {
            ticket_ref: Some("acme/api#4".into()),
            depends_on: vec![me.clone()],
            ..job(&t, "waits for me")
        })
        .await
        .unwrap()
        .id;
    tx.commit().await.unwrap();
    let later = ticketed(&db, &t, t.repo, "later", "acme/api#5").await;

    let refs: Vec<TicketQuery> = [
        "acme/api#2",
        "acme/api#9",
        "acme/api#3",
        "acme/api#4",
        "acme/api#1",
        "acme/api#5",
    ]
    .into_iter()
    .map(query)
    .collect();
    let mut tx = db.begin(t.org).await.unwrap();
    let got = tx
        .add_ticket_dependencies(&me, "acme/api", &refs, &TeamScope::All)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        got,
        vec![
            outcome(BlockerKind::Added, "acme/api#2", &[&blocker]),
            outcome(BlockerKind::NoJob, "acme/api#9", &[]),
            outcome(BlockerKind::Ambiguous, "acme/api#3", &[&twin_b, &twin_a]),
            outcome(BlockerKind::Cycle, "acme/api#4", &[&dependent]),
            outcome(BlockerKind::Cycle, "acme/api#1", &[&me]),
            outcome(BlockerKind::Added, "acme/api#5", &[&later]),
        ]
    );

    let mut tx = db.begin(t.org).await.unwrap();
    // `dependencies_of` orders by id, and both ids are single-digit.
    assert_eq!(
        tx.dependencies_of(&me).await.unwrap(),
        vec![blocker.clone(), later.clone()],
        "only the added ones were written"
    );

    // Again: what is already there says so and writes nothing new.
    let again = tx
        .add_ticket_dependencies(&me, "acme/api", &[query("acme/api#2")], &TeamScope::All)
        .await
        .unwrap();
    assert_eq!(
        again,
        vec![outcome(BlockerKind::Already, "acme/api#2", &[&blocker])]
    );
    assert_eq!(tx.dependencies_of(&me).await.unwrap().len(), 2);
    tx.commit().await.unwrap();
}

/// Org B's job carries the issue org A's line names: for A it does not exist.
#[sqlx::test]
async fn add_ticket_dependencies_never_crosses_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    ticketed(&db, &b, b.repo, "padding", "globex/api#1").await;
    ticketed(&db, &b, b.repo, "theirs", "acme/api#5").await;
    let me = ticketed(&db, &a, a.repo, "me", "acme/api#1").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let got = tx
        .add_ticket_dependencies(&me, "acme/api", &[query("acme/api#5")], &TeamScope::All)
        .await
        .unwrap();
    assert_eq!(got, vec![outcome(BlockerKind::NoJob, "acme/api#5", &[])]);
    assert!(tx.dependencies_of(&me).await.unwrap().is_empty());
    tx.commit().await.unwrap();
}

/// A candidate the caller's teams cannot see is `no_job`, never written and
/// never named. A job the caller cannot see is refused outright.
#[sqlx::test]
async fn add_ticket_dependencies_respects_the_team_scope(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let team = TeamId::from(uuid::Uuid::new_v4());
    let hidden_repo = second_repo(&db, &t, "web", Some(team)).await;
    let me = ticketed(&db, &t, t.repo, "me", "acme/api#1").await;
    // The same GitHub repository's issue, queued in a repo of a hidden team.
    let hidden = ticketed(&db, &t, hidden_repo, "hidden", "acme/api#5").await;

    let outsider = TeamScope::Teams(Default::default());
    let insider = TeamScope::Teams([team].into_iter().collect());

    let mut tx = db.begin(t.org).await.unwrap();
    let got = tx
        .add_ticket_dependencies(&me, "acme/api", &[query("acme/api#5")], &outsider)
        .await
        .unwrap();
    assert_eq!(got, vec![outcome(BlockerKind::NoJob, "acme/api#5", &[])]);
    assert!(tx.dependencies_of(&me).await.unwrap().is_empty());

    let err = tx
        .add_ticket_dependencies(&hidden, "acme/api", &[query("acme/api#1")], &outsider)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "job_not_found");
    tx.commit().await.unwrap();

    // The same line, for a caller on the team, lands: the refusals above are
    // the scope's doing.
    let mut tx = db.begin(t.org).await.unwrap();
    let got = tx
        .add_ticket_dependencies(&me, "acme/api", &[query("acme/api#5")], &insider)
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![outcome(BlockerKind::Added, "acme/api#5", &[&hidden])]
    );
    tx.commit().await.unwrap();
}

/// A blocker in another repository is `other_repo`: never looked up, never
/// written, never named, even when exactly one visible job carries it. The
/// home repo compares case-insensitively.
#[sqlx::test]
async fn add_ticket_dependencies_only_follows_the_issues_own_repo(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let me = ticketed(&db, &t, t.repo, "me", "acme/api#1").await;
    ticketed(&db, &t, t.repo, "elsewhere", "acme/web#5").await;
    let same = ticketed(&db, &t, t.repo, "same repo", "acme/api#6").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let got = tx
        .add_ticket_dependencies(
            &me,
            "Acme/API",
            &[query("acme/web#5"), query("acme/api#6")],
            &TeamScope::All,
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![
            outcome(BlockerKind::OtherRepo, "acme/web#5", &[]),
            outcome(BlockerKind::Added, "acme/api#6", &[&same]),
        ]
    );
    assert_eq!(tx.dependencies_of(&me).await.unwrap(), vec![same]);
    tx.commit().await.unwrap();
}

/// Two dependency writes in one org run one at a time. Transaction 1 adds
/// A→B and holds; transaction 2's B→A waits for it, and once 1 commits, 2's
/// cycle check sees A→B and refuses. Without the lock, 2 would check before
/// 1 committed, see no path, and both edges would land: a loop.
#[sqlx::test]
async fn concurrent_dependency_writes_cannot_close_a_loop(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let a = ticketed(&db, &t, t.repo, "a", "acme/api#1").await;
    let b = ticketed(&db, &t, t.repo, "b", "acme/api#2").await;

    let mut first = db.begin(t.org).await.unwrap();
    first
        .set_dependencies(&a, std::slice::from_ref(&b), &[])
        .await
        .unwrap();

    let second = {
        let db = db.clone();
        let (a, b, org) = (a.clone(), b.clone(), t.org);
        tokio::spawn(async move {
            let mut tx = db.begin(org).await.unwrap();
            let got = tx.set_dependencies(&b, std::slice::from_ref(&a), &[]).await;
            if got.is_ok() {
                tx.commit().await.unwrap();
            }
            got
        })
    };
    // Still waiting on the lock: it has not finished in this long.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !second.is_finished(),
        "the second write ran without waiting"
    );

    first.commit().await.unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(10), second)
        .await
        .expect("the second write finishes once the first commits")
        .unwrap();
    assert!(matches!(got, Err(Error::DependencyCycle(..))), "{got:?}");

    let mut tx = db.begin(t.org).await.unwrap();
    assert_eq!(tx.dependencies_of(&a).await.unwrap(), vec![b.clone()]);
    assert!(tx.dependencies_of(&b).await.unwrap().is_empty());
    tx.commit().await.unwrap();

    // The blocker import queues on the same lock.
    let mut first = db.begin(t.org).await.unwrap();
    first
        .set_dependencies(&a, &[], std::slice::from_ref(&b))
        .await
        .unwrap();
    first
        .set_dependencies(&b, std::slice::from_ref(&a), &[])
        .await
        .unwrap();
    let second = {
        let db = db.clone();
        let (a, org) = (a.clone(), t.org);
        tokio::spawn(async move {
            let mut tx = db.begin(org).await.unwrap();
            let got = tx
                .add_ticket_dependencies(&a, "acme/api", &[query("acme/api#2")], &TeamScope::All)
                .await;
            tx.commit().await.unwrap();
            got
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!second.is_finished(), "the import ran without waiting");
    first.commit().await.unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(10), second)
        .await
        .expect("the import finishes once the first commits")
        .unwrap()
        .unwrap();
    assert_eq!(got, vec![outcome(BlockerKind::Cycle, "acme/api#2", &[&b])]);
}

// ------------------------------------------- references on tools that write

/// A write takes a job id or a full reference; a short `#N` is refused before
/// any lookup, naming the forms that work, even when exactly one job has it.
#[sqlx::test]
async fn a_write_refuses_a_short_reference(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let id = ticketed(&db, &t, t.repo, "only one", "acme/api#13").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let e = tx
        .resolve_job_ref_for_write("#13", &TeamScope::All)
        .await
        .unwrap_err();
    assert_eq!(e.code(), "invalid_argument");
    let msg = e.to_string();
    assert!(msg.contains("owner/repo#13"), "{msg}");
    assert!(msg.contains("list_jobs(ticket=\"#13\")"), "{msg}");

    for raw in ["acme/api#13", id.0.as_str()] {
        assert_eq!(
            tx.resolve_job_ref_for_write(raw, &TeamScope::All)
                .await
                .unwrap(),
            id,
            "{raw}"
        );
    }
    let e = tx
        .resolve_job_refs_for_write(&["acme/api#13".into(), "#13".into()], &TeamScope::All)
        .await
        .unwrap_err();
    assert_eq!(e.code(), "invalid_argument");
    tx.commit().await.unwrap();
}

/// A dependency's short `#N` is issue N of the home repo, not of any repo;
/// with no home it is refused with the reason given.
#[sqlx::test]
async fn a_short_dependency_reads_in_the_home_repo(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let api = ticketed(&db, &t, t.repo, "api 14", "acme/api#14").await;
    ticketed(&db, &t, t.repo, "web 14", "acme/web#14").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let got = tx
        .resolve_dependency_refs(&["#14".into()], Some("Acme/API"), "unused", &TeamScope::All)
        .await
        .unwrap();
    assert_eq!(got, vec![api.clone()]);

    // Org-wide, the same `#14` is ambiguous; a full reference is never re-read.
    let got = tx
        .resolve_dependency_refs(
            &["acme/web#14".into(), api.0.clone()],
            Some("acme/api"),
            "unused",
            &TeamScope::All,
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[1], api);

    let e = tx
        .resolve_dependency_refs(
            &["#14".into()],
            None,
            "job-9 has no ticketRef of the form owner/repo#N",
            &TeamScope::All,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), "invalid_argument");
    let msg = e.to_string();
    assert!(msg.contains("job-9 has no ticketRef"), "{msg}");
    assert!(msg.contains("owner/repo#14"), "{msg}");
    tx.commit().await.unwrap();
}

/// `lock_job_visible` holds the job's row: a relink of the job waits for the
/// holder to finish rather than committing under it.
#[sqlx::test]
async fn lock_job_visible_makes_a_relink_wait(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let id = ticketed(&db, &t, t.repo, "linked", "acme/api#13").await;

    let mut holder = db.begin(t.org).await.unwrap();
    let job = holder.lock_job_visible(&id, &TeamScope::All).await.unwrap();
    assert_eq!(job.ticket_ref.as_deref(), Some("acme/api#13"));

    let relink = {
        let (db, id, org) = (db.clone(), id.clone(), t.org);
        tokio::spawn(async move {
            let mut tx = db.begin(org).await.unwrap();
            tx.link_ticket(&id, Tracker::Github, "acme/api#99")
                .await
                .unwrap();
            tx.commit().await.unwrap();
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!relink.is_finished(), "the relink did not wait");
    holder.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), relink)
        .await
        .expect("the relink finishes once the holder commits")
        .unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    let e = tx
        .lock_job_visible(&JobId::from("job-999"), &TeamScope::All)
        .await
        .unwrap_err();
    assert_eq!(e.code(), "job_not_found");
    tx.commit().await.unwrap();
}
