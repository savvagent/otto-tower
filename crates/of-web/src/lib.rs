//! `of-web` — the console API.
//!
//! Everything a human's browser asks of Savvagent Tower *about its own domain*:
//! repos, the queue, tracker connections, and this service's audit trail. Plus
//! two machine endpoints, both authenticated by signature rather than by token:
//! tracker webhooks (`/webhooks/{provider}`) and the Savvagent platform's lifecycle
//! webhooks (`/platform/webhooks`).
//!
//! Identity is not here. Accounts, orgs, members, teams, SSO, tokens, and the
//! usage meter live in the Savvagent platform, which is also the OAuth authorization
//! server for the MCP surface and for the console's own sign-in. This crate is a
//! *resource server* and an OAuth *client*: it asks the platform who a token
//! belongs to, and it holds the session a console login leaves behind. That is
//! all it knows about identity.
//!
//! ```text
//!   Browser        ──► /auth/login, /auth/callback   OAuth code + PKCE against the platform
//!   Browser        ──► /api/orgs/{org}/…   console session cookie ──┐
//!   Script         ──► /api/orgs/{org}/…   platform bearer token ───┴► OrgCtx ──► of-core
//!   Tracker        ──► /webhooks/{provider}  provider signature
//!   Platform       ──► /platform/webhooks    Otto-Signature ──► platform_events
//! ```
//!
//! The console signs in through the platform (`routes::auth`) and keeps its own
//! session: a cookie that keys a stored platform token pair (`console`). Both
//! credentials are resolved to the platform's claims by one function
//! (`session::authenticate`).
//!
//! ## What holds across the whole crate
//!
//! **No SQL.** Every statement is a `of-core` method, for the same reason as in
//! `of-mcp`: a query written here would bypass the tenant-pinned transaction
//! that isolation's second guard depends on.
//!
//! **Authorization is decided by an extractor, not by a handler.**
//! [`session::OrgCtx`] resolves the caller, the org in the path, and their role
//! before a handler body runs, and a handler that needs more than membership
//! says so in one line. A handler that forgets is a handler that serves another
//! tenant's data, and a type is a better place for that than a review checklist.
//!
//! **An org that is not the token's is `404`.** Answering `403` on a real slug
//! and `404` on a fake one turns any token into a directory of who uses the
//! product.
//!
//! **The router and the OpenAPI document come from one list.** See
//! [`catalog`] — routes and their descriptions are the same declaration, so
//! they cannot drift apart.

pub mod catalog;
pub mod console;
pub mod cookies;
pub mod csrf;
pub mod error;
pub mod oauth;
pub mod openapi;
pub mod routes;
pub mod session;
pub mod state;

use axum::Router;

pub use error::{ApiError, ApiResult};
pub use state::{AppState, Config};

/// Build the console surface, ready to be merged into `of-server`'s router.
///
/// Every route comes from [`catalog::catalog`]. Grouping by path before
/// mounting is not cosmetic: `Router::route` panics when the same path is
/// registered twice, so several methods on one path have to arrive as a single
/// merged `MethodRouter`.
pub fn router(state: AppState) -> Router {
    let mut by_path: Vec<(&'static str, axum::routing::MethodRouter<AppState>)> = Vec::new();

    for endpoint in catalog::catalog() {
        match by_path.iter_mut().find(|(path, _)| *path == endpoint.path) {
            Some((_, existing)) => {
                let merged = std::mem::replace(existing, axum::routing::MethodRouter::new());
                *existing = merged.merge(endpoint.route);
            }
            None => by_path.push((endpoint.path, endpoint.route)),
        }
    }

    let origin = csrf::allowed_origin(&state.config);

    by_path
        .into_iter()
        .fold(Router::new(), |router, (path, methods)| {
            router.route(path, methods)
        })
        // Cookie-authenticated writes must come from this origin; see `csrf`.
        .layer(axum::middleware::from_fn_with_state(origin, csrf::check))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The router is built by merging method routers per path. If that merge is
    /// ever replaced with repeated `route` calls, axum panics at startup — which
    /// is a better failure than a silent one, but only if something builds the
    /// router before a deployment does.
    #[tokio::test]
    async fn the_router_assembles() {
        let db = otto_tenant::Db::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                // Not connected — `connect_lazy` builds a pool without touching
                // the network, which is all this test needs.
                .connect_lazy("postgres://localhost/does-not-exist")
                .expect("lazy pool"),
        );
        let platform = std::sync::Arc::new(
            otto_resource::PlatformClient::new(otto_resource::ClientConfig::new(
                "http://127.0.0.1:1",
                "https://mcp.test/mcp",
                "secret",
            ))
            .expect("platform client"),
        );

        let config = Config::new(
            "https://console.test",
            "https://mcp.test/mcp",
            "https://otto.test",
            "whsec",
        );
        let state = AppState::new(
            db,
            otto_tenant::crypto::Cipher::from_base64_key(
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            )
            .expect("test key"),
            platform,
            config,
        );

        let _router = router(state);
    }
}
