//! The endpoint catalog — one description of the console API, used twice.
//!
//! **Why a catalog rather than annotations on each handler.** The plan asks for
//! an OpenAPI document generated from the handlers, and the failure mode it is
//! guarding against is a document that drifts from the code. A macro on each
//! handler is one way; this is another, and it makes drift impossible rather
//! than merely detectable: [`crate::router`] is *built from* this list, and
//! [`crate::openapi::document`] is *rendered from* the same list. There is no
//! second place where a route is declared, so there is nothing for a document
//! to fall out of step with.
//!
//! It also keeps the OpenAPI vocabulary out of `tower-core`. The alternative —
//! deriving schema traits on the domain types — spreads a documentation
//! dependency across every crate to describe an interface only this one serves.
//!
//! The cost is honest and worth naming: request and response bodies are
//! referenced by component name rather than derived from the Rust types, so a
//! field added to a response struct does not appear in the document until
//! someone adds it to [`crate::openapi::components`]. The test at the bottom of
//! `openapi.rs` catches a *missing* component, not a stale field.

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put, MethodRouter};

use crate::openapi;
use crate::routes::{audit, auth, jobs, platform, repos, trackers, webhooks};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Get => "get",
            Verb::Post => "post",
            Verb::Put => "put",
            Verb::Patch => "patch",
            Verb::Delete => "delete",
        }
    }
}

/// What a caller must hold to reach an endpoint.
///
/// Documentation *and* an assertion: the test in `openapi.rs` checks that every
/// endpoint claiming an org scope actually sits under an `{org}` path segment,
/// which is what makes [`crate::session::OrgCtx`] able to resolve one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No credential needed. Discovery documents, the machine endpoints whose
    /// caller authenticates some other way (a tracker's or the platform's
    /// webhook signature), and the console's sign-in routes, which exist to
    /// *produce* a credential.
    Public,
    /// A signed-in caller — a console session or a platform bearer token — in
    /// whichever org that credential opens. No `{org}` in the path.
    Session,
    /// A console session or a platform-issued bearer token whose org is the one
    /// in the path.
    OrgMember,
    /// As [`Auth::OrgMember`], and the token's holder is an `owner` or `admin`
    /// of that org.
    OrgAdmin,
}

impl Auth {
    pub fn as_str(self) -> &'static str {
        match self {
            Auth::Public => "public",
            Auth::Session => "session",
            Auth::OrgMember => "org member",
            Auth::OrgAdmin => "org admin",
        }
    }

    pub fn needs_org(self) -> bool {
        matches!(self, Auth::OrgMember | Auth::OrgAdmin)
    }
}

pub struct Endpoint {
    pub verb: Verb,
    pub path: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub auth: Auth,
    /// Component schema name for the request body, if any.
    pub request: Option<&'static str>,
    /// Component schema name for the response body, if any.
    pub response: Option<&'static str>,
    /// The success status this endpoint actually answers with. Defaults to
    /// `200`; call [`Endpoint::status`] for anything else. Getting this wrong
    /// is not cosmetic — a generated client that expects `200` treats a real
    /// `201`/`204`/`303` success as an error.
    pub success: u16,
    pub route: MethodRouter<AppState>,
}

impl Endpoint {
    fn build(verb: Verb, path: &'static str, route: MethodRouter<AppState>) -> Self {
        Self {
            verb,
            path,
            summary: "",
            description: "",
            auth: Auth::OrgMember,
            request: None,
            response: None,
            success: 200,
            route,
        }
    }

