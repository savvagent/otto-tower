//! An in-process stand-in for the Savvagent platform's resource-server API.
//!
//! Integration tests that exercise introspection, the internal lookups, usage
//! shipping, or the lifecycle webhooks run against [`MockPlatform`], a real HTTP
//! server on a loopback port speaking the same wire format as
//! `otto-platform-server` (it reuses `otto-resource`'s own wire types, so the two
//! cannot drift silently). Hand a test the real [`PlatformClient`] from
//! [`MockPlatform::client`] and the code under test cannot tell the difference.
//!
//! What it models, because the tests depend on it:
//!
//! - **Audience.** A token is minted for one `resource_uri`; the mock introspects
//!   it as active only for a caller whose HTTP Basic client id is that URI, like
//!   the platform does. [`MockPlatform::issue_for`] mints one for a *different*
//!   resource server to prove that is refused.
//! - **Outages.** [`MockPlatform::set_down`] makes every endpoint answer `503`,
//!   which is how "platform down means 503, never 401" is tested.
//! - **Usage dedupe.** `POST /internal/usage` counts an `event_id` once and
//!   reports repeats as duplicates, so "ships exactly once" is checkable by
//!   counting what [`MockPlatform::counted_usage`] holds.
//! - **The console's OAuth flow.** `POST /oauth/token` redeems an authorization
//!   code (single use, PKCE `S256` checked, `redirect_uri` and `client_id` must
//!   match) or rotates a refresh token; `POST /oauth/revoke` revokes one. Like the
//!   real platform, **a refresh token is consumed by use, and presenting a
//!   consumed one revokes the whole family** (every refresh and access token
//!   minted from the same login). [`MockPlatform::issue_code`] stands in for the
//!   platform's `/oauth/authorize` once a person has signed in;
//!   [`MockPlatform::refresh_calls`] counts rotations, which is how
//!   "refreshes exactly once" is checked.
//! - **Usage standing.** `GET /internal/orgs/{org}/usage-status` adds what was
//!   shipped to the org's counts and sums its metered events this UTC month
//!   into `meters`. [`MockPlatform::set_blocked_reason`] and
//!   [`MockPlatform::set_budget`] set the fields enforcement reads, and
//!   [`MockPlatform::status_calls`] counts the lookups on their own, so "this
//!   tool never asked" is checkable without member and team lookups in the way.
//! - **Meter declarations.** `PUT /internal/meters` records what a resource
//!   server declares ([`MockPlatform::declared_meters`]) and makes
//!   `usage-status` show the declared display name and unit.
//!   [`MockPlatform::set_meter_mode`] switches it to a `409 service_not_set`,
//!   an outage, or a `200` that rejects every meter, and
//!   [`MockPlatform::usage_posts_before_declaration`] catches a shipper that
//!   posts usage first.
//! - **Webhook signing.** [`MockPlatform::webhook`] produces a signed delivery
//!   (header + raw body) for a given event.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Form, Json, Router};
use base64::Engine;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use otto_resource::{
    BudgetStatus, ClientConfig, IntrospectionResponse, MemberInfo, MemberTeams, MeterDeclaration,
    MeterDeclarationBatch, MeterDeclarationReceipt, MeterUsage, OrgInfo, PlatformClient,
    RejectedEvent, RejectedMeter, Role, TeamInfo, TokenKind, UsageBatch, UsageEvent, UsageReceipt,
    UsageStatus, UserInfo,
};
use serde::Deserialize;
use uuid::Uuid;

pub use otto_resource;

/// The `resource_uri` the mock registers its default resource server under.
pub const RESOURCE_URI: &str = "https://factory.test/mcp";
/// The introspection credential the mock expects.
pub const SECRET: &str = "otto_rs_test_secret";
/// The key lifecycle webhooks are signed with.
pub const WEBHOOK_SECRET: &str = "otto_whsec_test_secret";

#[derive(Clone)]
struct TokenRecord {
    jti: Uuid,
    /// `None` for a [`MockPlatform::issue_floating`] token: it opens whichever
    /// org its user joined most recently, resolved at introspection time.
    org: Option<Uuid>,
    user: Uuid,
    scopes: Vec<String>,
    resource: String,
    exp: i64,
    kind: TokenKind,
}

#[derive(Default)]
struct Data {
    tokens: HashMap<String, TokenRecord>,
    orgs: HashMap<Uuid, OrgInfo>,
    /// (org, user) -> (info, role, join order)
    members: HashMap<(Uuid, Uuid), (UserInfo, Role, u64)>,
    joins: u64,
    teams: HashMap<(Uuid, Uuid), TeamInfo>,
    /// (org, user, team) memberships.
    team_members: HashSet<(Uuid, Uuid, Uuid)>,
    usage: HashMap<Uuid, UsageStatus>,
    counted: Vec<UsageEvent>,
    /// Meters declared through `PUT /internal/meters`, by key.
    meters: Vec<MeterDeclaration>,
    /// Usage POSTs that arrived while no meter had been declared.
    usage_before_declaration: usize,
    /// Once set (by [`MockPlatform::forget_meters`]), a metered event whose
    /// meter is not declared is refused as `unknown_meter`, as the real platform
    /// does. Off by default so tests that ship without declaring are unaffected.
    enforce_declared_meters: bool,
    /// Set by [`MockPlatform::refuse_every_meter`]: every metered event is
    /// `unknown_meter`, declared or not.
    refuse_every_meter: bool,
    seen: HashSet<Uuid>,
    /// Org ids whose usage the mock refuses, like a deleted org.
    unknown_orgs_for_usage: HashSet<Uuid>,
    /// Authorization codes issued and not yet redeemed.
    codes: HashMap<String, CodeRecord>,
    /// Refresh tokens ever issued, consumed or not.
    refresh_tokens: HashMap<String, RefreshRecord>,
    /// Access tokens issued per login family, so revoking the family kills them.
    family_access: HashMap<Uuid, Vec<String>>,
    revoked_families: HashSet<Uuid>,
    /// Refresh tokens a client revoked through `/oauth/revoke`.
    revoked_refresh_tokens: Vec<String>,
}

