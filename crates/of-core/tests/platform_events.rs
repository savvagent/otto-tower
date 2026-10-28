//! The platform's lifecycle webhooks, applied.
//!
//! Each test drives `of_core::platform_events::apply` with a verified event, the
//! way `POST /platform/webhooks` does, against a real Postgres.

mod common;

use common::{db, job, tenant, Member};
use of_core::jobs::{JobsExt, Status};
use of_core::leases::LeasesExt;
use of_core::messages::{ChannelRef, MessagesExt, NewMessage};
use of_core::platform_events::{apply, begin_live, revoked, Outcome};
use of_core::repos::{NewRepo, RepoPatch, ReposExt};
use of_core::teams::{TeamScope, VerifiedTeam};
use of_core::trackers::{upsert_binding, upsert_connection, Provider};
use of_testkit::MockPlatform;
use otto_resource::webhook::{self, WebhookEvent};
use otto_tenant::ids::{OrgId, TeamId};
use sqlx::PgPool;
use uuid::Uuid;

fn event(kind: &str, data: serde_json::Value) -> WebhookEvent {
    let (header, body) = MockPlatform::webhook(kind, data);
    webhook::verify(of_testkit::WEBHOOK_SECRET, &header, &body).expect("a verified event")
}

async fn count(db: &otto_tenant::Db, org: OrgId, table: &str) -> i64 {
    let mut tx = db.begin(org).await.unwrap();
    let n = sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE org_id = $1"))
        .bind(org)
        .fetch_one(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    n
}

/// Put something in every table an org can own.
async fn populate(db: &otto_tenant::Db, t: &common::Tenant) {
    let mut tx = db.begin(t.org).await.unwrap();
    let j1 = tx.add_job(job(t, "one")).await.unwrap();
    let j2 = tx.add_job(job(t, "two")).await.unwrap();
    tx.set_dependencies(&j2.id, std::slice::from_ref(&j1.id), &[])
        .await
        .unwrap();
    tx.acquire_lease(t.repo, "branch:main", t.user, Some("agent"), None, None)
        .await
        .unwrap();
    let hello = tx
        .send_message(
            t.user,
            NewMessage {
                body: "hello".into(),
                repo_id: Some(t.repo),
                job_id: Some(j1.id.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tx.ack_messages_for(t.user, ChannelRef::Repo(t.repo), hello.id, &TeamScope::All)
        .await
        .unwrap();
    let conn = upsert_connection(
        &mut tx,
        Provider::Github,
        &format!("install-{}", t.org),
        None,
        None,
    )
    .await
    .unwrap();
    upsert_binding(
        &mut tx,
        t.repo,
        Some(conn.id),
        Provider::Github,
        "acme/api",
        "otto-factory",
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO usage_outbox (org_id, user_id, tool, billable) VALUES ($1, $2, 'add_job', true)")
        .bind(t.org)
        .bind(t.user)
        .execute(tx.conn())
        .await
        .unwrap();
    tx.audit(otto_tenant::audit::Entry::new("repo.registered"))
        .await
        .unwrap();
    of_core::tracker_deliveries::record(&mut tx, Provider::Github, Some("delivery-1"), &[7; 32])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

const TABLES: [&str; 14] = [
    "repos",
    "repo_remotes",
    "repo_leases",
    "jobs",
    "job_dependencies",
    "messages",
    "message_cursors",
    "tracker_connections",
    "tracker_bindings",
    "tracker_connection_index",
    "org_counters",
    "usage_outbox",
    "audit_events",
    "tracker_deliveries",
];

#[sqlx::test]
async fn org_deleted_purges_that_org_and_only_that_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    populate(&db, &a).await;
    populate(&db, &b).await;
    for table in TABLES {
        assert!(
            count(&db, a.org, table).await > 0,
            "{table} was not populated"
        );
    }
    let b_before: Vec<i64> = {
        let mut v = vec![];
        for table in TABLES {
            v.push(count(&db, b.org, table).await);
        }
        v
    };

    let ev = event(
        "org.deleted",
        serde_json::json!({ "org_id": a.org.as_uuid() }),
    );
    let outcome = apply(&db, &ev).await.unwrap();
    assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome:?}");

    for table in TABLES {
        assert_eq!(
            count(&db, a.org, table).await,
            0,
            "{table} survived org.deleted"
        );
    }
    // The other tenant is untouched, row for row.
    for (table, before) in TABLES.iter().zip(b_before) {
        assert_eq!(
            count(&db, b.org, table).await,
            before,
            "{table} of another org changed"
        );
    }
    // The global connection index has no RLS: make sure the purge was scoped.
    let global: i64 = sqlx::query_scalar("SELECT count(*) FROM tracker_connection_index")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        global, 1,
        "org.deleted touched another org's connection index"
    );
}

#[sqlx::test]
async fn a_redelivery_reruns_the_cleanup_and_catches_work_that_raced_the_purge(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let id = Uuid::new_v4();
    let (header, body) = MockPlatform::webhook_with_id(
        id,
        "org.deleted",
        serde_json::json!({ "org_id": a.org.as_uuid() }),
    );
    let ev = webhook::verify(of_testkit::WEBHOOK_SECRET, &header, &body).unwrap();

    assert!(matches!(
        apply(&db, &ev).await.unwrap(),
        Outcome::Applied { .. }
    ));
    assert!(
        revoked(&db, a.org, a.user).await.unwrap(),
        "the org is tombstoned"
    );

    // A transaction that began before the purge commits after it.
    let mut tx = db.begin(a.org).await.unwrap();
    tx.register_repo(NewRepo {
        slug: "late".into(),
        remotes: vec!["git@github.com:acme/late.git".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&db, a.org, "repos").await, 1);

    // The platform's redelivery is acknowledged as a repeat, and sweeps it up.
    assert_eq!(apply(&db, &ev).await.unwrap(), Outcome::Duplicate);
    assert_eq!(
        count(&db, a.org, "repos").await,
        0,
        "a write that raced the purge survived"
    );
    assert_eq!(
        count(&db, a.org, "platform_events").await,
        1,
        "one marker, not one per delivery"
    );
}

/// A request that passed authentication before the tombstone and commits after
/// it must still be purged by the one delivery, and a request that starts once
/// the purge is under way must be refused, not written and left behind.
#[sqlx::test]
async fn a_write_in_flight_is_purged_and_a_later_one_refused_without_redelivery(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    // Past authentication, transaction open: the request is in flight.
    let mut writer = begin_live(&db, a.org, Some(a.user)).await.unwrap();

    let purge = {
        let db = db.clone();
        let ev = event(
            "org.deleted",
            serde_json::json!({ "org_id": a.org.as_uuid() }),
        );
        tokio::spawn(async move { apply(&db, &ev).await })
    };

    // The tombstone lands at once; the purge itself waits for the writer.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !revoked(&db, a.org, a.user).await.unwrap() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the tombstone was never written");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !purge.is_finished(),
        "the purge ran while a request transaction for the org was still open"
    );

    // A request that starts now queues behind the purge rather than slipping
    // in ahead of it, and is refused once the purge is done.
    let late_user = {
        let db = db.clone();
        tokio::spawn(async move { begin_live(&db, a.org, Some(a.user)).await.map(|_| ()) })
    };
    let late_nobody = {
        let db = db.clone();
        tokio::spawn(async move { begin_live(&db, a.org, None).await.map(|_| ()) })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!late_user.is_finished() && !late_nobody.is_finished());

    writer
        .register_repo(NewRepo {
            slug: "late".into(),
            remotes: vec!["git@github.com:acme/late.git".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    writer.commit().await.unwrap();

    assert!(matches!(
        purge.await.unwrap().unwrap(),
        Outcome::Applied { .. }
    ));
    assert_eq!(
        count(&db, a.org, "repos").await,
        0,
        "a write that was in flight during the purge survived one delivery"
    );
    for late in [late_user, late_nobody] {
        let res = late.await.unwrap();
        assert!(
            matches!(res, Err(of_core::Error::AccessRevoked)),
            "a request that queued behind the purge was let in: {res:?}"
        );
    }
    // Another org is neither blocked nor touched.
    let tx = begin_live(&db, b.org, Some(b.user)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&db, b.org, "repos").await, 1);
}

/// `member.removed` takes the same lock, and a removed member's next
/// transaction is refused while the rest of the org carries on.
#[sqlx::test]
async fn a_removed_members_transaction_is_refused_and_others_are_not(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let other = Member::new();
    apply(
        &db,
        &event(
            "member.removed",
            serde_json::json!({ "org_id": a.org.as_uuid(), "user_id": a.user.as_uuid() }),
        ),
    )
    .await
    .unwrap();

    let res = begin_live(&db, a.org, Some(a.user)).await;
    assert!(
        matches!(res, Err(of_core::Error::AccessRevoked)),
        "{:?}",
        res.err()
    );
    begin_live(&db, a.org, Some(other.id))
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    begin_live(&db, a.org, None)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
}

#[sqlx::test]
async fn a_removed_member_is_refused_briefly_and_then_the_platform_decides(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = Member::new().id;
    assert!(!revoked(&db, a.org, bob).await.unwrap());

    let ev = event(
        "member.removed",
        serde_json::json!({ "org_id": a.org.as_uuid(), "user_id": bob.as_uuid() }),
    );
    apply(&db, &ev).await.unwrap();
    assert!(revoked(&db, a.org, bob).await.unwrap());
    assert!(
        !revoked(&db, a.org, a.user).await.unwrap(),
        "only the removed user"
    );
    assert!(
        !revoked(&db, OrgId::new(), bob).await.unwrap(),
        "only in that org"
    );

    // Work that raced the removal (a claim and a lease taken by a request that
    // was already in flight) is released by the re-run.
    let mut tx = db.begin(a.org).await.unwrap();
    tx.acquire_lease(a.repo, "branch:late", bob, None, None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(apply(&db, &ev).await.unwrap(), Outcome::Duplicate);
    let mut tx = db.begin(a.org).await.unwrap();
    assert!(tx.list_leases(None).await.unwrap().is_empty());
    tx.commit().await.unwrap();

    // The tombstone is short-lived: once the platform's own cache has caught up
    // it is the platform's word that counts, so a re-added member is not locked out.
    sqlx::query("UPDATE removed_members SET removed_at = now() - interval '1 hour'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(!revoked(&db, a.org, bob).await.unwrap());
    assert!(of_core::platform_events::sweep(&db, 30).await.unwrap() >= 1);
}

#[sqlx::test]
async fn the_sweep_forgets_old_event_markers_only(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    apply(
        &db,
        &event(
            "team.deleted",
            serde_json::json!({ "org_id": a.org.as_uuid(), "team_id": TeamId::new().as_uuid() }),
        ),
    )
    .await
    .unwrap();
    apply(
        &db,
        &event(
            "team.deleted",
            serde_json::json!({ "org_id": a.org.as_uuid(), "team_id": TeamId::new().as_uuid() }),
        ),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE platform_events SET received_at = now() - interval '40 days' WHERE ctid = (SELECT ctid FROM platform_events LIMIT 1)")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(of_core::platform_events::sweep(&db, 30).await.unwrap(), 1);
    assert_eq!(count(&db, a.org, "platform_events").await, 1);
}

/// The housekeeping runs on the pool with no org pinned. `#[sqlx::test]`
/// connects as a superuser, which bypasses row-level security, so the test above
/// cannot tell a working sweep from one that matches zero rows. Here the pool
/// runs as `otto_app`, a role RLS binds, which is the shape of a deployment
/// whose connecting role neither is a superuser nor has `BYPASSRLS`.
#[sqlx::test]
async fn housekeeping_deletes_where_row_level_security_applies(pool: PgPool) {
    let db = db(pool.clone());
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    // On such a deployment the connecting role owns the tables and so holds
    // DELETE on the audit trail; `otto_app` has it revoked. Grant it here, in
    // this throwaway database, so that what is under test is the policies.
    sqlx::query("GRANT DELETE ON audit_events TO otto_app")
        .execute(&pool)
        .await
        .unwrap();
    let restricted = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE otto_app").execute(conn).await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let rdb = otto_tenant::Db::from_pool(restricted);

    // Old event markers in two orgs; the org-less sweep must reach both.
    for t in [&a, &b] {
        apply(
            &rdb,
            &event(
                "team.deleted",
                serde_json::json!({ "org_id": t.org.as_uuid(), "team_id": TeamId::new().as_uuid() }),
            ),
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE platform_events SET received_at = now() - interval '40 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(of_core::platform_events::sweep(&rdb, 30).await.unwrap(), 2);
    assert_eq!(count(&db, a.org, "platform_events").await, 0);
    assert_eq!(count(&db, b.org, "platform_events").await, 0);

    // A pinned transaction still sees only its own org's markers.
    apply(
        &rdb,
        &event(
            "team.deleted",
            serde_json::json!({ "org_id": b.org.as_uuid(), "team_id": TeamId::new().as_uuid() }),
        ),
    )
    .await
    .unwrap();
    let mut tx = rdb.begin(a.org).await.unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM platform_events")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen, 0, "org A saw org B's event markers");

    // org.deleted purges the org's audit trail, and only that org's.
    for t in [&a, &b] {
        let mut tx = rdb.begin(t.org).await.unwrap();
        tx.audit(otto_tenant::audit::Entry::new("repo.registered"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let theirs = count(&db, b.org, "audit_events").await;
    assert!(count(&db, a.org, "audit_events").await > 0);
    apply(
        &rdb,
        &event(
            "org.deleted",
            serde_json::json!({ "org_id": a.org.as_uuid() }),
        ),
    )
    .await
    .unwrap();
    assert_eq!(count(&db, a.org, "audit_events").await, 0);
    assert_eq!(count(&db, b.org, "audit_events").await, theirs);

    rdb.pool().close().await;
}

#[sqlx::test]
async fn team_deleted_keeps_the_scope_and_never_widens_it(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let team = TeamId::new();
    platform.add_org(a.org.as_uuid(), "acme", "Acme");
    platform.add_team(a.org.as_uuid(), team.as_uuid(), "platform", "Platform");

    let verified = VerifiedTeam::verify(&platform.client(), a.org, team)
        .await
        .unwrap();
    let mut tx = db.begin(a.org).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: "scoped".into(),
            remotes: vec!["git@github.com:acme/scoped.git".into()],
            team_id: Some(verified),
            ..Default::default()
        })
        .await
        .unwrap();
    let j = tx
        .add_job(of_core::jobs::NewJob {
            repo_id: repo.id,
            title: "scoped work".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(j.team_id, Some(team), "a job inherits its repo's team");
    tx.commit().await.unwrap();

    let ev = event(
        "team.deleted",
        serde_json::json!({ "org_id": a.org.as_uuid(), "team_id": team.as_uuid() }),
    );
    let Outcome::Applied { detail } = apply(&db, &ev).await.unwrap() else {
        panic!("expected the event to apply");
    };
    assert_eq!(detail["repos"], 1);
    assert_eq!(detail["jobs"], 1);

    // Nothing was nulled: a null team_id means org-wide.
    let mut tx = db.begin(a.org).await.unwrap();
    let repo = tx.get_repo(repo.id).await.unwrap().unwrap();
    let job = tx.get_job(&j.id).await.unwrap();
    let audit = tx.audit_trail(Some("platform."), 10).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        repo.team_id,
        Some(team),
        "team.deleted widened a repo to org-wide"
    );
    assert_eq!(
        job.team_id,
        Some(team),
        "team.deleted widened a job to org-wide"
    );
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "platform.team.deleted");

    // And the dead id cannot be attached to anything new: the platform no longer
    // knows it.
    platform.remove_team(a.org.as_uuid(), team.as_uuid());
    let err = VerifiedTeam::verify(&platform.client(), a.org, team).await;
    assert!(
        matches!(err, Err(of_core::Error::TeamNotFound { .. })),
        "{err:?}"
    );

    assert_eq!(apply(&db, &ev).await.unwrap(), Outcome::Duplicate);
}

#[sqlx::test]
async fn member_removed_releases_what_they_held(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = Member::new().id;

    let mut tx = db.begin(a.org).await.unwrap();
    let held = tx.add_job(job(&a, "bobs")).await.unwrap();
    let mine = tx.add_job(job(&a, "mine")).await.unwrap();
    let done = tx.add_job(job(&a, "done")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&held.id), bob, Some("bob-agent"), None)
        .await
        .unwrap();
    tx.claim_jobs(std::slice::from_ref(&mine.id), a.user, None, None)
        .await
        .unwrap();
    tx.claim_jobs(std::slice::from_ref(&done.id), bob, None, None)
        .await
        .unwrap();
    tx.complete_job(&done.id, bob, Some("ok"), None)
        .await
        .unwrap();
    tx.acquire_lease(a.repo, "branch:bob", bob, Some("bob-agent"), None, None)
        .await
        .unwrap();
    tx.acquire_lease(a.repo, "branch:mine", a.user, None, None, None)
        .await
        .unwrap();
    let was_here = tx
        .send_message(
            bob,
            NewMessage {
                body: "bob was here".into(),
                repo_id: Some(a.repo),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tx.ack_messages_for(bob, ChannelRef::Repo(a.repo), was_here.id, &TeamScope::All)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let ev = event(
        "member.removed",
        serde_json::json!({ "org_id": a.org.as_uuid(), "user_id": bob.as_uuid() }),
    );
    let Outcome::Applied { detail } = apply(&db, &ev).await.unwrap() else {
        panic!("expected the event to apply");
    };
    assert_eq!(detail["leases_released"], 1);
    assert_eq!(detail["claims_released"], 1);
    assert_eq!(detail["cursors_dropped"], 1);

    let mut tx = db.begin(a.org).await.unwrap();
    let held = tx.get_job(&held.id).await.unwrap();
    let mine = tx.get_job(&mine.id).await.unwrap();
    let done = tx.get_job(&done.id).await.unwrap();
    let leases = tx.list_leases(None).await.unwrap();
    let messages = tx
        .inbox(
            a.user,
            &of_core::messages::InboxQuery {
                channel: ChannelRef::Repo(a.repo),
                unread_only: true,
                limit: 50,
                newest_first: false,
                visible_teams: None,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        held.status,
        Status::Pending,
        "bob's in-flight job was not released"
    );
    assert_eq!(held.claimed_by, None);
    assert_eq!(
        mine.status,
        Status::InProgress,
        "someone else's claim was touched"
    );
    assert_eq!(
        done.status,
        Status::Completed,
        "finished history was rewritten"
    );
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].resource, "branch:mine");
    assert_eq!(messages.len(), 1, "what bob wrote is history and stays");

    assert_eq!(apply(&db, &ev).await.unwrap(), Outcome::Duplicate);
}

/// A member has one cursor per channel they have read; removal drops them all,
/// and only in the org they left.
#[sqlx::test]
async fn member_removed_drops_every_channel_cursor(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let (ours, theirs) = teams_in_two_orgs(&platform, &a, &b).await;
    let bob = Member::new().id;

    // The same person in both orgs, reading a repo channel and a team channel
    // in each.
    for (t, team) in [(&a, ours), (&b, theirs)] {
        let mut tx = db.begin(t.org).await.unwrap();
        for (channel, repo_id, team_id) in [
            (ChannelRef::Repo(t.repo), Some(t.repo), None),
            (ChannelRef::Team(team.id()), None, Some(team)),
        ] {
            let id = tx
                .send_message(
                    t.user,
                    NewMessage {
                        body: "read me".into(),
                        repo_id,
                        team_id,
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .id;
            assert_eq!(
                tx.ack_messages_for(bob, channel, id, &TeamScope::All)
                    .await
                    .unwrap(),
                id
            );
        }
        tx.commit().await.unwrap();
    }
    assert_eq!(count(&db, a.org, "message_cursors").await, 2);

    let ev = event(
        "member.removed",
        serde_json::json!({ "org_id": a.org.as_uuid(), "user_id": bob.as_uuid() }),
    );
    let Outcome::Applied { detail } = apply(&db, &ev).await.unwrap() else {
        panic!("expected the event to apply");
    };
    assert_eq!(detail["cursors_dropped"], 2);
    assert_eq!(count(&db, a.org, "message_cursors").await, 0);
    assert_eq!(
        count(&db, b.org, "message_cursors").await,
        2,
        "removal in one org dropped cursors in another"
    );
}

#[sqlx::test]
async fn an_event_for_one_org_cannot_touch_another(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    // The same user id in both orgs (a person in two companies).
    let mut tx = db.begin(b.org).await.unwrap();
    tx.acquire_lease(b.repo, "branch:main", a.user, None, None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let ev = event(
        "member.removed",
        serde_json::json!({ "org_id": a.org.as_uuid(), "user_id": a.user.as_uuid() }),
    );
    apply(&db, &ev).await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    assert_eq!(
        tx.list_leases(None).await.unwrap().len(),
        1,
        "removal in one org released a lease in another"
    );
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn an_unknown_event_type_is_acknowledged_not_failed(pool: PgPool) {
    let db = db(pool);
    let ev = event("plan.changed", serde_json::json!({ "anything": true }));
    assert_eq!(apply(&db, &ev).await.unwrap(), Outcome::Ignored);
}

// ------------------------------------------------------------- VerifiedTeam

#[tokio::test]
async fn an_unknown_team_fails_closed() {
    let platform = MockPlatform::start().await;
    let org = OrgId::new();
    platform.add_org(org.as_uuid(), "acme", "Acme");

    let res = VerifiedTeam::verify(&platform.client(), org, TeamId::new()).await;
    assert!(
        matches!(res, Err(of_core::Error::TeamNotFound { .. })),
        "{res:?}"
    );
}

#[tokio::test]
async fn a_team_from_another_org_is_not_found() {
    let platform = MockPlatform::start().await;
    let (mine, theirs) = (OrgId::new(), OrgId::new());
    let team = TeamId::new();
    platform.add_org(mine.as_uuid(), "mine", "Mine");
    platform.add_org(theirs.as_uuid(), "theirs", "Theirs");
    platform.add_team(theirs.as_uuid(), team.as_uuid(), "secret", "Secret");

    let res = VerifiedTeam::verify(&platform.client(), mine, team).await;
    assert!(
        matches!(res, Err(of_core::Error::TeamNotFound { .. })),
        "{res:?}"
    );
}

#[tokio::test]
async fn an_unreachable_platform_refuses_instead_of_assuming() {
    let platform = MockPlatform::start().await;
    let org = OrgId::new();
    let team = TeamId::new();
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.add_team(org.as_uuid(), team.as_uuid(), "platform", "Platform");
    platform.set_down(true);

    let res = VerifiedTeam::verify(&platform.client(), org, team).await;
    let Err(e) = res else {
        panic!("a team was verified while the platform was down")
    };
    assert!(matches!(e, of_core::Error::Platform(_)), "{e:?}");
    assert!(e.retriable(), "an outage is worth retrying");
}

#[tokio::test]
async fn by_slug_finds_a_team_in_its_org() {
    let platform = MockPlatform::start().await;
    let org = OrgId::new();
    let team = TeamId::new();
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.add_team(org.as_uuid(), team.as_uuid(), "platform", "Platform");

    let verified = VerifiedTeam::by_slug(&platform.client(), org, "platform")
        .await
        .expect("a team the org has");
    assert_eq!(verified.id(), team);
    assert_eq!(verified.org(), org);
}

/// The platform scopes the lookup by org, so this proves that scoping holds
/// end to end. `by_slug`'s own re-check of the answer's org is defense against a
/// confused response, which the mock cannot produce (the same as for `verify`).
#[tokio::test]
async fn by_slug_refuses_another_orgs_slug() {
    let platform = MockPlatform::start().await;
    let (mine, theirs) = (OrgId::new(), OrgId::new());
    platform.add_org(mine.as_uuid(), "mine", "Mine");
    platform.add_org(theirs.as_uuid(), "theirs", "Theirs");
    platform.add_team(theirs.as_uuid(), Uuid::new_v4(), "secret", "Secret");

    let res = VerifiedTeam::by_slug(&platform.client(), mine, "secret").await;
    assert!(
        matches!(res, Err(of_core::Error::TeamNotFound { .. })),
        "{res:?}"
    );
}

#[tokio::test]
async fn by_slug_refuses_an_unknown_slug() {
    let platform = MockPlatform::start().await;
    let org = OrgId::new();
    platform.add_org(org.as_uuid(), "acme", "Acme");

    let res = VerifiedTeam::by_slug(&platform.client(), org, "nope").await;
    assert!(
        matches!(res, Err(of_core::Error::TeamNotFound { .. })),
        "{res:?}"
    );
}

/// A slug that could not name a team is refused before any request: with the
/// platform down, a request would have come back as `Error::Platform`.
#[tokio::test]
async fn by_slug_refuses_a_malformed_slug_without_asking_the_platform() {
    let platform = MockPlatform::start().await;
    let org = OrgId::new();
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.set_down(true);

    let long = "a".repeat(of_core::teams::TEAM_SLUG_MAX + 1);
    for slug in [
        "", "  ", ".", "..", "a/b", "a\\b", "a%2Fb", "a?b", "a#b", "a b", "a\tb", &long,
    ] {
        let res = VerifiedTeam::by_slug(&platform.client(), org, slug).await;
        assert!(
            matches!(res, Err(of_core::Error::TeamNotFound { .. })),
            "{slug:?}: {res:?}"
        );
    }
}

#[tokio::test]
async fn by_slug_on_an_unreachable_platform_is_a_platform_error() {
    let platform = MockPlatform::start().await;
    let org = OrgId::new();
    platform.add_org(org.as_uuid(), "acme", "Acme");
    platform.add_team(org.as_uuid(), Uuid::new_v4(), "platform", "Platform");
    platform.set_down(true);

    let res = VerifiedTeam::by_slug(&platform.client(), org, "platform").await;
    let Err(e) = res else {
        panic!("a team was verified while the platform was down")
    };
    assert!(matches!(e, of_core::Error::Platform(_)), "{e:?}");
    assert!(
        e.retriable(),
        "an outage is worth retrying, not a missing team"
    );
}

#[sqlx::test]
async fn a_team_verified_for_another_org_cannot_be_attached_here(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let team = TeamId::new();
    platform.add_org(b.org.as_uuid(), "globex", "Globex");
    platform.add_team(b.org.as_uuid(), team.as_uuid(), "theirs", "Theirs");
    let theirs = VerifiedTeam::verify(&platform.client(), b.org, team)
        .await
        .unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    let res = tx
        .register_repo(NewRepo {
            slug: "sneaky".into(),
            remotes: vec!["git@github.com:acme/sneaky.git".into()],
            team_id: Some(theirs),
            ..Default::default()
        })
        .await;
    let _ = tx.rollback().await;
    assert!(
        matches!(res, Err(of_core::Error::TeamNotFound { .. })),
        "{res:?}"
    );
}

/// The three other places a `VerifiedTeam` is written: each must accept a team
/// verified for this org and refuse one verified for another, writing nothing.
async fn teams_in_two_orgs(
    platform: &MockPlatform,
    a: &common::Tenant,
    b: &common::Tenant,
) -> (VerifiedTeam, VerifiedTeam) {
    let (ours, theirs) = (TeamId::new(), TeamId::new());
    platform.add_org(a.org.as_uuid(), "acme", "Acme");
    platform.add_org(b.org.as_uuid(), "globex", "Globex");
    platform.add_team(a.org.as_uuid(), ours.as_uuid(), "ours", "Ours");
    platform.add_team(b.org.as_uuid(), theirs.as_uuid(), "theirs", "Theirs");
    let client = platform.client();
    (
        VerifiedTeam::verify(&client, a.org, ours).await.unwrap(),
        VerifiedTeam::verify(&client, b.org, theirs).await.unwrap(),
    )
}

#[sqlx::test]
async fn a_job_takes_a_team_from_its_own_org_only(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let (ours, theirs) = teams_in_two_orgs(&platform, &a, &b).await;

    let mut tx = db.begin(a.org).await.unwrap();
    let mut new = job(&a, "ours");
    new.team_id = Some(ours);
    let created = tx.add_job(new).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(created.team_id, Some(ours.id()));

    let mut tx = db.begin(a.org).await.unwrap();
    let mut new = job(&a, "theirs");
    new.team_id = Some(theirs);
    let res = tx.add_job(new).await;
    let _ = tx.rollback().await;
    assert!(
        matches!(res, Err(of_core::Error::TeamNotFound { .. })),
        "{res:?}"
    );
    assert_eq!(
        count(&db, a.org, "jobs").await,
        1,
        "the refused job was inserted"
    );
}

#[sqlx::test]
async fn a_message_takes_a_team_from_its_own_org_only_keyed_or_not(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let (ours, theirs) = teams_in_two_orgs(&platform, &a, &b).await;

    // Keyed and unkeyed sends insert on separate paths; both are checked.
    for key in [None, Some("send-1".to_string())] {
        let mut tx = db.begin(a.org).await.unwrap();
        let sent = tx
            .send_message(
                a.user,
                NewMessage {
                    body: "to our team".into(),
                    team_id: Some(ours),
                    idempotency_key: key.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(sent.team_id, Some(ours.id()), "key {key:?}");

        let mut tx = db.begin(a.org).await.unwrap();
        let res = tx
            .send_message(
                a.user,
                NewMessage {
                    body: "to their team".into(),
                    team_id: Some(theirs),
                    idempotency_key: key.map(|k| format!("{k}-theirs")),
                    ..Default::default()
                },
            )
            .await;
        let _ = tx.rollback().await;
        assert!(
            matches!(res, Err(of_core::Error::TeamNotFound { .. })),
            "{res:?}"
        );
    }
    assert_eq!(
        count(&db, a.org, "messages").await,
        2,
        "a message addressed to another org's team was inserted"
    );
}

#[sqlx::test]
async fn a_repo_is_reassigned_to_a_team_from_its_own_org_only(pool: PgPool) {
    let platform = MockPlatform::start().await;
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let (ours, theirs) = teams_in_two_orgs(&platform, &a, &b).await;

    let mut tx = db.begin(a.org).await.unwrap();
    let repo = tx
        .update_repo(
            a.repo,
            RepoPatch {
                team_id: Some(Some(ours)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(repo.team_id, Some(ours.id()));

    let mut tx = db.begin(a.org).await.unwrap();
    let res = tx
        .update_repo(
            a.repo,
            RepoPatch {
                team_id: Some(Some(theirs)),
                ..Default::default()
            },
        )
        .await;
    let _ = tx.rollback().await;
    assert!(
        matches!(res, Err(of_core::Error::TeamNotFound { .. })),
        "{res:?}"
    );

    let mut tx = db.begin(a.org).await.unwrap();
    let after = tx.get_repo(a.repo).await.unwrap().unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        after.team_id,
        Some(ours.id()),
        "a refused reassignment changed the repo's team"
    );
}
