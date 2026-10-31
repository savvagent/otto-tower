//! Tracker connection and binding CRUD.

mod common;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use common::{db, tenant};
use savvagent_tenant::crypto::Cipher;
use sqlx::PgPool;
use tower_core::trackers::{
    delete_binding, delete_connection, find_binding_by_external_ref, get_binding, get_connection,
    list_bindings_for_repo, list_connections, resolve_binding, resolve_connection_org,
    upsert_binding, upsert_connection, Provider,
};

#[sqlx::test]
async fn tracker_connections_and_bindings_round_trip(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let cipher = Cipher::from_base64_key(&B64.encode([7u8; 32])).unwrap();
    let credentials = cipher.seal(b"jira-refresh-token").unwrap();
    let webhook = cipher.seal(b"shared-secret").unwrap();

    let mut tx = db.begin(t.org).await.unwrap();

    let github = upsert_connection(&mut tx, Provider::Github, "installation-1", None, None)
        .await
        .unwrap();
    assert_eq!(github.external_id, "installation-1");
    assert!(github.encrypted_credentials.is_none());
    assert!(github.encrypted_webhook_secret.is_none());

    let jira = upsert_connection(
        &mut tx,
        Provider::Jira,
        "site-1",
        Some(&credentials),
        Some(&webhook),
    )
    .await
    .unwrap();
    assert_eq!(
        jira.encrypted_credentials.as_deref(),
        Some(
            B64.encode([credentials.nonce.clone(), credentials.ciphertext.clone()].concat())
                .as_str()
        )
    );
    assert_eq!(
        jira.encrypted_webhook_secret.as_deref(),
        Some(
            B64.encode([webhook.nonce.clone(), webhook.ciphertext.clone()].concat())
                .as_str()
        )
    );

    let fetched = get_connection(&mut tx, Provider::Jira)
        .await
        .unwrap()
        .expect("jira connection");
    assert_eq!(fetched, jira);

    let rebound = upsert_connection(
        &mut tx,
        Provider::Jira,
        "site-2",
        Some(&cipher.seal(b"rotated-refresh-token").unwrap()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(rebound.id, jira.id);
    assert_eq!(rebound.external_id, "site-2");
    assert!(rebound.encrypted_credentials.is_some());
    assert!(rebound.encrypted_webhook_secret.is_none());
    tx.commit().await.unwrap();

    assert_eq!(
        resolve_connection_org(&db, Provider::Jira, "site-1")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        resolve_connection_org(&db, Provider::Jira, "site-2")
            .await
            .unwrap(),
        Some(t.org)
    );

    let mut tx = db.begin(t.org).await.unwrap();

    let binding = upsert_binding(
        &mut tx,
        t.repo,
        Some(github.id),
        Provider::Github,
        "acme/api",
        "otto-factory",
    )
    .await
    .unwrap();
    assert_eq!(binding.connection_id, Some(github.id));

    let fetched = get_binding(&mut tx, binding.id)
        .await
        .unwrap()
        .expect("github binding");
    assert_eq!(fetched, binding);

    let resolved = resolve_binding(&mut tx, t.repo, Provider::Github)
        .await
        .unwrap()
        .expect("resolved github binding");
    assert_eq!(resolved, binding);
    let by_external_ref = find_binding_by_external_ref(&mut tx, Provider::Github, "acme/api")
        .await
        .unwrap()
        .expect("binding by external ref");
    assert_eq!(by_external_ref, binding);

    delete_connection(&mut tx, Provider::Github).await.unwrap();
    assert!(get_connection(&mut tx, Provider::Github)
        .await
        .unwrap()
        .is_none());

    let orphaned = get_binding(&mut tx, binding.id)
        .await
        .unwrap()
        .expect("binding survives");
    assert_eq!(orphaned.connection_id, None);

    delete_binding(&mut tx, binding.id).await.unwrap();
    assert!(get_binding(&mut tx, binding.id).await.unwrap().is_none());
    assert!(resolve_binding(&mut tx, t.repo, Provider::Github)
        .await
        .unwrap()
        .is_none());

    tx.commit().await.unwrap();
    assert_eq!(
        resolve_connection_org(&db, Provider::Github, "installation-1")
            .await
            .unwrap(),
        None
    );
}

/// Guard 1 (the API shape), not guard 2 (RLS): a binding must not silently
/// point at a connection of the wrong provider, even within the caller's own
/// org, because the sync engine trusts `binding.provider` to pick the client
/// it dials. This is exercised at the `require_connection_in_org` level
/// directly, independent of row-level security, per the same-org same-caller
/// path an RLS test cannot cover.
#[sqlx::test]
async fn binding_rejects_a_connection_of_the_wrong_provider(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let jira = upsert_connection(&mut tx, Provider::Jira, "site-1", None, None)
        .await
        .unwrap();

    let err = upsert_binding(
        &mut tx,
        t.repo,
        Some(jira.id),
        Provider::Github,
        "acme/api",
        "otto-factory",
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("not found in this org"),
        "unexpected error: {err}"
    );
}

/// A binding must not accept a connection id that belongs to another org, even
/// though RLS already makes such a row invisible to the query inside
/// `require_connection_in_org`. This asserts the function's own contract
/// (Invalid, not silently succeeding or panicking) rather than relying on the
/// RLS test suite to be the only thing standing between orgs.
#[sqlx::test]
async fn binding_rejects_a_connection_from_another_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(b.org).await.unwrap();
    let globex_connection =
        upsert_connection(&mut tx, Provider::Github, "installation-9", None, None)
            .await
            .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    let err = upsert_binding(
        &mut tx,
        a.repo,
        Some(globex_connection.id),
        Provider::Github,
        "acme/api",
        "otto-factory",
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("not found in this org"),
        "unexpected error: {err}"
    );
}

/// A binding must not accept a repo id that belongs to another org. `get_repo`
/// is scoped by the pinned transaction's own org, so this is really a test of
/// `require_repo_in_org`'s contract rather than a fresh isolation guarantee,
/// but the contract deserves its own coverage independent of the RLS suite.
#[sqlx::test]
async fn binding_rejects_a_repo_from_another_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let err = upsert_binding(
        &mut tx,
        b.repo,
        None,
        Provider::Github,
        "acme/api",
        "otto-factory",
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("repo not found"),
        "unexpected error: {err}"
    );
}