struct CodeRecord {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    org: Uuid,
    user: Uuid,
    scopes: Vec<String>,
    resource: String,
}

struct RefreshRecord {
    family: Uuid,
    client_id: String,
    org: Uuid,
    user: Uuid,
    scopes: Vec<String>,
    resource: String,
    consumed: bool,
}

struct Inner {
    data: Mutex<Data>,
    down: AtomicBool,
    /// While set, `usage-status` alone answers `503`.
    status_down: AtomicBool,
    /// Fail the next N usage POSTs with 503, then recover.
    fail_usage: AtomicUsize,
    /// How `PUT /internal/meters` answers; see [`MeterMode`].
    meter_mode: Mutex<MeterMode>,
    /// Calls to `PUT /internal/meters`, whatever they answered.
    meter_calls: AtomicUsize,
    /// Answer the next N usage POSTs with a receipt that under-reports by one.
    short_receipts: AtomicUsize,
    introspect_calls: AtomicUsize,
    usage_calls: AtomicUsize,
    lookup_calls: AtomicUsize,
    /// `GET /internal/orgs/{org}/usage-status` alone (also in `lookup_calls`).
    status_calls: AtomicUsize,
    /// Milliseconds `usage-status` waits before answering (after counting).
    status_delay_ms: AtomicU64,
    code_exchanges: AtomicUsize,
    refresh_calls: AtomicUsize,
    /// Lifetime, in seconds, of access tokens minted by the token endpoint.
    access_ttl_secs: AtomicI64,
    /// How long the token endpoint sleeps before answering a refresh, so a test
    /// can hold the window in which concurrent requests would double-refresh.
    refresh_delay_ms: AtomicU64,
}

/// How the mock answers `PUT /internal/meters`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MeterMode {
    /// Declare every meter and say so (200, all in `declared`).
    #[default]
    Accept,
    /// The resource server's service slug is unset: `409 service_not_set`.
    ServiceNotSet,
    /// An outage: `503`.
    Outage,
    /// A refusal: `200` with every meter in `rejected` and none declared.
    Reject,
}

pub struct MockPlatform {
    pub url: String,
    inner: Arc<Inner>,
    _task: tokio::task::JoinHandle<()>,
}

