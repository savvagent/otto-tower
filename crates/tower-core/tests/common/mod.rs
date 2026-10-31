//! Shared fixtures. Every integration test needs at least one org with a user
//! and a repo, and the isolation tests need two orgs that must never see each
//! other.

#![allow(dead_code)]

use savvagent_tenant::ids::{OrgId, UserId};
use savvagent_tenant::Db;
use sqlx::PgPool;
use tower_core::ids::RepoId;
use tower_core::jobs::NewJob;
use tower_core::repos::NewRepo;
use tower_core::repos::ReposExt;

pub struct Tenant {
    pub org: OrgId,
    pub user: UserId,
    pub repo: RepoId,
}

/// An org with one user and one registered repo.
///
/// Orgs and users are the platform's, not this database's: an `OrgId` is just a
/// uuid the platform would have issued, so a fixture mints one. Nothing here
/// needs a row to exist first because nothing in this schema references one.
pub async fn tenant(db: &Db, slug: &str, remote: &str) -> Tenant {
    let org = OrgId::new();
    let user = UserId::new();

    let mut tx = db.begin(org).await.expect("begin");
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some(format!("{slug} api")),
            remotes: vec![remote.into()],
            created_by: Some(user),
            ..Default::default()
        })
        .await
        .expect("register repo");
    tx.commit().await.expect("commit");

    Tenant {
        org,
        user,
        repo: repo.id,
    }
}

pub fn db(pool: PgPool) -> Db {
    Db::from_pool(pool)
}

/// A minimal job in this tenant's repo.
pub fn job(t: &Tenant, title: &str) -> NewJob {
    NewJob {
        repo_id: t.repo,
        title: title.into(),
        created_by: Some(t.user),
        ..Default::default()
    }
}

/// A second member of a fixture org. Only an id: membership is the platform's.
pub struct Member {
    pub id: UserId,
}

impl Member {
    pub fn new() -> Self {
        Self { id: UserId::new() }
    }
}
