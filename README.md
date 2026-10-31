# Savvagent Tower

A hosted, multi-tenant **MCP server for coordinating agentic coding work** across
enterprises and teams.

Server-only: no TUI, no PTY, no local bridge binary, no plugin. A team member adds one
HTTPS endpoint to their coding agent, signs in through the browser once (at the
[Savvagent platform](https://github.com/savvagent/otto-platform), which owns accounts, orgs, teams,
SSO, and billing), and from inside
any registered git repository gets tools to add, claim, and complete jobs; to see what
every other agent on the team is working on and where; and to keep GitHub Issues / JIRA in
sync with that work.

```bash
claude mcp add --transport http savvagent-tower https://tower.savvagent.com/mcp
```

That is the entire client-side install.

Want your agent to register its own sessions automatically? See
[`client-skills/`](client-skills/README.md) for per-client templates, maintained by the
community rather than the server.

## Design principles

**It is about coordinating work, anchored on repositories.** A repo is a first-class,
org-owned entity, not a config string. Jobs belong to repos, agents announce which repo
they are in and which resource within it they hold (a branch, a staging slot, a migration
lock), and the primitives that stop two agents colliding are repo-scoped.

**It is a substrate, not a workflow.** Savvagent Tower deliberately does less than its
ancestor. It provides coordination primitives and ships no opinion about how work should be
specified, planned, reviewed, or measured. Customers encode their own methodology in their
own skills, commands, plugins, and subagents. When a capability could live either in the
server or in a customer's skill calling the server, it belongs in the skill.

**It is coding-agent agnostic.** Claude Code, Copilot CLI, Cursor, Codex, and anything else
that speaks MCP are all first-class. No dependence on any client's hook, plugin, or skill
system; free-form `agentType`; and a personal-access-token path for clients whose OAuth
support is incomplete.

Full design: [`docs/specs/2026-09-01-otto-factory-design.md`](docs/specs/2026-09-01-otto-factory-design.md).
Build order: [`docs/plans/2026-09-01-milestone-1.md`](docs/plans/2026-09-01-milestone-1.md).

## A resource server of the Savvagent platform

Savvagent Tower does not sign anyone in and does not store who anyone is. The **Savvagent platform**
is the OAuth 2.1 authorization server and the system of record for identity and billing;
Savvagent Tower is one of the *resource servers* it protects, and keeps only its own domain —
repos, jobs, leases, messages, trackers — in its own database.

```
  agent ──Bearer──► POST /mcp ──introspect (cached 60 s)──► platform
                       │  usage_outbox ──background shipper──► platform /internal/usage
  platform ──signed webhook──► POST /platform/webhooks   (org.deleted, team.deleted, member.removed)
```

- **Tokens** are opaque and validated by RFC 7662 introspection. A platform outage is a `503`,
  never a `401`.
- **Identity reads** (who am I, is this email a member, does this team exist) go to the
  platform and fail closed: an unknown or unreachable answer is a refusal, never "org-wide".
- **Metering** writes an outbox row in the tool's own transaction and ships it later;
  quota checks read the platform's cached usage status.
- **Cleanup** after the platform deletes an org, team, or member is driven by its signed
  lifecycle webhooks, since there are no foreign keys across databases.

Registering this service with a platform, and the one-time cutover, are in
[`docs/deploy/fly.md`](docs/deploy/fly.md).

## Status

Milestone 1, tasks 2–12 of 13 complete. `of-server` binds a port and serves every surface
on it, and two real coding agents have coordinated on one queue through it — see
[`docs/clients/matrix.md`](docs/clients/matrix.md). What remains of Milestone 1 is the
first live deploy (task 13) and CI (task 1). Milestone 2 (GitHub App + JIRA two-way sync,
plus the console UI for it) is complete —
[`docs/plans/2026-09-03-of-trackers.md`](docs/plans/2026-09-03-of-trackers.md).

| Crate | State |
|---|---|
| `of-core` | ✅ repos, jobs, leases, messages, change-watch, platform lifecycle events |
| `of-mcp` | ✅ Streamable HTTP MCP, 27 tools, platform-introspected bearer auth |
| `of-billing` | ✅ price list, quota policy, usage outbox + shipper |
| `of-trackers` | ✅ GitHub App + JIRA two-way sync (milestone 2) |
| `of-web` | ✅ console API (session cookie or platform bearer token), console sign-in, tracker console, platform webhooks |
| `of-server` | ✅ config, startup migrations, router assembly, health, usage shipper |
| `of-testkit` | ✅ in-process mock of the platform, for tests |
| `web/` | ✅ SvelteKit 2 / Svelte 5 console — signs in through the platform |

> The console signs in through the platform (OAuth authorization code + PKCE) and keeps its
> own session cookie; members, teams, SSO, usage, and tokens are managed at the platform,
> which the console links to. Sign-in needs `OF_CONSOLE_CLIENT_ID`
> ([`docs/deploy/fly.md`](docs/deploy/fly.md)); agents (`/mcp`) never do.

## Tenant isolation

The product's central claim is that one org cannot see or touch another's data. It rests on
two independent guards, and both are tested:

1. **API shape.** Tenant data is reachable only through `Tx`, which cannot be constructed
   without an `OrgId`, and every statement carries `org_id = $1`.
2. **Row-level security.** Every tenant transaction opens with `SET LOCAL ROLE otto_app` and
   `SET LOCAL app.org_id`. A query that forgets its predicate returns nothing rather than
   leaking.

The `SET LOCAL ROLE` is the non-obvious half and the reason guard 2 works at all: Postgres
exempts **superusers and table owners** from their own RLS policies, and the connecting user
is frequently both. `tests/isolation.rs` proves each guard separately — the two
`rls_scopes_*` tests issue deliberately unscoped SQL inside a pinned transaction and fail if
RLS is not in effect. Verified by removing `SET LOCAL ROLE` and confirming exactly those two
tests go red while the other eight stay green.

## Local development

```bash
podman compose up -d                  # Postgres 16 on host port 15433
cp .env.example .env                  # DATABASE_URL for sqlx (and the platform settings, to run the server)
cargo test                            # unit + integration tests; the platform is an in-process mock
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

Integration tests are `#[sqlx::test]`: each gets a fresh throwaway database with migrations
applied. Port 15433 is deliberately non-standard so it cannot clash with a system Postgres
or with dark-agent's container on 15432.

```bash
cargo test -p of-core --test isolation   # tenant isolation only
cargo test -p of-core --test queue       # queue behaviour only
```

The console has its own gate, which is the same two checks in the other language:

```bash
cd web
npm install
npm run check     # svelte-check, strict
npm run lint      # prettier --check
npm run build     # static bundle into web/build
```

`npm run dev` proxies `/api`, `/oauth`, and `/.well-known` to `OF_API_ORIGIN` (default
`http://127.0.0.1:8080`). See [`web/README.md`](web/README.md).

### Running the server

```bash
cargo run -p of-server
```

It reads `.env`, applies migrations, and serves everything on one port (it needs a reachable
platform to authenticate anyone; a local one is `otto-platform-server` from
[`savvagent/otto-platform`](https://github.com/savvagent/otto-platform)):

| Path | Surface |
|---|---|
| `/healthz` | Liveness. Never touches the database. |
| `/readyz` | Readiness. Probes the database; `503` when it cannot. |
| `/api/…` | Console REST API (`/api/openapi.json` describes it). Console session cookie or platform bearer token. |
| `/auth/…` | Console sign-in: `login`, `callback` (OAuth code + PKCE against the platform), `logout`. |
| `/webhooks/{provider}` | Tracker deliveries (GitHub, JIRA), provider-signed. |
| `/platform/webhooks` | The platform's lifecycle events, `Otto-Signature`-signed. |
| `/.well-known/oauth-protected-resource` | Discovery; names the platform as the authorization server. |
| `/mcp` | The MCP endpoint. Bearer tokens only, introspected at the platform. |
| everything else | The console SPA, with an `index.html` fallback. |

`OF_PUBLIC_URL`, `OF_PLATFORM_URL`, `OF_INTROSPECTION_SECRET`, `OF_PLATFORM_WEBHOOK_SECRET`,
and `OF_ENCRYPTION_KEY` are required and have no defaults, because a wrong value for any of
them fails silently rather than loudly — see the comments in `.env.example`.
`OF_CONSOLE_CLIENT_ID` is optional: without it the server runs and console sign-in answers
`503 console_login_disabled`.
Run `npm run build` in `web/` first, or every console page answers `404` while the API
works perfectly.

## Deployment

```bash
podman build -t otto-tower .
```

One image: the console bundle is built by a `node` stage, the binary by a `rust` stage, and
both land in a `debian-slim` runtime that runs as a non-root user. No database is needed to
build it — every statement in `of-core` is a runtime `sqlx::query` rather than a `query!`
macro, so there is no compile-time schema check and no `.sqlx` offline data to keep current.

On Fly.io, [`fly.toml`](fly.toml) carries the non-secret configuration and the health check.
The rest are secrets:

```bash
fly secrets set \
  DATABASE_URL="postgres://…" \
  OF_ENCRYPTION_KEY="$(openssl rand -base64 32)" \
  OF_INTROSPECTION_SECRET="sav_rs_…" \
  OF_PLATFORM_WEBHOOK_SECRET="sav_whsec_…"
fly deploy
```

Migrations run at startup under a Postgres advisory lock, so several machines booting
together is safe: the losers wait rather than racing through the same DDL.

The last two secrets come from registering this service at the platform
(`otto-platform-server resource register` and `resource set-webhook`), and `fly.toml` sets
`OF_PLATFORM_URL`, `OF_PUBLIC_URL`, and `OF_RESOURCE_URI`. Two of those are easy to get
subtly wrong:

- **`OF_RESOURCE_URI`** is the audience every token is bound to, and it must be *exactly* the
  `resource_uri` registered at the platform: it is also the HTTP Basic user on every call this
  service makes there. It is not derived from the `Host` header on purpose — that header is
  attacker-controlled, and an audience derived from one is not an audience check.
- **`OF_PUBLIC_URL`** is the origin of the discovery pointer in a `401` and of the tracker OAuth
  callbacks.

The one-time cutover from the pre-split deployment (empty database, new baseline, register the
resource, set the secrets) is a checklist in [`docs/deploy/fly.md`](docs/deploy/fly.md).

## Repository layout

| Path | What it is |
|---|---|
| `crates/of-core` | Domain + all SQL. Every tenant operation takes an `OrgId`. |
| `crates/of-core/migrations` | Append-only schema, from a domain-only baseline. |
| `crates/of-mcp` | MCP server and tool surface. |
| `crates/of-billing` | The tool classification, the blocked-org policy, and the usage outbox/shipper. |
| `crates/of-trackers` | GitHub App + JIRA sync. |
| `crates/of-web` | Console REST API, tracker and platform webhooks. |
| `crates/of-server` | The binary: config, migrations, router assembly, health, usage shipper. |
| `crates/of-testkit` | Dev-only mock of the platform's resource-server API. |
| `Dockerfile`, `fly.toml` | The image and its Fly.io deployment. |
| `web/` | SvelteKit 2 + Svelte 5 console. |

## Metering

**You pay for outcomes, not for looking or coordinating.** Every tool call is free, including
`watch`, the 30-second long poll every connected agent calls continuously. What is billed is a
completed job: a successful `complete_job`, or a linked ticket closed as done in the tracker,
records one `otto-tower/jobs_completed` event. Every call is still recorded so pricing can
change later without losing history.

Events are written to a local outbox in the same transaction as the work (so a failed call is
never billed) and shipped to the platform, which owns plans and totals, by a background task that
can retry forever without double-counting. When the platform reports an org blocked
(`budget_exceeded` or `payment_required`), `complete_job` is refused with a link to fix it
(`OF_UPGRADE_URL`, else the platform URL); everything else keeps working. If the platform cannot
be asked, the call goes through. Console changes take up to a minute to apply.

## Relationship to dark-agent

`dark-agent` is the single-organization ancestor — a TUI hosting a `claude`
PTY plus a queue server authenticated by AWS SigV4 with an IAM-ARN allowlist. Savvagent Tower
takes the server's ideas (job lifecycle, atomic claim, dependency graph, `LISTEN`/`NOTIFY`
watch, message channel, repo registry), re-tenants them, and drops the rest. It is not a fork
and shares no code.