impl MockPlatform {
    /// Start the mock on a free loopback port.
    pub async fn start() -> Self {
        let inner = Arc::new(Inner {
            data: Mutex::new(Data::default()),
            down: AtomicBool::new(false),
            status_down: AtomicBool::new(false),
            fail_usage: AtomicUsize::new(0),
            meter_mode: Mutex::new(MeterMode::Accept),
            meter_calls: AtomicUsize::new(0),
            short_receipts: AtomicUsize::new(0),
            introspect_calls: AtomicUsize::new(0),
            usage_calls: AtomicUsize::new(0),
            lookup_calls: AtomicUsize::new(0),
            status_calls: AtomicUsize::new(0),
            status_delay_ms: AtomicU64::new(0),
            code_exchanges: AtomicUsize::new(0),
            refresh_calls: AtomicUsize::new(0),
            access_ttl_secs: AtomicI64::new(3600),
            refresh_delay_ms: AtomicU64::new(0),
        });

        let app = Router::new()
            .route("/oauth/introspect", post(introspect))
            .route("/oauth/token", post(token_endpoint))
            .route("/oauth/revoke", post(revoke_endpoint))
            .route("/internal/usage", post(ingest_usage))
            .route("/internal/meters", put(declare_meters))
            .route("/internal/orgs/{org}/usage-status", get(usage_status))
            .route(
                "/internal/orgs/{org}/members/by-email",
                get(member_by_email),
            )
            .route("/internal/orgs/{org}/members/{user}", get(member))
            .route(
                "/internal/orgs/{org}/members/{user}/teams",
                get(member_teams),
            )
            .route(
                "/internal/orgs/{org}/teams/by-slug/{slug}",
                get(team_by_slug),
            )
            .route("/internal/orgs/{org}/teams/{team}", get(team))
            .with_state(inner.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock platform");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock platform");
        });

        Self {
            url: format!("http://{addr}"),
            inner,
            _task: task,
        }
    }

    /// A real [`PlatformClient`] pointed at this mock, authenticating as the
    /// default resource server.
    pub fn client(&self) -> Arc<PlatformClient> {
        self.client_with(ClientConfig::new(&self.url, RESOURCE_URI, SECRET))
    }

    pub fn client_with(&self, cfg: ClientConfig) -> Arc<PlatformClient> {
        Arc::new(PlatformClient::new(cfg).expect("build the platform client"))
    }

    // ----------------------------------------------------------------- world

    /// Register an org (and make it known to usage and lookups).
    pub fn add_org(&self, org: Uuid, slug: &str, name: &str) {
        let mut d = self.inner.data.lock().unwrap();
        d.orgs.insert(
            org,
            OrgInfo {
                id: org,
                slug: slug.into(),
                name: name.into(),
                plan: "free".into(),
            },
        );
        d.usage.entry(org).or_insert_with(|| UsageStatus {
            org_id: org,
            plan: "free".into(),
            period_start: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            billable_count: 0,
            total_count: 0,
            included_ops: 500,
            hard_stop: true,
            features: Default::default(),
            service: None,
            meters: Vec::new(),
            budget: None,
            blocked_reason: None,
        });
    }

    pub fn add_member(&self, org: Uuid, user: Uuid, email: &str, role: Role) {
        let mut d = self.inner.data.lock().unwrap();
        d.joins += 1;
        let order = d.joins;
        d.members.insert(
            (org, user),
            (
                UserInfo {
                    id: user,
                    email: Some(email.into()),
                    name: Some(email.split('@').next().unwrap_or(email).into()),
                },
                role,
                order,
            ),
        );
    }

    /// Change a member's role.
    pub fn set_role(&self, org: Uuid, user: Uuid, role: Role) {
        if let Some(m) = self
            .inner
            .data
            .lock()
            .unwrap()
            .members
            .get_mut(&(org, user))
        {
            m.1 = role;
        }
    }

    pub fn remove_member(&self, org: Uuid, user: Uuid) {
        self.inner.data.lock().unwrap().members.remove(&(org, user));
    }

    pub fn add_team(&self, org: Uuid, team: Uuid, slug: &str, name: &str) {
        self.inner.data.lock().unwrap().teams.insert(
            (org, team),
            TeamInfo {
                id: team,
                org_id: org,
                slug: slug.into(),
                name: name.into(),
            },
        );
    }

    /// Put `user` on `team`. The platform lists a member's teams from its own
    /// membership table, so the team and the member must both exist too.
    pub fn add_team_member(&self, org: Uuid, user: Uuid, team: Uuid) {
        self.inner
            .data
            .lock()
            .unwrap()
            .team_members
            .insert((org, user, team));
    }

    pub fn remove_team_member(&self, org: Uuid, user: Uuid, team: Uuid) {
        self.inner
            .data
            .lock()
            .unwrap()
            .team_members
            .remove(&(org, user, team));
    }

    pub fn remove_team(&self, org: Uuid, team: Uuid) {
        self.inner.data.lock().unwrap().teams.remove(&(org, team));
    }

    /// Set an org's usage standing.
    pub fn set_usage(&self, org: Uuid, billable: i64, included: i64, hard_stop: bool) {
        let mut d = self.inner.data.lock().unwrap();
        let entry = d.usage.entry(org).or_insert_with(|| UsageStatus {
            org_id: org,
            plan: "free".into(),
            period_start: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            billable_count: 0,
            total_count: 0,
            included_ops: 0,
            hard_stop,
            features: Default::default(),
            service: None,
            meters: Vec::new(),
            budget: None,
            blocked_reason: None,
        });
        entry.billable_count = billable;
        entry.total_count = billable;
        entry.included_ops = included;
        entry.hard_stop = hard_stop;
    }

    /// Set (or clear, with `None`) why the platform tells the calling service to
    /// refuse billable calls for `org`: `budget_exceeded`, `payment_required`,
    /// or anything else a future platform might send.
    pub fn set_blocked_reason(&self, org: Uuid, reason: Option<&str>) {
        let mut d = self.inner.data.lock().unwrap();
        usage_entry(&mut d, org).blocked_reason = reason.map(str::to_string);
    }

    /// Set (or clear, with `None`) the calling service's budget for `org`.
    pub fn set_budget(&self, org: Uuid, budget: Option<BudgetStatus>) {
        let mut d = self.inner.data.lock().unwrap();
        usage_entry(&mut d, org).budget = budget;
    }

    /// Make the platform refuse usage for an org the way it refuses a deleted one.
    pub fn reject_usage_for(&self, org: Uuid) {
        self.inner
            .data
            .lock()
            .unwrap()
            .unknown_orgs_for_usage
            .insert(org);
    }

    // ---------------------------------------------------------------- tokens

    /// Make sure the org, the membership (with `role`), and a usage record exist.
    fn ensure_member(&self, org: Uuid, user: Uuid, role: Role) {
        {
            let mut d = self.inner.data.lock().unwrap();
            d.orgs.entry(org).or_insert_with(|| OrgInfo {
                id: org,
                slug: format!("org-{}", &org.to_string()[..8]),
                name: "Test Org".into(),
                plan: "free".into(),
            });
            if !d.members.contains_key(&(org, user)) {
                d.joins += 1;
                let order = d.joins;
                d.members.insert(
                    (org, user),
                    (
                        UserInfo {
                            id: user,
                            email: Some(format!("{}@test.example", &user.to_string()[..8])),
                            name: None,
                        },
                        role,
                        order,
                    ),
                );
            }
            d.members.get_mut(&(org, user)).unwrap().1 = role;
            d.usage.entry(org).or_insert_with(|| UsageStatus {
                org_id: org,
                plan: "free".into(),
                period_start: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                billable_count: 0,
                total_count: 0,
                included_ops: 500,
                hard_stop: true,
                features: Default::default(),
                service: None,
                meters: Vec::new(),
                budget: None,
                blocked_reason: None,
            });
        }
    }

    /// Mint an OAuth token for the default resource server that opens `org` as
    /// `user`. Also registers the membership (with `role`) and the org if the
    /// mock has not seen them, so one call gives a working caller.
    pub fn issue(&self, org: Uuid, user: Uuid, role: Role, scopes: &[&str]) -> String {
        self.issue_for(RESOURCE_URI, org, user, role, scopes)
    }

    /// As [`Self::issue`], for an arbitrary resource server's audience.
    pub fn issue_for(
        &self,
        resource: &str,
        org: Uuid,
        user: Uuid,
        role: Role,
        scopes: &[&str],
    ) -> String {
        self.ensure_member(org, user, role);
        let token = new_token();
        self.inner.data.lock().unwrap().tokens.insert(
            token.clone(),
            TokenRecord {
                jti: Uuid::new_v4(),
                org: Some(org),
                user,
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                resource: resource.into(),
                exp: (Utc::now() + chrono::Duration::hours(1)).timestamp(),
                kind: TokenKind::Oauth,
            },
        );
        token
    }

    /// A token for `user` that opens whichever org they joined most recently,
    /// resolved when it is introspected. **Not how the real platform works** (a
    /// real token is fixed to one org when it is issued); it exists so a fixture
    /// can mint one credential per person and then place them in orgs, the way
    /// the console tests were written. Tests about *which org a token opens* use
    /// [`Self::issue`], which is faithful.
    pub fn issue_floating(&self, user: Uuid, scopes: &[&str]) -> String {
        let token = new_token();
        self.inner.data.lock().unwrap().tokens.insert(
            token.clone(),
            TokenRecord {
                jti: Uuid::new_v4(),
                org: None,
                user,
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                resource: RESOURCE_URI.into(),
                exp: (Utc::now() + chrono::Duration::hours(1)).timestamp(),
                kind: TokenKind::Oauth,
            },
        );
        token
    }

    // --------------------------------------------------- console login (OAuth)

    /// What the platform does at `/oauth/authorize` once a person has signed in
    /// and been placed in `org`: mint a single-use authorization code bound to
    /// the client, the redirect URI, and the PKCE `challenge` the client sent.
    /// Registers the membership (with `role`) like [`Self::issue`] does.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_code(
        &self,
        client_id: &str,
        redirect_uri: &str,
        challenge: &str,
        org: Uuid,
        user: Uuid,
        role: Role,
        scopes: &[&str],
    ) -> String {
        self.ensure_member(org, user, role);
        let code = format!("code_{}", &new_token()[8..]);
        self.inner.data.lock().unwrap().codes.insert(
            code.clone(),
            CodeRecord {
                client_id: client_id.into(),
                redirect_uri: redirect_uri.into(),
                challenge: challenge.into(),
                org,
                user,
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                resource: RESOURCE_URI.into(),
            },
        );
        code
    }

    /// Lifetime of access tokens from now on. A value under the console's
    /// refresh skew (60 s) makes a token "about to expire" the moment it is issued.
    pub fn set_access_ttl_secs(&self, secs: i64) {
        self.inner.access_ttl_secs.store(secs, Ordering::SeqCst);
    }

    /// Delay every refresh by this long before answering.
    pub fn set_refresh_delay_ms(&self, ms: u64) {
        self.inner.refresh_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// Successful and failed `grant_type=authorization_code` calls.
    pub fn code_exchanges(&self) -> usize {
        self.inner.code_exchanges.load(Ordering::SeqCst)
    }

    /// `grant_type=refresh_token` calls, whatever they answered.
    pub fn refresh_calls(&self) -> usize {
        self.inner.refresh_calls.load(Ordering::SeqCst)
    }

    /// Refresh tokens clients revoked through `/oauth/revoke`.
    pub fn revoked_refresh_tokens(&self) -> Vec<String> {
        self.inner
            .data
            .lock()
            .unwrap()
            .revoked_refresh_tokens
            .clone()
    }

    /// Revoke every login at once, the way the platform would revoke a family
    /// after an admin signs a user out: their refresh tokens answer
    /// `invalid_grant` and their access tokens go inactive.
    pub fn revoke_all_logins(&self) {
        let mut d = self.inner.data.lock().unwrap();
        let families: Vec<Uuid> = d.refresh_tokens.values().map(|r| r.family).collect();
        for family in families {
            revoke_family(&mut d, family);
        }
    }

    /// Whether any login family has been revoked (by reuse or by `/oauth/revoke`).
    pub fn any_family_revoked(&self) -> bool {
        !self.inner.data.lock().unwrap().revoked_families.is_empty()
    }

    /// Revoke a token at the platform (its next introspection is inactive).
    pub fn revoke(&self, token: &str) {
        self.inner.data.lock().unwrap().tokens.remove(token);
    }

    // --------------------------------------------------------------- failures

    /// Make the next `n` usage POSTs count the batch but answer with a receipt
    /// that accounts for one event fewer than was sent.
    pub fn short_receipts(&self, n: usize) {
        self.inner.short_receipts.store(n, Ordering::SeqCst);
    }

    /// While down, every endpoint answers `503`.
    pub fn set_down(&self, down: bool) {
        self.inner.down.store(down, Ordering::SeqCst);
    }

    /// While down, only `GET /internal/orgs/{org}/usage-status` answers `503`;
    /// introspection and member and team lookups keep working. That is the
    /// outage enforcement has to fail open through, isolated from the lookups
    /// that fail closed.
    pub fn set_status_down(&self, down: bool) {
        self.inner.status_down.store(down, Ordering::SeqCst);
    }

    /// Make `GET /internal/orgs/{org}/usage-status` wait `ms` before it
    /// answers, so a test can change state while a lookup is in flight.
    /// [`Self::status_calls`] already counts the call while it waits.
    pub fn set_status_delay_ms(&self, ms: u64) {
        self.inner.status_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// Fail the next `n` usage POSTs with `503`, then recover.
    pub fn fail_next_usage_posts(&self, n: usize) {
        self.inner.fail_usage.store(n, Ordering::SeqCst);
    }

    /// Choose how `PUT /internal/meters` answers from now on.
    pub fn set_meter_mode(&self, mode: MeterMode) {
        *self.inner.meter_mode.lock().unwrap() = mode;
    }

    // ------------------------------------------------------------ observation

    /// The meters declared so far, in the order first declared. A redeclaration
    /// replaces the display name and unit in place.
    pub fn declared_meters(&self) -> Vec<MeterDeclaration> {
        self.inner.data.lock().unwrap().meters.clone()
    }

    /// Drop every declared meter and, from now on, refuse a metered event whose
    /// meter is not declared with `unknown_meter`: the platform losing a
    /// declaration mid-run.
    pub fn forget_meters(&self) {
        let mut d = self.inner.data.lock().unwrap();
        d.meters.clear();
        d.enforce_declared_meters = true;
    }

    /// Refuse every metered event as `unknown_meter` even after the meter is
    /// declared: a platform that keeps refusing what it was just told about.
    pub fn refuse_every_meter(&self) {
        self.inner.data.lock().unwrap().refuse_every_meter = true;
    }

    /// Calls to `PUT /internal/meters`, including refused ones.
    pub fn meter_calls(&self) -> usize {
        self.inner.meter_calls.load(Ordering::SeqCst)
    }

    /// Usage POSTs that arrived before any meter was declared. A shipper that
    /// declares first leaves this at zero; the real platform would reject a
    /// metered event in such a post as `unknown_meter`.
    pub fn usage_posts_before_declaration(&self) -> usize {
        self.inner.data.lock().unwrap().usage_before_declaration
    }

    /// Usage events the platform has counted, each `event_id` once.
    pub fn counted_usage(&self) -> Vec<UsageEvent> {
        self.inner.data.lock().unwrap().counted.clone()
    }

    pub fn introspect_calls(&self) -> usize {
        self.inner.introspect_calls.load(Ordering::SeqCst)
    }

    pub fn usage_calls(&self) -> usize {
        self.inner.usage_calls.load(Ordering::SeqCst)
    }

    /// Calls to the `/internal/orgs/...` lookup endpoints.
    pub fn lookup_calls(&self) -> usize {
        self.inner.lookup_calls.load(Ordering::SeqCst)
    }

    /// Calls to `GET /internal/orgs/{org}/usage-status` alone: the lookup
    /// enforcement makes. Member and team lookups are not counted here.
    pub fn status_calls(&self) -> usize {
        self.inner.status_calls.load(Ordering::SeqCst)
    }

    // --------------------------------------------------------------- webhooks

    /// A signed delivery for `kind` with `data`: `(Otto-Signature header, raw
    /// body)`. Signed now, with [`WEBHOOK_SECRET`].
    pub fn webhook(kind: &str, data: serde_json::Value) -> (String, Vec<u8>) {
        Self::webhook_with_id(Uuid::new_v4(), kind, data)
    }

    /// As [`Self::webhook`], with a chosen event id, for redelivery tests.
    pub fn webhook_with_id(id: Uuid, kind: &str, data: serde_json::Value) -> (String, Vec<u8>) {
        let body = otto_resource::webhook::body(id, kind, Utc::now(), &data);
        let header = otto_resource::webhook::sign(WEBHOOK_SECRET, Utc::now().timestamp(), &body);
        (header, body)
    }
}

/// A token string with the shape the client accepts (`otto_at_` + 43 chars).
pub fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!(
        "otto_at_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

// ------------------------------------------------------------------ handlers

type S = State<Arc<Inner>>;

fn unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "mock platform is down").into_response()
}