    pub fn get<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Get, path, get(handler))
    }

    pub fn post<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Post, path, post(handler))
    }

    pub fn put<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Put, path, put(handler))
    }

    pub fn patch<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Patch, path, patch(handler))
    }

    pub fn delete<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Delete, path, delete(handler))
    }

    pub fn summary(mut self, summary: &'static str) -> Self {
        self.summary = summary;
        self
    }

    pub fn describe(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    pub fn takes(mut self, schema: &'static str) -> Self {
        self.request = Some(schema);
        self
    }

    pub fn returns(mut self, schema: &'static str) -> Self {
        self.response = Some(schema);
        self
    }

    /// Declare a success status other than the `200` default —
    /// `201 Created`, `204 No Content`, `303 See Other`, and so on, matching
    /// exactly what the handler actually sends.
    pub fn status(mut self, code: u16) -> Self {
        self.success = code;
        self
    }

    /// Cap the request body this route will read into memory, in bytes.
    ///
    /// Only needed on `Auth::Public` routes: token-authenticated
    /// endpoints already require a caller who spent a credential to reach
    /// them, but a public route like `/webhooks/{provider}` is reachable by
    /// anyone on the internet, and `Bytes`/`Json` extractors buffer the whole
    /// body before a handler ever runs. Without an explicit limit, an
    /// oversized payload is a free memory/CPU DoS against an unauthenticated
    /// surface.
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.route = self.route.layer(DefaultBodyLimit::max(bytes));
        self
    }

    /// `GET /api/orgs/{org}/repos` → `getApiOrgsOrgRepos`. Stable across
    /// renames of the Rust function, which is what an OpenAPI operation id has
    /// to be — client generators turn it into a method name.
    pub fn operation_id(&self) -> String {
        let mut id = String::from(self.verb.as_str());
        for segment in self.path.split('/').filter(|s| !s.is_empty()) {
            let segment = segment.trim_matches(|c| c == '{' || c == '}');
            // Filtered before the first character is singled out, not after:
            // a segment like `.well-known` would otherwise capitalize the
            // leading `.` and leave it in place, producing an identifier that
            // starts with a dot. `operationId` becomes a method name in most
            // generators, and a leading `.` is not a legal identifier start
            // anywhere that matters.
            let mut chars = segment.chars().filter(|c| c.is_ascii_alphanumeric());
            if let Some(first) = chars.next() {
                id.push(first.to_ascii_uppercase());
                id.extend(chars);
            }
        }
        id
    }

    /// The `{name}` segments in this endpoint's path.
    pub fn path_params(&self) -> Vec<&'static str> {
        self.path
            .split('/')
            .filter(|s| s.starts_with('{') && s.ends_with('}'))
            .map(|s| s.trim_matches(|c| c == '{' || c == '}'))
            .collect()
    }
}

