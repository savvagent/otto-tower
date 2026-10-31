//! Console login sessions: the table, its isolation, and its lifecycle.
//!
//! `console_sessions` is a tenant table whose policy lets an *unpinned* statement
//! see every org's rows (the cookie lookup has to learn the org). That makes the
//! pinned side the one that needs proving: a pinned transaction must see, lock,
//! rotate, and delete only its own org's sessions.

mod common;

use chrono::{Duration, Utc};
use common::db;
use savvagent_tenant::crypto::Cipher;
use savvagent_tenant::ids::{OrgId, UserId};
use savvagent_tenant::Db;
use sqlx::PgPool;
use tower_core::console_sessions::{self as sessions, NewSession};

fn cipher() -> Cipher {
    use base64::Engine;
    Cipher::from_base64_key(&base64::engine::general_purpose::STANDARD.encode([3u8; 32])).unwrap()
}

async fn session(db: &Db, cipher: &Cipher, org: OrgId, user: UserId, cookie: &str) -> [u8; 32] {
    let id_hash = sessions::hash_cookie(cookie);
    sessions::create(
        db,
        cipher,
        &NewSession {
            id_hash,
            user_id: user,
            org_id: org,
            access_token: "sav_at_access",
            refresh_token: "sav_rt_refresh",
            access_expires_at: Utc::now() + Duration::hours(1),
            scopes: &["jobs:read".to_string()],
            expires_at: Utc::now() + Duration::days(30),
        },
    )
    .await
    .expect("create");
    id_hash
}