/// Returns the calling resource server's `resource_uri`, or the response to send.
#[allow(clippy::result_large_err)]
fn caller(inner: &Inner, headers: &HeaderMap) -> Result<String, Response> {
    if inner.down.load(Ordering::SeqCst) {
        return Err(unavailable());
    }
    let unauth = || (StatusCode::UNAUTHORIZED, "bad credential").into_response();
    let raw = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(unauth)?;
    let b64 = raw.strip_prefix("Basic ").ok_or_else(unauth)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| unauth())?;
    let decoded = String::from_utf8(decoded).map_err(|_| unauth())?;
    let (user, secret) = decoded.split_once(':').ok_or_else(unauth)?;
    let uri = percent_encoding::percent_decode_str(user)
        .decode_utf8()
        .map_err(|_| unauth())?
        .into_owned();
    if secret != SECRET {
        return Err(unauth());
    }
    Ok(uri)
}

#[derive(Deserialize)]
struct IntrospectForm {
    token: Option<String>,
}

async fn introspect(
    State(inner): S,
    headers: HeaderMap,
    Form(form): Form<IntrospectForm>,
) -> Response {
    let resource = match caller(&inner, &headers) {
        Ok(r) => r,
        Err(res) => return res,
    };
    inner.introspect_calls.fetch_add(1, Ordering::SeqCst);

    let d = inner.data.lock().unwrap();
    let token = form.token.unwrap_or_default();
    let body = match d.tokens.get(token.trim()) {
        // Audience: a token for another resource server is just inactive.
        Some(t) if t.resource == resource && t.exp > Utc::now().timestamp() => {
            let org = t.org.or_else(|| {
                d.members
                    .iter()
                    .filter(|((_, u), _)| *u == t.user)
                    .max_by_key(|(_, (_, _, order))| *order)
                    .map(|((o, _), _)| *o)
            });
            // Active also means the user is still a member; the role is today's.
            match org.and_then(|o| d.members.get(&(o, t.user)).map(|m| (o, m))) {
                Some((org, (_, role, _))) => IntrospectionResponse {
                    active: true,
                    sub: Some(t.user),
                    org_id: Some(org),
                    role: Some(*role),
                    scope: Some(t.scopes.join(" ")),
                    aud: Some(t.resource.clone()),
                    exp: Some(t.exp),
                    client_id: Some("otto_client_test".into()),
                    token_type: Some("Bearer".into()),
                    token_kind: Some(t.kind),
                    jti: Some(t.jti),
                },
                None => IntrospectionResponse::inactive(),
            }
        }
        _ => IntrospectionResponse::inactive(),
    };
    Json(body).into_response()
}