#[sqlx::test]
async fn trigger_label_round_trips_through_bindings(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let binding = upsert_binding(
        &mut tx,
        t.repo,
        None,
        Provider::Github,
        "acme/api",
        "ship-it",
    )
    .await
    .unwrap();
    let fetched = get_binding(&mut tx, binding.id).await.unwrap().unwrap();
    tx.commit().await.unwrap();

    assert_eq!(fetched.trigger_label, "ship-it");
}

/// The schema default is what a binding gets when nothing names a label
/// (savvagent/otto-factory#233). Migrations never rewrite stored rows, so this
/// reads the column's default, not any existing binding.
#[sqlx::test]
async fn a_new_binding_defaults_to_the_otto_tower_label(pool: PgPool) {
    let default: String = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = 'tracker_bindings' AND column_name = 'trigger_label'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(default.contains("'otto-tower'"), "{default}");
}

/// This is deliberately not an `rls_scopes_*` test: `tracker_connection_index`
/// is outside RLS by design so a webhook can resolve which org owns a provider
/// id before any `app.org_id` exists. What matters here is guard 1 — the only
/// writers are pinned `Tx` methods, and the reverse lookup returns each org's
/// own id rather than crossing tenants.
#[sqlx::test]
async fn resolve_connection_org_is_scoped_by_the_index_rows_written_from_each_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    upsert_connection(&mut tx, Provider::Github, "installation-a", None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    upsert_connection(&mut tx, Provider::Github, "installation-b", None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        resolve_connection_org(&db, Provider::Github, "installation-a")
            .await
            .unwrap(),
        Some(a.org)
    );
    assert_eq!(
        resolve_connection_org(&db, Provider::Github, "installation-b")
            .await
            .unwrap(),
        Some(b.org)
    );
    assert_ne!(
        resolve_connection_org(&db, Provider::Github, "installation-a")
            .await
            .unwrap(),
        Some(b.org)
    );
}

/// The console reads both tables as lists — one card per provider on the org
/// page, one binding row per provider in a repo's expander. Both listings are
/// pinned to the transaction's org, which is what keeps the console's tracker
/// page from becoming a second surface onto another tenant's rows.
#[sqlx::test]
async fn listing_connections_and_bindings_is_scoped_to_the_transactions_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(b.org).await.unwrap();
    upsert_connection(&mut tx, Provider::Github, "installation-99", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        b.repo,
        None,
        Provider::Github,
        "globex/api",
        "otto-factory",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    assert!(
        list_connections(&mut tx).await.unwrap().is_empty(),
        "another org's connection is visible in this org's listing"
    );

    let github = upsert_connection(&mut tx, Provider::Github, "installation-1", None, None)
        .await
        .unwrap();
    upsert_connection(&mut tx, Provider::Jira, "site-1", None, None)
        .await
        .unwrap();

    // Ordered by provider so the console renders the same two cards in the same
    // order on every load, rather than in whatever order Postgres returns rows.
    let connections = list_connections(&mut tx).await.unwrap();
    assert_eq!(connections.len(), 2);
    assert_eq!(connections[0].provider, Provider::Github);
    assert_eq!(connections[1].provider, Provider::Jira);

    assert!(
        list_bindings_for_repo(&mut tx, a.repo)
            .await
            .unwrap()
            .is_empty(),
        "another org's binding is visible against this org's repo"
    );

    upsert_binding(
        &mut tx,
        a.repo,
        Some(github.id),
        Provider::Github,
        "acme/api",
        "otto-factory",
    )
    .await
    .unwrap();

    let bindings = list_bindings_for_repo(&mut tx, a.repo).await.unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].external_ref, "acme/api");
    assert_eq!(bindings[0].connection_id, Some(github.id));
}

/// A repo id from another org lists nothing rather than that repo's bindings.
/// `list_bindings_for_repo` binds `org_id` as well as `repo_id` for exactly
/// this reason: a `repo_id` is a uuid a console caller could otherwise guess
/// their way into, and the binding names the customer's JIRA project.
#[sqlx::test]
async fn listing_bindings_for_another_orgs_repo_returns_nothing(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(b.org).await.unwrap();
    upsert_binding(
        &mut tx,
        b.repo,
        None,
        Provider::Jira,
        "GLOBEX",
        "otto-factory",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    assert!(list_bindings_for_repo(&mut tx, b.repo)
        .await
        .unwrap()
        .is_empty());
}