/// Every endpoint the console API serves.
pub fn catalog() -> Vec<Endpoint> {
    vec![
        Endpoint::get("/api/openapi.json", openapi::serve)
            .auth(Auth::Public)
            .summary("This document")
            .describe("The OpenAPI description of everything below."),
        Endpoint::post("/webhooks/{provider}", webhooks::receive)
            .auth(Auth::Public)
            .body_limit(1_048_576)
            .summary("Receive tracker webhooks")
            .describe(
                "Public by necessity: GitHub App deliveries are authenticated by \
                 `X-Hub-Signature-256`, and JIRA Automation deliveries by the \
                 org's own `X-OF-Webhook-Secret` plus a `?site=<cloud-id>` URL \
                 parameter. This endpoint only verifies, parses, and resolves the \
                 owning org today; Task 4 turns accepted events into sync work. \
                 Bodies are capped at 1 MiB — far larger than any GitHub issue/\
                 comment or JIRA Automation payload this route parses, and small \
                 enough to bound memory/CPU spent buffering an unauthenticated \
                 request before signature verification runs.",
            ),
        Endpoint::post("/platform/webhooks", platform::receive)
            .auth(Auth::Public)
            .body_limit(65_536)
            .returns("PlatformEventAck")
            .summary("Receive Savvagent platform lifecycle webhooks")
            .describe(
                "Called by the Savvagent platform when it deletes an org (`org.deleted`), \
                 deletes a team (`team.deleted`), or removes a member \
                 (`member.removed`), so this service can clean up the rows it holds \
                 for them. Public by necessity: authenticated by the `Otto-Signature` \
                 header (HMAC-SHA256 over the timestamp and raw body, keyed by this \
                 resource server's webhook signing secret), and rejected with 401 \
                 otherwise. Deliveries are at-least-once and idempotent on the event \
                 id, so any 2xx means \"handled\", including a repeat. A handling \
                 failure answers 5xx so the platform retries. Bodies are capped at \
                 64 KiB.",
            ),
        // ------------------------------------------------------------ sign-in
        Endpoint::get("/auth/login", auth::login)
            .auth(Auth::Public)
            .status(303)
            .summary("Start console sign-in")
            .describe(
                "Redirects (303) to the Savvagent platform's authorization endpoint with a \
                 fresh PKCE challenge (S256) and `state`, remembering both, with where to \
                 land afterwards, in a short-lived sealed `__Host-of_oauth` cookie. \
                 `org=<slug>` is passed to the platform as `org_hint`, so a person who \
                 belongs to several orgs is signed in to that one; `next=<path>` is where \
                 to land afterwards, honoured only if it is a same-origin path. Answers \
                 `503 console_login_disabled` when this deployment has no console client \
                 id configured.",
            ),
        Endpoint::get("/auth/callback", auth::callback)
            .auth(Auth::Public)
            .status(303)
            .summary("Finish console sign-in")
            .describe(
                "Where the platform sends the browser back. Checks `state` against the \
                 `__Host-of_oauth` cookie, redeems the code at the platform's token \
                 endpoint (server to server, with the PKCE verifier), stores the resulting \
                 session, sets the `__Host-of_session` cookie, and redirects (303) to the \
                 requested path or `/o/{org}`. Every failure redirects to \
                 `/?login_error=<code>` (`access_denied`, `invalid_state`, \
                 `platform_unavailable`, `failed`).",
            ),
        Endpoint::post("/auth/logout", auth::logout)
            .auth(Auth::Public)
            .status(204)
            .summary("Sign out of the console")
            .describe(
                "Revokes the session's refresh token at the platform (best effort), \
                 deletes the session, and clears the cookie. Idempotent. Like every \
                 write made with the session cookie, it must come from this site: the \
                 `Origin` must be this service's own origin.",
            ),
        Endpoint::get("/api/session", auth::get_session)
            .auth(Auth::Session)
            .returns("Session")
            .summary("Who is signed in")
            .describe(
                "The caller, the one org their session or token is signed in to, their \
                 role in it, the scopes granted, and where the platform's own console \
                 lives (`platformUrl`) for everything identity-shaped. `401` when there \
                 is no valid session.",
            ),
        // ----------------------------------------------------------- repos
        Endpoint::get("/api/orgs/{org}/repos", repos::list_repos)
            .auth(Auth::OrgMember)
            .returns("RepoList")
            .summary("Registered repos")
            .describe("Pass `includeInactive=true` to include soft-disabled ones."),
        Endpoint::post("/api/orgs/{org}/repos", repos::register_repo)
            .auth(Auth::OrgAdmin)
            .takes("RegisterRepoRequest")
            .returns("Repo")
            .status(201)
            .summary("Register a repo")
            .describe(
                "Remotes are normalized, so the SSH and HTTPS forms of one repository \
                 collapse to a single row and either resolves to it.",
            ),
        Endpoint::get("/api/orgs/{org}/repos/{repo}", repos::get_repo)
            .auth(Auth::OrgMember)
            .returns("Repo")
            .summary("One repo, by slug"),
        Endpoint::patch("/api/orgs/{org}/repos/{repo}", repos::update_repo)
            .auth(Auth::OrgAdmin)
            .takes("UpdateRepoRequest")
            .returns("Repo")
            .summary("Update a repo")
            .describe(
                "Absent fields are left alone. The slug cannot be changed — agents \
                 name it, so renaming would break every skill that does.",
            ),
        // -------------------------------------------------------- trackers
        Endpoint::get(
            "/api/orgs/{org}/tracker-connections",
            trackers::list_tracker_connections,
        )
        .auth(Auth::OrgAdmin)
        .returns("TrackerConnections")
        .summary("Tracker connections, and what this deployment can connect")
        .describe(
            "Stored ciphertext is never returned; `hasCredentials` says whether there is \
             any. The per-provider `configured` flag is the conjunction of every credential \
             the connect flow needs, so a console never offers a flow the server cannot \
             finish.",
        ),
        Endpoint::post(
            "/api/orgs/{org}/tracker-connections/{provider}",
            trackers::connect_tracker,
        )
        .auth(Auth::OrgAdmin)
        .takes("ConnectTrackerRequest")
        .returns("TrackerConnection")
        .summary("Redeem a provider authorization and record the connection")
        .describe(
            "A POST because the body carries a single-use authorization code: a link \
             preview following the provider's redirect must burn nothing. GitHub also \
             requires `installationId`, and it is verified against the installations the \
             authorizing account actually administers — an unverified installation id \
             would let one org drive another's issues.",
        ),
        Endpoint::delete(
            "/api/orgs/{org}/tracker-connections/{provider}",
            trackers::disconnect_tracker,
        )
        .auth(Auth::OrgAdmin)
        .status(204)
        .summary("Disconnect a tracker")
        .describe(
            "Bindings pointing at it go inert rather than invalid. Removing the connection \
             also releases the provider's external id, so another org can connect it.",
        ),
        Endpoint::get(
            "/api/orgs/{org}/repos/{repo}/tracker-bindings",
            trackers::list_repo_bindings,
        )
        .auth(Auth::OrgMember)
        .returns("TrackerBindingList")
        .summary("Which tracker projects this repo maps to")
        .describe("`live` is false while the org has no connection for that provider."),
        Endpoint::put(
            "/api/orgs/{org}/repos/{repo}/tracker-bindings/{provider}",
            trackers::bind_repo,
        )
        .auth(Auth::OrgAdmin)
        .takes("BindRepoRequest")
        .returns("TrackerBinding")
        .summary("Point a repo at a tracker project")
        .describe(
            "`externalRef` is `owner/repo` for GitHub and a project key for JIRA — the \
             exact strings webhook ingest matches on, so anything else is refused rather \
             than stored inert. The connection is looked up from the org, never supplied.",
        ),
        Endpoint::delete(
            "/api/orgs/{org}/repos/{repo}/tracker-bindings/{provider}",
            trackers::unbind_repo,
        )
        .auth(Auth::OrgAdmin)
        .status(204)
        .summary("Stop mapping a repo to a tracker project"),
        Endpoint::get("/api/orgs/{org}/repos/{repo}/leases", repos::list_leases)
            .auth(Auth::OrgMember)
            .returns("LeaseList")
            .summary("Who is in this repo right now")
            .describe("The console's answer to \"why is my agent waiting?\"."),
        // ----------------------------------------------------------- queue
        Endpoint::get("/api/orgs/{org}/jobs", jobs::list_jobs)
            .auth(Auth::OrgMember)
            .returns("JobList")
            .summary("The queue, newest first")
            .describe(
                "Read-only, and deliberately so: every write to a job belongs to the \
                 MCP surface, because the agent doing the work is the only party that \
                 can say when it is done. Filter with `status`, `repo`, `team`, and \
                 `mine=true`; `repo` and `team` name slugs, and an unregistered one \
                 is an error listing what is registered.",
            ),
        Endpoint::get("/api/orgs/{org}/jobs/stats", jobs::job_stats)
            .auth(Auth::OrgMember)
            .returns("QueueStats")
            .summary("Counts per status, plus how many are blocked")
            .describe(
                "`blocked` is not a status — it counts pending jobs still waiting on \
                 a dependency, which is what separates a queue that is idle from one \
                 that is stuck. Optionally narrowed to one repo slug.",
            ),
        Endpoint::get("/api/orgs/{org}/jobs/{job}", jobs::get_job)
            .auth(Auth::OrgMember)
            .returns("JobDetail")
            .summary("One job, with the jobs it waits on"),
        Endpoint::get("/api/orgs/{org}/audit", audit::get_audit)
            .auth(Auth::OrgAdmin)
            .returns("AuditEventList")
            .summary("This service's audit log for the org")
            .describe(
                "The domain events this service records — repo, tracker, and job \
                 events, and the clean-up it performs for platform lifecycle webhooks. \
                 Identity and sign-in events are the platform's own audit log. \
                 Admin-only, unlike the rest of the console's reads.",
            ),
    ]
}