// ------------------------------------------------------------- oauth handlers

#[derive(Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    client_id: Option<String>,
    resource: Option<String>,
    refresh_token: Option<String>,
}

fn oauth_error(error: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

fn s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Mint a refresh token in `family` and an access token, and answer like the
/// platform's token endpoint.
#[allow(clippy::too_many_arguments)]
fn mint_pair(
    inner: &Inner,
    d: &mut Data,
    family: Uuid,
    client_id: &str,
    org: Uuid,
    user: Uuid,
    scopes: Vec<String>,
    resource: String,
) -> Response {
    let ttl = inner.access_ttl_secs.load(Ordering::SeqCst);
    let access = new_token();
    d.tokens.insert(
        access.clone(),
        TokenRecord {
            jti: Uuid::new_v4(),
            org: Some(org),
            user,
            scopes: scopes.clone(),
            resource: resource.clone(),
            exp: (Utc::now() + chrono::Duration::seconds(ttl)).timestamp(),
            kind: TokenKind::Oauth,
        },
    );
    d.family_access
        .entry(family)
        .or_default()
        .push(access.clone());

    let refresh = new_token().replacen("otto_at_", "otto_rt_", 1);
    d.refresh_tokens.insert(
        refresh.clone(),
        RefreshRecord {
            family,
            client_id: client_id.into(),
            org,
            user,
            scopes: scopes.clone(),
            resource,
            consumed: false,
        },
    );
    Json(serde_json::json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ttl,
        "refresh_token": refresh,
        "scope": scopes.join(" "),
    }))
    .into_response()
}