#[sqlx::test]
async fn tokens_are_sealed_at_rest_and_the_cookie_is_never_stored(pool: PgPool) {
    let db = db(pool.clone());
    let cipher = cipher();
    let (org, user) = (OrgId::new(), UserId::new());
    let id_hash = session(&db, &cipher, org, user, "the-cookie-value").await;

    let (access, refresh, key): (String, String, Vec<u8>) =
        sqlx::query_as("SELECT access_token_enc, refresh_token_enc, id_hash FROM console_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!access.contains("sav_at_") && !refresh.contains("sav_rt_"));
    assert_eq!(key, id_hash.to_vec());
    assert_ne!(key, b"the-cookie-value".to_vec());

    let found = sessions::find(&db, &id_hash).await.unwrap().expect("found");
    assert_eq!(found.org_id, org);
    assert_eq!(found.access_token(&cipher).unwrap(), "sav_at_access");
    assert_eq!(found.refresh_token(&cipher).unwrap(), "sav_rt_refresh");

    // A different key cannot open them.
    use base64::Engine;
    let other =
        Cipher::from_base64_key(&base64::engine::general_purpose::STANDARD.encode([4u8; 32]))
            .unwrap();
    assert!(found.access_token(&other).is_err());
}

#[sqlx::test]
async fn an_expired_session_is_not_found_and_is_purged(pool: PgPool) {
    let db = db(pool.clone());
    let cipher = cipher();
    let (org, user) = (OrgId::new(), UserId::new());
    let id_hash = session(&db, &cipher, org, user, "c1").await;
    sqlx::query("UPDATE console_sessions SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();

    assert!(sessions::find(&db, &id_hash).await.unwrap().is_none());
    assert!(sessions::lock(&db, org, &id_hash).await.unwrap().is_none());
    assert_eq!(sessions::purge_expired(&db).await.unwrap(), 1);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM console_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn another_orgs_session_cannot_be_locked_rotated_or_deleted(pool: PgPool) {
    let db = db(pool.clone());
    let cipher = cipher();
    let (a, b) = (OrgId::new(), OrgId::new());
    let id_hash = session(&db, &cipher, a, UserId::new(), "cookie-a").await;

    // Org B, holding A's cookie hash, gets nothing.
    assert!(sessions::lock(&db, b, &id_hash).await.unwrap().is_none());
    assert_eq!(sessions::delete(&db, b, &id_hash).await.unwrap(), 0);
    let mut tx = db.begin(b).await.unwrap();
    assert_eq!(
        sessions::delete_for_user(&mut tx, UserId::new())
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();

    let still = sessions::find(&db, &id_hash)
        .await
        .unwrap()
        .expect("intact");
    assert_eq!(still.org_id, a);
    assert_eq!(still.access_token(&cipher).unwrap(), "sav_at_access");
}

/// Deliberately unscoped SQL in a pinned transaction: RLS, not the predicate,
/// is what has to hold. The test must `SET LOCAL ROLE otto_app` explicitly,
/// because `#[sqlx::test]` connects as a superuser and bypasses RLS.
#[sqlx::test]
async fn rls_scopes_console_sessions_to_the_pinned_org(pool: PgPool) {
    let db = db(pool);
    let cipher = cipher();
    let (a, b) = (OrgId::new(), OrgId::new());
    session(&db, &cipher, a, UserId::new(), "cookie-a").await;
    session(&db, &cipher, b, UserId::new(), "cookie-b").await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(b.to_string())
        .execute(tx.conn())
        .await
        .unwrap();

    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM console_sessions")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    let updated = sqlx::query("UPDATE console_sessions SET expires_at = now()")
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    let deleted = sqlx::query("DELETE FROM console_sessions WHERE org_id = $1")
        .bind(a)
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    let forged = sqlx::query(
        "INSERT INTO console_sessions (id_hash, user_id, org_id, access_token_enc, \
         refresh_token_enc, access_expires_at, expires_at) \
         VALUES (decode(repeat('ab', 32), 'hex'), gen_random_uuid(), $1, 'x', 'x', now(), now())",
    )
    .bind(a)
    .execute(tx.conn())
    .await;
    tx.rollback().await.unwrap();

    assert_eq!(seen, 1, "org B saw org A's session");
    assert_eq!(updated, 1, "org B rewrote more than its own session");
    assert_eq!(deleted, 0, "org B deleted org A's session");
    assert!(forged.is_err(), "org B planted a session in org A");
}

#[sqlx::test]
async fn locking_serializes_a_rotation_and_the_loser_sees_the_new_tokens(pool: PgPool) {
    let db = db(pool);
    let cipher = cipher();
    let org = OrgId::new();
    let id_hash = session(&db, &cipher, org, UserId::new(), "cookie").await;

    let (mut tx, first) = sessions::lock(&db, org, &id_hash).await.unwrap().unwrap();
    assert_eq!(first.refresh_token(&cipher).unwrap(), "sav_rt_refresh");

    // A second locker blocks until the first commits.
    let waiter = {
        let db = db.clone();
        tokio::spawn(async move { sessions::lock(&db, org, &id_hash).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!waiter.is_finished(), "the second lock did not wait");

    sessions::rotate(
        &mut tx,
        &cipher,
        &id_hash,
        "sav_at_new",
        "sav_rt_new",
        Utc::now() + Duration::hours(1),
        Utc::now() + Duration::days(30),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let (tx2, second) = waiter.await.unwrap().unwrap().unwrap();
    assert_eq!(second.access_token(&cipher).unwrap(), "sav_at_new");
    assert_eq!(second.refresh_token(&cipher).unwrap(), "sav_rt_new");
    tx2.rollback().await.unwrap();
}

/// A sign-in that completes after the platform deleted the org, or removed the
/// member, must not leave a session behind the clean-up that ran for it.
#[sqlx::test]
async fn no_session_is_created_for_a_deleted_org_or_a_removed_member(pool: PgPool) {
    use savvagent_resource::webhook;
    use tower_core::platform_events::apply;
    use tower_testkit::MockPlatform;

    let db = db(pool);
    let cipher = cipher();
    let deliver = |kind: &str, data: serde_json::Value| {
        let (header, body) = MockPlatform::webhook(kind, data);
        webhook::verify(tower_testkit::WEBHOOK_SECRET, &header, &body).unwrap()
    };
    let new = |org: OrgId, user: UserId| NewSession {
        id_hash: sessions::hash_cookie(&format!("{org}{user}")),
        user_id: user,
        org_id: org,
        access_token: "sav_at_access",
        refresh_token: "sav_rt_refresh",
        access_expires_at: Utc::now() + Duration::hours(1),
        scopes: &[],
        expires_at: Utc::now() + Duration::days(30),
    };

    let (gone, user) = (OrgId::new(), UserId::new());
    apply(
        &db,
        &deliver(
            "org.deleted",
            serde_json::json!({ "org_id": gone.as_uuid() }),
        ),
    )
    .await
    .unwrap();
    let res = sessions::create(&db, &cipher, &new(gone, user)).await;
    assert!(
        matches!(res, Err(tower_core::Error::AccessRevoked)),
        "{res:?}"
    );

    let (org, removed, staying) = (OrgId::new(), UserId::new(), UserId::new());
    apply(
        &db,
        &deliver(
            "member.removed",
            serde_json::json!({ "org_id": org.as_uuid(), "user_id": removed.as_uuid() }),
        ),
    )
    .await
    .unwrap();
    let res = sessions::create(&db, &cipher, &new(org, removed)).await;
    assert!(
        matches!(res, Err(tower_core::Error::AccessRevoked)),
        "{res:?}"
    );
    sessions::create(&db, &cipher, &new(org, staying))
        .await
        .expect("another member of the org still signs in");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM console_sessions")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
}