fn revoke_family(d: &mut Data, family: Uuid) {
    d.revoked_families.insert(family);
    for access in d.family_access.remove(&family).unwrap_or_default() {
        d.tokens.remove(&access);
    }
}

async fn token_endpoint(State(inner): S, Form(form): Form<TokenForm>) -> Response {
    if inner.down.load(Ordering::SeqCst) {
        return unavailable();
    }
    match form.grant_type.as_deref() {
        Some("authorization_code") => {
            inner.code_exchanges.fetch_add(1, Ordering::SeqCst);
            let mut d = inner.data.lock().unwrap();
            // Single use: removed whether or not the rest checks out.
            let Some(code) = form.code.as_deref().and_then(|c| d.codes.remove(c)) else {
                return oauth_error("invalid_grant");
            };
            let ok = form.client_id.as_deref() == Some(code.client_id.as_str())
                && form.redirect_uri.as_deref() == Some(code.redirect_uri.as_str())
                && form.resource.as_deref() == Some(code.resource.as_str())
                && form.code_verifier.as_deref().map(s256).as_deref()
                    == Some(code.challenge.as_str());
            if !ok {
                return oauth_error("invalid_grant");
            }
            let family = Uuid::new_v4();
            mint_pair(
                &inner,
                &mut d,
                family,
                &code.client_id,
                code.org,
                code.user,
                code.scopes,
                code.resource,
            )
        }
        Some("refresh_token") => {
            inner.refresh_calls.fetch_add(1, Ordering::SeqCst);
            let delay = inner.refresh_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            let mut d = inner.data.lock().unwrap();
            let Some(token) = form.refresh_token.as_deref() else {
                return oauth_error("invalid_request");
            };
            let Some(record) = d.refresh_tokens.get(token) else {
                return oauth_error("invalid_grant");
            };
            let family = record.family;
            if d.revoked_families.contains(&family) {
                return oauth_error("invalid_grant");
            }
            if record.client_id.as_str() != form.client_id.as_deref().unwrap_or_default() {
                return oauth_error("invalid_grant");
            }
            if record.consumed {
                // Reuse of a spent token: the whole login is compromised.
                revoke_family(&mut d, family);
                return oauth_error("invalid_grant");
            }
            let (client_id, org, user, scopes, resource) = (
                record.client_id.clone(),
                record.org,
                record.user,
                record.scopes.clone(),
                record.resource.clone(),
            );
            // A member who left cannot refresh.
            if !d.members.contains_key(&(org, user)) {
                revoke_family(&mut d, family);
                return oauth_error("invalid_grant");
            }
            d.refresh_tokens.get_mut(token).unwrap().consumed = true;
            mint_pair(
                &inner, &mut d, family, &client_id, org, user, scopes, resource,
            )
        }
        _ => oauth_error("unsupported_grant_type"),
    }
}

#[derive(Deserialize)]
struct RevokeForm {
    token: Option<String>,
}

/// Always `200`, like the platform (RFC 7009).
async fn revoke_endpoint(State(inner): S, Form(form): Form<RevokeForm>) -> Response {
    if inner.down.load(Ordering::SeqCst) {
        return unavailable();
    }
    if let Some(token) = form.token {
        let mut d = inner.data.lock().unwrap();
        if let Some(family) = d.refresh_tokens.get(&token).map(|r| r.family) {
            d.revoked_refresh_tokens.push(token);
            revoke_family(&mut d, family);
        }
    }
    StatusCode::OK.into_response()
}

async fn ingest_usage(
    State(inner): S,
    headers: HeaderMap,
    Json(batch): Json<UsageBatch>,
) -> Response {
    if let Err(res) = caller(&inner, &headers) {
        return res;
    }
    inner.usage_calls.fetch_add(1, Ordering::SeqCst);
    // Toolchain-neutral: `fetch_update` was renamed `try_update` in newer Rust.
    let mut left = inner.fail_usage.load(Ordering::SeqCst);
    let mut fail = false;
    while left > 0 {
        match inner
            .fail_usage
            .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {
                fail = true;
                break;
            }
            Err(now) => left = now,
        }
    }
    if fail {
        return unavailable();
    }

    let mut d = inner.data.lock().unwrap();
    if d.meters.is_empty() {
        d.usage_before_declaration += 1;
    }
    let mut receipt = UsageReceipt::default();
    for ev in batch.events {
        let undeclared = ev.meter.as_ref().is_some_and(|m| {
            d.refuse_every_meter
                || (d.enforce_declared_meters && !d.meters.iter().any(|x| &x.key == m))
        });
        if undeclared {
            receipt.rejected.push(RejectedEvent {
                event_id: ev.event_id,
                reason: "unknown_meter: the meter is not declared".into(),
            });
        } else if d.unknown_orgs_for_usage.contains(&ev.org_id) {
            receipt.rejected.push(RejectedEvent {
                event_id: ev.event_id,
                reason: format!("org {} does not exist", ev.org_id),
            });
        } else if d.seen.insert(ev.event_id) {
            receipt.accepted += 1;
            d.counted.push(ev);
        } else {
            receipt.duplicates += 1;
        }
    }
    if inner
        .short_receipts
        .load(Ordering::SeqCst)
        .checked_sub(1)
        .is_some()
    {
        inner.short_receipts.fetch_sub(1, Ordering::SeqCst);
        receipt.accepted = receipt.accepted.saturating_sub(1);
    }
    Json(receipt).into_response()
}

async fn declare_meters(
    State(inner): S,
    headers: HeaderMap,
    Json(batch): Json<MeterDeclarationBatch>,
) -> Response {
    if let Err(res) = caller(&inner, &headers) {
        return res;
    }
    inner.meter_calls.fetch_add(1, Ordering::SeqCst);
    let mode = *inner.meter_mode.lock().unwrap();
    let mut receipt = MeterDeclarationReceipt::default();
    match mode {
        MeterMode::ServiceNotSet => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "service_not_set" })),
            )
                .into_response();
        }
        MeterMode::Outage => return unavailable(),
        MeterMode::Reject => {
            receipt.rejected = batch
                .meters
                .into_iter()
                .map(|m| RejectedMeter {
                    key: m.key,
                    reason: "the mock was told to refuse this meter".into(),
                })
                .collect();
        }
        MeterMode::Accept => {
            let mut d = inner.data.lock().unwrap();
            for m in batch.meters {
                receipt.declared.push(m.key.clone());
                match d.meters.iter_mut().find(|x| x.key == m.key) {
                    Some(existing) => *existing = m,
                    None => d.meters.push(m),
                }
            }
        }
    }
    Json(receipt).into_response()
}

async fn usage_status(State(inner): S, headers: HeaderMap, Path(org): Path<Uuid>) -> Response {
    if let Err(res) = caller(&inner, &headers) {
        return res;
    }
    inner.lookup_calls.fetch_add(1, Ordering::SeqCst);
    inner.status_calls.fetch_add(1, Ordering::SeqCst);
    let delay = inner.status_delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    if inner.status_down.load(Ordering::SeqCst) {
        return unavailable();
    }
    let d = inner.data.lock().unwrap();
    match d.usage.get(&org) {
        Some(u) => {
            // The platform's own count includes what was shipped to it.
            let mut u = u.clone();
            let shipped_billable = d
                .counted
                .iter()
                .filter(|e| e.org_id == org && e.billable)
                .count() as i64;
            let shipped_total = d.counted.iter().filter(|e| e.org_id == org).count() as i64;
            u.billable_count += shipped_billable;
            u.total_count += shipped_total;
            u.meters = shipped_meters(&d.counted, &d.meters, org, Utc::now());
            Json(u).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A budget whose hard stop has tripped, as the platform reports one: spend
/// past the budget, `blocked`, reason `budget_exceeded`. Hand it to
/// [`MockPlatform::set_budget`], or spread it to build a budget that is not
/// blocked.
pub fn tripped_budget() -> BudgetStatus {
    BudgetStatus {
        monthly_budget_cents: 1000,
        spend_cents: Some(1250),
        percent_used: Some(125),
        thresholds_reached: vec![50, 80, 100],
        hard_stop: true,
        blocked: true,
        reason: Some(otto_resource::BUDGET_EXCEEDED.into()),
    }
}

/// `org`'s usage record, created with the defaults `add_org` uses if missing.
fn usage_entry(d: &mut Data, org: Uuid) -> &mut UsageStatus {
    d.usage.entry(org).or_insert_with(|| UsageStatus {
        org_id: org,
        plan: "free".into(),
        period_start: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
        billable_count: 0,
        total_count: 0,
        included_ops: 500,
        hard_stop: true,
        features: Default::default(),
        service: None,
        meters: Vec::new(),
        budget: None,
        blocked_reason: None,
    })
}

/// The quantity of each meter `org` shipped in `now`'s UTC month, summed the
/// way the platform sums it (a metered event with no quantity counts one),
/// ordered by key. A declared meter shows its declared display name and unit,
/// and zero when nothing was shipped; one never declared shows its key and an
/// empty unit.
fn shipped_meters(
    counted: &[UsageEvent],
    declared: &[MeterDeclaration],
    org: Uuid,
    now: DateTime<Utc>,
) -> Vec<MeterUsage> {
    let mut sums: std::collections::BTreeMap<&str, u64> = Default::default();
    for ev in counted.iter().filter(|e| {
        e.org_id == org
            && e.occurred_at.year() == now.year()
            && e.occurred_at.month() == now.month()
    }) {
        if let Some(meter) = ev.meter.as_deref() {
            *sums.entry(meter).or_default() += ev.quantity.unwrap_or(1);
        }
    }
    // A declared meter with no usage this month shows zero, as the platform does.
    for decl in declared {
        sums.entry(decl.key.as_str()).or_default();
    }
    sums.into_iter()
        .map(|(meter, quantity)| {
            let decl = declared.iter().find(|m| m.key == meter);
            MeterUsage {
                meter: meter.to_string(),
                display_name: decl.map_or_else(|| meter.to_string(), |m| m.display_name.clone()),
                unit: decl.map(|m| m.unit.clone()).unwrap_or_default(),
                quantity,
            }
        })
        .collect()
}

fn member_info(d: &Data, org: Uuid, user: Uuid) -> Option<MemberInfo> {
    let (u, role, _) = d.members.get(&(org, user))?;
    Some(MemberInfo {
        user: u.clone(),
        org: d.orgs.get(&org)?.clone(),
        role: *role,
    })
}

async fn member(
    State(inner): S,
    headers: HeaderMap,
    Path((org, user)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(res) = caller(&inner, &headers) {
        return res;
    }
    inner.lookup_calls.fetch_add(1, Ordering::SeqCst);
    let d = inner.data.lock().unwrap();
    match member_info(&d, org, user) {
        Some(m) => Json(m).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn member_teams(
    State(inner): S,
    headers: HeaderMap,
    Path((org, user)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(res) = caller(&inner, &headers) {
        return res;
    }
    inner.lookup_calls.fetch_add(1, Ordering::SeqCst);
    let d = inner.data.lock().unwrap();
    if member_info(&d, org, user).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Ordered by name, as the platform answers; a deleted team is gone from the
    // answer even though the membership row may linger in the mock.
    let mut teams: Vec<TeamInfo> = d
        .team_members
        .iter()
        .filter(|(o, u, _)| *o == org && *u == user)
        .filter_map(|(o, _, t)| d.teams.get(&(*o, *t)).cloned())
        .collect();
    teams.sort_by(|a, b| a.name.cmp(&b.name));
    Json(MemberTeams { teams }).into_response()
}

#[derive(Deserialize)]
struct ByEmail {
    email: String,
}

async fn member_by_email(
    State(inner): S,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Query(q): Query<ByEmail>,
) -> Response {
    if let Err(res) = caller(&inner, &headers) {
        return res;
    }
    inner.lookup_calls.fetch_add(1, Ordering::SeqCst);
    let d = inner.data.lock().unwrap();
    let found = d
        .members
        .iter()
        .filter(|((o, _), _)| *o == org)
        .find(|(_, (u, _, _))| {
            u.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(q.email.trim()))
        })
        .and_then(|((o, u), _)| member_info(&d, *o, *u));
    match found {
        Some(m) => Json(m).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn team(
    State(inner): S,
    headers: HeaderMap,
    Path((org, team)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(res) = caller(&inner, &headers) {
        return res;
    }
    inner.lookup_calls.fetch_add(1, Ordering::SeqCst);
    let d = inner.data.lock().unwrap();
    match d.teams.get(&(org, team)) {
        Some(t) => Json(t.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn team_by_slug(
    State(inner): S,
    headers: HeaderMap,
    Path((org, slug)): Path<(Uuid, String)>,
) -> Response {
    if let Err(res) = caller(&inner, &headers) {
        return res;
    }
    inner.lookup_calls.fetch_add(1, Ordering::SeqCst);
    let d = inner.data.lock().unwrap();
    match d
        .teams
        .iter()
        .find(|((o, _), t)| *o == org && t.slug == slug)
    {
        Some((_, t)) => Json(t.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Parsed `DateTime` re-export so tests need not depend on chrono directly for it.
pub type Timestamp = DateTime<Utc>;

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        org: Uuid,
        at: DateTime<Utc>,
        meter: Option<&str>,
        quantity: Option<u64>,
    ) -> UsageEvent {
        UsageEvent {
            event_id: Uuid::new_v4(),
            org_id: org,
            user_id: None,
            tool: "complete_job".into(),
            billable: meter.is_some(),
            occurred_at: at,
            service: None,
            meter: meter.map(str::to_string),
            quantity,
        }
    }

    /// Only this org's metered events from this UTC month count, summed per
    /// meter: what the enforcement and reporting tests read back.
    #[test]
    fn meters_sum_this_months_shipped_quantities_per_org() {
        let (org, other) = (Uuid::new_v4(), Uuid::new_v4());
        let now = "2026-10-20T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let last_month = "2026-09-30T23:59:59Z".parse::<DateTime<Utc>>().unwrap();
        let jobs = "otto-tower/jobs_completed";
        let counted = vec![
            event(org, now, Some(jobs), Some(1)),
            event(org, now, Some(jobs), None),
            event(org, now, Some(jobs), Some(3)),
            event(org, now, None, None),
            event(org, last_month, Some(jobs), Some(100)),
            event(other, now, Some(jobs), Some(50)),
        ];

        let meters = shipped_meters(&counted, &[], org, now);

        assert_eq!(meters.len(), 1, "{meters:?}");
        assert_eq!((meters[0].meter.as_str(), meters[0].quantity), (jobs, 5));
        assert!(shipped_meters(&counted, &[], Uuid::new_v4(), now).is_empty());
    }

    /// A declared meter shows its declared name and unit; an undeclared one the
    /// key and no unit.
    #[test]
    fn meters_use_the_declared_display_name_and_unit() {
        let org = Uuid::new_v4();
        let now = "2026-10-20T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let counted = vec![
            event(org, now, Some("a/declared"), Some(2)),
            event(org, now, Some("a/undeclared"), Some(1)),
        ];
        let declared = [MeterDeclaration::new("a/declared", "Declared", "thing")];

        let meters = shipped_meters(&counted, &declared, org, now);

        assert_eq!(meters.len(), 2, "{meters:?}");
        assert_eq!(
            (meters[0].display_name.as_str(), meters[0].unit.as_str()),
            ("Declared", "thing")
        );
        assert_eq!(
            (meters[1].display_name.as_str(), meters[1].unit.as_str()),
            ("a/undeclared", "")
        );
    }
}
