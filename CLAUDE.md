# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

**otto-tower** is a hosted, multi-tenant MCP server for coordinating agentic coding work
across enterprises and teams. Server-only — no TUI, no PTY, no local binary, no plugin.

It is a **resource server of the otto platform** (`savvagent/otto-platform`). The platform
is the OAuth authorization server and the system of record for identity (accounts, orgs,
teams, members), SSO, and billing; otto-tower holds only the coordination domain, in its
own database, and asks the platform who a token belongs to. Nothing in this repository
issues a token, signs anyone in, or stores who a person is.

Read [`docs/specs/2026-09-01-otto-factory-design.md`](docs/specs/2026-09-01-otto-factory-design.md)
before any non-trivial change. The build order is in
[`docs/plans/2026-09-01-milestone-1.md`](docs/plans/2026-09-01-milestone-1.md).

## Naming: otto-tower, and the identifiers that keep the old name

The product was renamed from otto-factory to **otto-tower** (epic `savvagent/otto-platform#81`;
design of record `docs/specs/2026-10-21-otto-tower-rename-design.md` in otto-platform). Everything a
person or agent reads says otto-tower: the MCP server name (`mcp__otto-tower__*`), the canonical
endpoint `https://otto-tower.savvagent.com/mcp`, the console, the docs, the skills and the meter
(`otto-tower/jobs_completed`). `git grep -i otto-factory` is expected to find only these deliberately
kept identifiers, and a new hit outside the list is a bug:

- The `of-*` crates, the `OF_*` env vars (including `OF_LEGACY_HOSTS` / `OF_LEGACY_SUNSET`), the
  `otto_factory` database and the `factory` Postgres role.
- The Fly app `otto-factory-mcp` and its `fly.dev` host.
- The Rust type `Factory`.
- `savvagent/otto-factory#NNN` issue references, and the repo slug `savvagent/otto-factory` itself
  until the GitHub repo is renamed (a separate human step).
- The tracker trigger label `otto-factory` as stored data on bindings created before
  `savvagent/otto-factory#233` (new bindings default to `otto-tower`; no migration rewrites stored
  rows), and in the tests that set it explicitly.
- The Cloudflare Worker script names (`otto-factory-console`, `otto-factory-console-dev`).
- `CHANGELOG.md`, and the dated historical `docs/specs/*` and `docs/plans/*` (including the design
  of record's filename `2026-09-01-otto-factory-design.md`, which links depend on).
- The legacy host `otto-factory.savvagent.com` where it appears as deprecation config or in the
  cutover runbook (`docs/deploy/fly.md`). It is the legacy/deprecation path only; user-facing
  docs and `ci.yml`'s `environment.url` link to `otto-tower.savvagent.com`.
- The legacy file name `~/.claude/otto-factory-repos`, still read as a fallback by
  `client-skills/claude-code/session-start-hook.sh` and documented in `client-skills/claude-code/README.md`.
- Historical session records in `client-skills/claude-code/README.md` (the verification notes keep the
  repo slug and idempotency-key examples they were recorded with).
- The `factory-scanner:` key prefix, still recognised for back-compat.
- The `x-otto-factory-auth` OpenAPI extension key, emitted by the server and read by the console.
- The applied migration `0001_baseline.sql` (its comments and the `trigger_label` default): an
  applied migration is never edited.
- The old meter key `otto-factory/jobs_completed` (and its `otto-factory/` prefix) where it names
  what is being renamed away from: `0006_meter_otto_tower.sql`, `crates/of-core/tests/meter_rename.rs`,
  `of_billing::RETIRED_METERS` with its doc example and `crates/of-billing/tests/outbox.rs`, and the
  runbook's preflight and rollback in `docs/deploy/fly.md`.
- The old `otto-factory-worker:trio-cleared:` marker named in `.github/skills/otto-tower-worker/SKILL.md`'s
  transition note (it says that marker no longer counts).
- In the cutover runbook (`docs/deploy/fly.md`): the platform's service slug `otto-factory` that
  `rename-service` changes (step 2, the preflight in step 3, and the resource registration notes),
  this repository named by its current name in the runbook's prose ("Release and deploy
  otto-factory", "the otto-factory flip PR"), and the customer instruction
  `claude mcp remove otto-factory`, which names what customers registered the server as.
- Sentences recording the rename itself ("renamed from otto-factory"), here, in `docs/deploy/fly.md`
  and in `docs/deploy/cloudflare.md`, and this list.

## The three constraints

These decide scope disputes. When a change conflicts with one of them, the change is wrong.

1. **Coordination is anchored on repos.** A repo is a first-class entity, not a config
   string. Jobs belong to repos (`repo_id` is `NOT NULL`); leases are repo-scoped; an
   unresolvable repo is an error naming the registered slugs, never a silent fallback.
   (Messages go to a repo's channel or a team's; a team channel carries a team's coordination
   across repos and is not a precedent for a job or lease without a repo.)
2. **Substrate, not workflow.** otto-tower ships no opinion about how work is specified,
   planned, reviewed, or measured. If a capability could live either in the server or in a
   customer's own skill calling the server, **it belongs in the skill**. Jobs carry an
   opaque `metadata` JSONB field for exactly this reason — the server never interprets it.
3. **Coding-agent agnostic.** Claude Code, Copilot CLI, Cursor, Codex and anything else
   speaking MCP are equally first-class. Never depend on a specific client's hook, plugin,
   or skill system; never validate `agentType` against a list; never add a client-specific
   tool annotation. If a feature only works in one agent, it does not ship.

## Tenant isolation — the rule that outranks convenience

One org must never see or touch another's data. Two independent guards, and **both** are
required for any new tenant table:

1. **API shape.** Tenant data is reachable only through `Tx`, which cannot be constructed
   without an `OrgId`. Every statement carries `org_id = $1` explicitly, even though RLS
   would also filter it — the predicate keeps plans index-friendly and intent legible.
2. **Row-level security.** `Db::begin` issues `SET LOCAL ROLE otto_app` **and**
   `SET LOCAL app.org_id`. Both matter. Postgres exempts superusers and table owners from
   their own RLS policies, and the connecting user is frequently one or both, so **without
   the `SET LOCAL ROLE` the policies do nothing at all**. This was verified empirically,
   not assumed.

   The role is issued *only when it can be assumed*, because `CREATE ROLE` needs a
   cluster-level privilege that managed Postgres does not hand out — on Fly's managed
   cluster `otto_app` cannot be created at all. There, every tenant table being
   `FORCE ROW LEVEL SECURITY` carries the same guarantee: FORCE applies the policies to
   the table's owner, and the connecting role is neither a superuser nor `BYPASSRLS`.
   **Nothing assumes which of the two shapes it is in.** `Db::verify_tenant_isolation`
   reads it back out of the catalog as the role a tenant transaction actually runs as,
   and `of-server` refuses to bind a port unless one of them holds. Guard 2 is the one
   guard the *environment* can switch off, so it is the one guard that gets checked at
   startup rather than trusted.

When you add a tenant table: give it a `NOT NULL org_id`, add it to the `tenant_tables`
array in the migration that creates it (the baseline's is in `0001_baseline.sql`), and add a
cross-org negative test. A tenant-scoped function
without a cross-org negative test is not done. The policy must be named
`<table>_tenant_isolation` — `verify_tenant_isolation` discovers tenant tables by that
convention rather than from a list it would have to be told about, so a differently named
policy is a table the startup check will not vouch for.

**Testing WebAuthn needs a real authenticator.** `webauthn-authenticator-rs`'s software
tokens do **not** support resident keys (`SoftToken` says "These will be supported in
future"), so `tests/passkeys.rs` softens the challenge it hands the fake authenticator and
names the credential in `allowCredentials`. Only what the fake sees is softened; every
server step is the production one. What that cannot cover — a browser finding a credential
unprompted — needs a CDP virtual authenticator, which is how the flow was actually verified.

**A privilege granted to `otto_app` is not a protection.** `otto_app` does not exist on
managed Postgres, so `REVOKE … FROM otto_app` protects nothing there. Express the rule as a
policy instead: `audit_events` is append-only because it has no `UPDATE` policy, which
under FORCE binds the table's owner too — strictly stronger than the grant it replaced,
and it survives both deployment shapes. `#[sqlx::test]` connects as a superuser and
bypasses RLS, so a test of such a policy **must** `SET LOCAL ROLE otto_app` explicitly or it
passes against no policy at all.

**A migration that touches a tenant table's data needs its own `org_id` scoping — RLS is not
available to supply one.** `of_core::migrate` runs every migration statement on the connection pool
directly, never through `Db::begin`: `app.org_id` is never set and `otto_app` is never assumed.
What that means depends on which side of guard 2 the connecting role is on. If it is a
superuser or has `BYPASSRLS` — this deployment's actual shape today, per `docs/deploy/fly.md`
— RLS does not apply at all, `FORCE` included; an unscoped `UPDATE`/`DELETE` against a tenant
table silently rewrites **every org's matching rows in one statement**, a cross-tenant write,
not a no-op. Otherwise RLS does apply — every table carrying a `<table>_tenant_isolation` policy
is `ENABLE`/`FORCE ROW LEVEL SECURITY` unconditionally (`0001_baseline.sql`), which holds whether the migrating role happens to own the table (where
`FORCE` is what removes its exemption) or not (where a non-owner has no exemption to begin
with) — and `current_org()` is NULL for the statement's entire lifetime, so
`org_id = current_org()` is never true and the statement silently matches **zero rows, for
every tenant**, forever. (`tracker_connection_index` is the one `org_id NOT NULL` table with no policy at all, deliberately
— the unauthenticated tracker webhook route has to resolve an org before one is known. It has no
second branch: an unscoped rewrite of it always hits the first outcome, on every deployment shape.
`usage_outbox`'s, `platform_events`' and `tracker_deliveries`' policies have two branches, `org_id = current_org() OR
current_org() IS NULL`, so org-less background code (the shipper, the event and delivery sweeps) can reach every
org's rows while a pinned transaction still sees only its own. `tracker_deliveries` keeps the second branch off its
insert side: separate `SELECT`/`DELETE` policies have it, and its `INSERT` policy is pinned-only
(`0011_tracker_deliveries_pinned_insert.sql`), because no org-less code inserts. A DELETE-only policy is not enough for
that: a DELETE's `WHERE` reads the rows it filters, so the table's SELECT policies apply to it too,
which is why `audit_events` has `audit_events_retention_read` beside `audit_events_retention`
(`0002_org_less_retention.sql`; `housekeeping_deletes_where_row_level_security_applies` runs it as
`otto_app`).)
`savvagent/otto-factory#70` is what the fallback shape produced for a since-squashed relabeling
`UPDATE`; under this deployment's actual (bypassing) role it produced the first outcome instead,
harmlessly, because that rewrite was genuinely meant to apply the same way to every org. There is no
`orgs` table to loop over any more (orgs are the platform's): iterate the distinct `org_id`s of a
domain table, as above.
`rls_scopes_a_migration_style_update_with_no_org_context` in `tests/isolation.rs` reproduces the
zero-rows outcome directly, as a non-owner role — which needs no `FORCE` to be bound — using the
real tenant tables, whose owner is this deployment's own connecting role.

The one pattern that is safe under every shape, for any migration that must rewrite existing
tenant-table data — org-agnostic or not: loop over every org and give the statement **both** an
explicit `org_id` predicate (what saves it when RLS is bypassed) **and**
`set_config('app.org_id', ..., true)` (what saves it when RLS applies) — a migration author
cannot know in advance which deployment will run it, so both together, not either alone.
`Db::migrate` runs raw SQL with no parameter binding, so this is plpgsql, not a bound query:

```sql
DO $$
DECLARE
  o uuid;
BEGIN
  FOR o IN SELECT DISTINCT org_id FROM repos LOOP
    PERFORM set_config('app.org_id', o::text, true);
    UPDATE tracker_bindings SET trigger_label = 'otto-factory'
      WHERE org_id = o AND trigger_label = 'dark-factory';
  END LOOP;
  -- Leaving `app.org_id` set past the loop would apply it to any later
  -- statement in this same migration file on an RLS-applying shape —
  -- restore it to unset, the same as a schema migration always starts.
  PERFORM set_config('app.org_id', '', true);
END $$;
```

Temporarily toggling `ALTER TABLE <table> NO FORCE ROW LEVEL SECURITY` / `... FORCE ROW LEVEL
SECURITY` around an unscoped statement is not recommended: it only helps when the migrating role
owns the table, and a forgotten restore is caught by `Db::verify_tenant_isolation` at the next
boot only on the fallback shape. `otto_app` being assumable (also true of this deployment today,
per `docs/deploy/fly.md`) is a separate fact from whether the connecting role bypasses RLS, and
it is the one that matters here: the startup check itself runs as `otto_app`, which owns nothing,
so `FORCE` is not load-bearing for that check either, and a forgotten restore is not caught
there at all. The loop above has no such gap. `TRUNCATE` and `COPY ... FROM` against a tenant
table must never appear in a migration, for related but distinct reasons: Postgres has no RLS
policy class for `TRUNCATE` at all, so neither guard ever covers it, and it silently wipes every
org's rows whenever RLS is bypassed; `COPY ... FROM` is refused outright whenever RLS applies,
**even with the correct `app.org_id` already set** ("`COPY FROM not supported with row-level
security`" — confirmed, the per-org loop above does not rescue it), so a migration cannot be
written to run it safely under every shape at all. Use per-org `INSERT`s (the loop above, with
`INSERT` in place of the `UPDATE`) for anything `COPY` would otherwise have done. A schema-only
change (`ALTER TABLE ... ADD COLUMN`, a new `DEFAULT`, an index) is unaffected — none of this
constrains a table's own schema, only what its rows may be read, written, or matched against.

Note that ordinary cross-org tests pass on the strength of guard 1 alone. The tests that
actually exercise RLS are the `rls_scopes_*` ones in `tests/isolation.rs`, which issue
deliberately **unscoped** SQL inside a pinned transaction. Keep that distinction — a test
suite that cannot tell the two guards apart cannot tell you when one has broken.

## Commands

```bash
podman compose up -d          # Postgres 16 on host port 15433
cp .env.example .env          # DATABASE_URL for sqlx, and the platform settings for `cargo run`
cargo test                    # everything
cargo test -p of-core --test isolation   # tenant isolation
cargo test -p of-core --test queue       # queue behaviour
cargo clippy --all-targets -- -D warnings
cargo fmt --all

cd web && npm install
npm run check                 # svelte-check + tsc over worker/ — the console's `cargo test`
npm run lint                  # prettier --check — the console's `cargo fmt --check`
npm test                      # vitest — the Cloudflare Worker's routing rule
npm run build                 # static bundle into web/build

cargo run -p of-server        # everything on one port, reading .env
podman build -t otto-tower .   # console stage + rust stage + slim runtime
```

`OF_PUBLIC_URL`, `OF_PLATFORM_URL`, `OF_INTROSPECTION_SECRET`, `OF_PLATFORM_WEBHOOK_SECRET`,
and `OF_ENCRYPTION_KEY` are required with no defaults; `.env.example` says why for each.
`OF_CONSOLE_CLIENT_ID` is optional (console sign-in answers `503` without it). Build
`web/` first or every console page answers `404` while the API works. `cargo test` needs only
`DATABASE_URL`: the platform is `of-testkit`'s in-process mock, never a live service.

Integration tests are `#[sqlx::test]` against a real Postgres — one fresh throwaway
database per test, migrations auto-applied. There are no mocks for the database, on
purpose: RLS, `FOR UPDATE`, `LISTEN`/`NOTIFY`, and enum round-tripping are the things most
likely to be wrong, and a mock cannot tell you about any of them.

The **platform**, by contrast, is mocked — over real HTTP. `crates/of-testkit`'s
`MockPlatform` is an axum server on a loopback port speaking the platform's wire format (it
reuses `otto-resource`'s own types, so the two cannot drift silently), and tests hand the code
under test the *real* `PlatformClient` pointed at it. It models audience (a token is active
only for the resource server it was minted for), outages (`set_down`), usage dedupe by event
id, and signed webhooks. A test about identity behavior belongs against it; a test about the
platform's own behavior belongs in `savvagent/otto-platform`.

## Development skills

This repo's own development skill (and anything else added alongside it) lives at
`.github/skills/`, not `.claude/skills/` — that stays the single source of truth, so it never
forks into per-agent copies that can drift. `.claude/skills` is a git-tracked directory symlink
into it, purely a discovery alias for Claude Code, which only reads project skills from
`.claude/skills/`. Adding a second agent's own discovery path later means another symlink beside
it, never a copy of the content. (On a Windows checkout without `core.symlinks` enabled, the link
checks out as a text file instead of resolving, and the skill silently isn't discovered — the
same as not having it at all, not a crash.)

## Architecture

One binary (`of-server`) mounts every HTTP surface on one port. The crates are a
compile-time layering discipline, not separate services.

| Crate | Responsibility |
|---|---|
| `of-core` | Domain + **all** SQL. No HTTP, no auth. Every tenant fn takes an `OrgId`. |
| (otto-platform) | Two crates from `savvagent/otto-platform`, pinned by one rev in the workspace `Cargo.toml`: `otto-tenant` (the pinned-transaction/RLS substrate, `Cipher`, the audit-trail pattern) and `otto-resource` (the resource-server client: introspection, usage shipping, member/team lookups, webhook verification). Nothing else — no `otto-core`/`otto-auth`/`otto-billing`, and no identity tables. Never call `otto_tenant::Db::migrate` on this database; use `of_core::migrate`. |
| `of-mcp` | `rmcp` Streamable HTTP server, tool surface, bearer middleware (introspects at the platform). |
| `of-billing` | The price list (`classify`), what a completed job records (`completion`), enforcement of the platform's billing block (`meter`), and the usage outbox + shipper. |
| `of-trackers` | GitHub App + JIRA clients, webhook ingest, two-way sync. |
| `of-web` | Console REST API (console session cookie or platform bearer token), console sign-in (`/auth/*`), tracker webhooks, the platform's lifecycle webhook. |
| `of-server` | Config, migrations, router assembly, the usage shipper task, graceful shutdown. |
| `of-testkit` | Dev-only. `MockPlatform`: the platform's resource-server API, in process. |

`web/` is the console UI — SvelteKit 2 / Svelte 5 runes / Tailwind v4, TypeScript strict —
built to static files that `of-server` serves beside `/api`. See `web/README.md`.

**Every SQL statement lives in `of-core`.** A query in `of-mcp` or `of-web` is a bug — it
bypasses the `Tx` pinning that guard 2 depends on. The one exception is the usage outbox:
its SQL lives in `of-billing` (`outbox.rs`), because the outbox row must be written in the
tool's own `Tx` by the crate that owns metering, and the shipper drains it across all orgs.
It still writes through the caller's pinned `Tx`. The same file also holds the shipper's org-less
statements and `rewrite_retired_meters`, the startup rewrite of queued rows on a renamed meter key
(per org, with both guards); nothing else in `of-billing` touches SQL.

## The MCP surface

`of-mcp` is the only crate a customer's agent talks to, and four conventions hold across
every tool in it:

- **The caller comes from the request, not the session.** An MCP session spans many HTTP
  requests; `require_bearer` asks the platform about the token on each one
  (`PlatformClient::introspect`, which caches a positive answer for 60 s) and puts the
  `Principal` in the request extensions, which the transport carries into the handler as
  `Extension<http::request::Parts>`. That 60 s is the whole revocation delay — do not cache a
  principal on the service.
- **A platform failure is a `503`, never a `401`.** `401` tells an agent its token is dead and
  sends it to re-authenticate against the very platform that is down. Only the platform
  *answering* "inactive" is a `401`. Anything else (transport error, 5xx, our own credential
  being rejected) is `503` with `Retry-After`. Identity questions a tool asks — whoami,
  resolving a message's team channel — **fail closed** the same way: an error, never a guess.
- **The org comes from the token and nowhere else.** No tool takes an org argument.
- **Every result is a one-field object** — `{"job": …}`, `{"jobs": […]}` — defined in
  `tools::out`. MCP requires `outputSchema` to be rooted at `object`, so a bare array is
  not a legal result, and the envelope can gain a field without breaking callers.
- **Descriptions are the documentation.** The reader is an LLM that has never seen these
  docs: say what the tool does, when to reach for it instead of a neighbour, and what the
  failure means. `tests/tools.rs` asserts the tool list and that every tool describes
  itself, so a tool that silently disappears fails a test rather than a customer.

## The console surface

`of-web` serves the console's REST API — repos, the queue, tracker connections, this
service's audit trail — plus two machine endpoints that authenticate by signature rather than
by token: tracker webhooks (`/webhooks/{provider}`) and the platform's lifecycle webhooks
(`/platform/webhooks`). It serves **no identity**: no `/oauth/*`, `/api/auth/*`, `/api/me*`,
members, invites, teams, SSO, tokens, or usage. Those are the platform's, and
`the_identity_surface_is_not_served_here` fails if one comes back. Conventions:

- **Two credentials, one identity.** A script sends a platform bearer token; the console in a
  browser holds a **session cookie** (`__Host-of_session`) that keys a platform token pair
  stored server-side in `console_sessions` (sealed with `OF_ENCRYPTION_KEY`; the cookie
  itself is only ever stored as a SHA-256). Both end in the same call: the platform
  introspecting an access token (`session::authenticate`, the one place a credential becomes
  an identity). A bearer, when sent, decides the request even if it is bad — it is never
  rescued by a cookie. Nothing downstream of `OrgCtx` knows which it was, except that a
  cookie session asking for another org is `401 org_session_mismatch` (not `404`: the person
  is signed in, just not to that org, and the console's answer is to sign in again with an
  `org_hint`).
- **Console sign-in is an OAuth client of the platform, and it is optional.** `GET /auth/login`
  (PKCE `S256` + `state` in a sealed, `HttpOnly`, ten-minute `__Host-of_oauth` cookie) → the
  platform's `/oauth/authorize` → `GET /auth/callback` (state checked in constant time, code
  redeemed server to server, access token introspected, session created, `next` honoured only
  as a same-origin path) → `POST /auth/logout` (revokes the refresh token at the platform, best
  effort). Every `/auth/*` failure is a redirect to `/?login_error=<code>` from a fixed set;
  nothing from the query string is reflected. `OF_CONSOLE_CLIENT_ID` unset means
  `503 console_login_disabled` and nothing else changes. `/auth` is an API prefix (JSON
  `404`s, never the SPA); `/oauth` still is too and is served by no one here.
- **Refresh is single-flight, because the platform rotates and punishes reuse.** Every use of
  a refresh token consumes it, and presenting a consumed one revokes the whole token family.
  `console::access_token` therefore refreshes under `SELECT … FOR UPDATE` on the session row,
  rechecks expiry after taking the lock (the loser of a race finds the winner's fresh token
  and refreshes nothing — across replicas, not just tasks), and runs in a spawned task so a
  client disconnecting mid-refresh cannot drop a rotation on the floor. `invalid_grant`
  deletes the session (`401`); a platform that cannot answer is `503` and the session is
  untouched.
- **Cookie-authenticated writes are CSRF-checked; bearer requests are exempt.** SameSite does
  not cover same-site siblings (`otto.savvagent.com` and `otto-tower.savvagent.com`), so a
  non-safe request carrying the session cookie and no bearer must have `Origin` equal to
  `OF_PUBLIC_URL`'s origin (else `Sec-Fetch-Site: same-origin`, else a same-origin `Referer`),
  or it is `403 cross_site_request` (`csrf.rs`, a port of the platform's). `/auth/logout` is
  included. A bearer is not an ambient credential, so it needs no such proof.
- **Authorization is an extractor, not a handler's first line.** `OrgCtx` resolves the
  caller (from the token), the `{org}` path segment (which must be the token's one org, by
  slug or id), and their role before any handler body runs; `require_admin()` and
  `require_scope()` narrow it. A handler that forgets is a handler that serves another
  tenant's data, and a type catches that where a review checklist does not. Scopes are checked
  as the MCP tools check them, so the console cannot do what an agent's token could not.
- **An org that is not the token's is `404`, never `403`.** A `403` on a real slug and a `404`
  on a fake one turns any token into a directory of who uses the product.
- **A team id earns its way onto a row, and fails closed.** `repos.team_id` null means
  org-wide, so an unchecked id is dangerous in both directions. `NewRepo`, `RepoPatch`, and
  `NewJob` take an `of_core::teams::VerifiedTeam`, whose only constructor asks the platform
  whether the team exists in the caller's org: unknown or foreign is a `404`, an unreachable
  platform is a `503`, and neither is ever read as "no team". Team *slugs* in query strings
  (`?team=`) resolve the same way. When the platform deletes a team, its repos stay scoped to the
  dangling id (a tombstone nobody matches) rather than going org-wide — see
  `of_core::platform_events`.
- **Team membership is the platform's, asked per caller, and it fails closed.** Owners and admins
  see every repo and job. Anyone else sees org-wide rows (null `team_id`) plus those of the teams
  `PlatformClient::member_teams` lists for them (`of_core::teams::TeamScope`, built by
  `OrgCtx::team_scope` on the console and `Factory::team_scope` over MCP, **before** the
  transaction opens). A platform that cannot answer is `503 platform_unavailable`, never "all
  teams"; a team id the answer does not list (unknown, foreign, deleted) matches nothing. A row of
  a team the caller is not on is `404` / `job_not_found` / `repo_unresolved`, and
  `resolve_repo_visible` keeps its slug out of the "registered repos" list too. A job is
  visible only if its own team **and** its repo's team are (`job_visible_sql`, `get_job_visible`),
  messages follow the same rule through their team, repo, and job **and** through `scope_teams`,
  the teams they were bound to when sent (thread included), which outlives a deleted job (a
  message goes to exactly one channel, a repo's or a team's; a team channel the caller is not
  on is `team_not_found`, the same as an unknown slug, and owners and admins read every team's);
  lease ids of hidden repos are `lease_not_held`, and counts, dependency lists, returned leases, and
  errors are computed or redacted for what the caller may see — a redacted error keeps its
  original `code()` (`Error::Redacted`). A write checks visibility with the repo row **held**
  (`hold_repo_visible`, `get_job_visible_held`, `repo_for_write`), so an admin's team reassignment
  cannot commit between the check and the write. A user the platform no longer lists as a member
  is `access_revoked`, never a member of no teams. Every tool or route that names a repo, a job, or a lease resolves it through the
  scope, writes included. Accepted, not closed: `watch` wakes on any activity in the org (a timing
  signal, it carries no data), and the per-org sequential job ids (`job-N`) reveal how many jobs
  exist. The MCP scope is built from a fresh `member` lookup (10s cache), not the introspected role
  (60s), so a demotion applies within seconds.
- **The platform's webhooks are idempotent and signature-first.** `/platform/webhooks` verifies
  `Otto-Signature` (HMAC over timestamp and raw body, replay-bounded) before parsing anything;
  a bad signature is `401` and does nothing. A handled event, a repeat (the event id is recorded in the same transaction as its effects, `platform_events`;
  a redelivery **re-runs** the idempotent clean-up to catch work that raced the first run), and an
  unknown event type are all `200`; a failure to apply is `5xx` so the platform retries. It is mounted at
  `/platform/webhooks`, not under `/webhooks/{provider}`, so a platform event can never be
  mistaken for a tracker one.
- **Tombstones close the cache window.** Introspection is cached for 60 s, so `org.deleted` and
  `member.removed` first write `deleted_orgs` / `removed_members` (the latter swept after minutes)
  and both HTTP surfaces refuse a tombstoned org or user on every request
  (`of_core::platform_events::revoked`; a database error there is a `503`). Only then does the
  clean-up run. A request already past that check can still be on its way to a transaction, so
  every request-path tenant transaction opens through `platform_events::begin_live` (`Factory::tx`,
  `OrgCtx::begin`), which holds the org's lifecycle advisory lock shared and re-checks the
  tombstones under it; the clean-up takes it exclusively. A write in flight finishes first and is
  purged with the rest; one that starts later is refused (`access_revoked`). **Never open a
  request's transaction with `Db::begin` directly.**
- **The router and the OpenAPI document are built from one list.** Adding a route means
  adding it to `catalog.rs` with its summary and description; `router()` mounts the list and
  `openapi::document` renders it. A route not in the catalog is not reachable, on purpose.

The console API is read-only over the queue, and a unit test
(`the_queue_is_read_only_over_the_console`) fails if a write ever appears under `/jobs`.
Every job write belongs to `of-mcp`: the agent doing the work is the only party that can
say when it is done, and a "mark complete" button would let a human put something into the
audit trail that they did not observe.

## `web/` — the console UI

> **Status.** Signs in through the platform (`/auth/login`) and talks to the current API.
> Everything identity-shaped — members, teams, SSO, usage, tokens, the account, switching
> org — is a link into the platform's console (`web/src/lib/platform.ts`, from the
> `platformUrl` that `GET /api/session` reports); the old identity pages and API calls are
> gone. A session is signed in to **one org**: `/o/{slug}` for another is a sign-in for that
> org (`/auth/login?org=`), and `$lib/login` refuses to repeat an attempt for the same org
> within a minute so a platform that signs the visitor into a different org cannot loop the
> tab. The paragraphs below still describe the older shape where they mention the account's
> locale, members, or the connect page.

Six things hold. The first explains the next four; the sixth stands on its own.

- **It is a single-page app for a security reason, not a performance one.** The session is
  an `HttpOnly`, `__Host-`-prefixed cookie, which browsers refuse to store unless it is
  `Secure`, has `Path=/`, and carries no `Domain` — so it is bound to one origin. A
  SvelteKit *server* rendering these pages would have to hold that credential to fetch on
  the user's behalf: a second process with the keys to every console session, for pages
  behind a login that cannot be cached anyway. `adapter-static` with an `index.html`
  fallback keeps the cookie in the browser and makes CORS a non-question. The same fact is
  why `vite.config.ts` *proxies* `/api`, `/auth`, `/oauth`, and `/.well-known` in development — a
  cross-port `fetch` would not carry the cookie, and no CORS header could rescue it.

- **The server's rules are mirrored, never re-implemented.** A `404` on an org renders as
  "no such organization" and never "you don't have access", because the API answers `404`
  for both cases on purpose. `OrgContext.isAdmin` hides buttons; `OrgCtx` is what refuses
  them. An invitation link opens a *page* that `POST`s — `/invite/{org}` — so a link
  preview following the URL burns nothing.

- **Nothing about the deployment is baked into the bundle.** The MCP endpoint and the
  grantable scopes are read from `/.well-known/oauth-protected-resource` at runtime. A
  hard-coded MCP URL is how a staging build ends up printing a connect command pointing at
  production.

- **Every coding agent gets the same shape.** `src/lib/clients.ts` is one table with one
  entry per client and two forms each (OAuth, access token). A bespoke wizard for one agent
  and a footnote for the rest is the first place constraint 3 would quietly break.

- **The console ships in six languages, and a new string costs six catalog entries.** Paraglide
  compiles `web/messages/{en,es,de,fr,it,hi}.json` into tree-shaken message functions — no
  runtime lookup, no SvelteKit server, which is what makes i18n compatible with the
  `adapter-static` rule above. `npm run check` runs `scripts/check-messages.mjs` first and fails
  on a key missing from any locale, a dropped `{placeholder}`, or the wrong plural categories,
  because **Paraglide silently falls back to the base locale for a missing key** — without that
  gate a half-translated release looks correct in development and reaches a customer as half a
  page in the wrong language. Plural categories are genuinely per-locale: `en`/`de`/`hi` take
  `one`/`other`, `es`/`fr`/`it` also take `many`.

  The locale is the account's (`users.locale`, source of truth), cached in `localStorage` for
  first paint, and detected from `navigator.languages` when nothing was chosen — where `NULL`
  means "never chose", not "chose English". A change reloads the document, because `m.*()` are
  plain calls with nothing for Svelte to invalidate. `of_core::i18n` owns the locale list and a
  test reads `web/project.inlang/settings.json` to prove the two halves agree.

  **The MCP surface stays English.** `of-mcp` tool descriptions and `of-core` error messages are
  written for an LLM caller that has never read these docs; translating them fragments the one
  audience they have. Commands, config paths, product names and every wire value stay verbatim
  too — a translated `--transport http` is a broken command. `web/README.md` has the details.

- **A page that shows live state polls through `Poller`, never a bare `setInterval`.**
  `web/src/lib/poll.svelte.ts` holds six rules that are each easy to omit one at a time, and
  omitting any one makes a polling page worse than the static page it replaced: a refresh is not a
  load (no skeleton twice a minute); a failed refresh keeps the last good data and says how old it
  is; a failure the caller calls **fatal** — a `401`, a `404` — stops the poll instead of
  whispering, because a page still rendering data under a small warning is asserting access it no
  longer has; a hidden tab does not poll; refreshes never overlap and a failing one backs off; and a
  tab nobody has touched for hours parks itself, because `sessions.rs` slides the 14-day idle
  deadline forward on every authenticated request and an open console would otherwise hold a session
  open to the 90-day cap on an unattended desk. The console polls rather than consuming `of-core`'s
  change stream on purpose: a feed to the browser would mean a new console route and a held
  connection per open tab, for data that is a second stale at worst.

Runes throughout — `$state` / `$derived` / `$props` / `$effect`, no Svelte 4 stores, no
`export let`. Shared state lives in `.svelte.ts` modules (`session.svelte.ts`) or in
context (`org.svelte.ts`); the org is read from the route on every access rather than
copied into state, because a copy and the URL disagree for one frame after a navigation
and that frame is where one org's data renders under another's heading.

Org pages live under `/o/[org]`, not `/[org]`, so no org slug can collide with a page name.
The paths the *server* names — `/login`, `/verify`, `/recover` and `/invite/{org}` —
are fixed by what the server puts in an invitation link, and cannot be renamed here alone.
(The billing fix link is `OF_UPGRADE_URL` or the bare platform URL, so it names no console path.)

## `of-server` — assembly, and the two things only it can get wrong

One binary mounts every surface on one port. Nothing here has business logic; what it has is
the decisions no single crate could make.

- **Route collisions are a startup panic, so a test builds the router.** `Router::merge`
  panics on a path registered twice rather than choosing, and `the_whole_router_assembles`
  reaches that panic before a deployment does. `/.well-known/oauth-protected-resource` is
  `of-mcp`'s alone (it names the platform as the authorization server); `of-web` does not
  serve it.
- **The console SPA is the fallback, but not under `/api`, `/auth`, `/oauth`, `/mcp`,
  `/.well-known`, `/platform`, or `/webhooks`.** `index.html` answering an unknown path is what makes a hard refresh of a
  deep link work; `index.html` answering `/api/no/such/thing` with `200 text/html` is what
  makes an agent retry forever against a route that will never exist.
- **`/healthz` never touches the database and `/readyz` always does.** They answer different
  questions — "should this process be killed?" and "should traffic come here?" — and wiring
  liveness to the database turns a brief database blip into a simultaneous cold start of
  every replica. `/readyz` also reports the usage shipper (`usage.meters_declared`
  only; the cross-tenant outbox depth stays out of an unauthenticated body) as detail only: a stalled meter declaration never changes its status code,
  because every replica shares the platform's answer.
- **One `PlatformClient`, built once in `main` and shared.** It holds the introspection and
  usage-status caches, so a second client would be a second, colder cache. `of-mcp`, `of-web`,
  and the usage shipper all take the same `Arc`.
- **The usage shipper is a task of this binary** (`of_billing::outbox::run`, spawned in
  `main`), declaring the meters and then draining `usage_outbox` to the platform with per-row
  backoff. It stops on shutdown after a final flush, except while it is still waiting for the
  meter declaration: then it stops without one (shipping metered rows for an undeclared meter
  would dead-letter them). Anything undelivered stays in the table for the next process.
  An image that predates a meter rename (rolled back past `0006_meter_otto_tower.sql`, or still
  running beside a newer one during a deploy) keeps enqueuing the retired key, which would ship
  under an undeclared meter and be dead-lettered as `unknown_meter`. So the shipper maps every row
  it ships through `of_billing::outbox::current_meter` (`RETIRED_METERS`, beside
  `METER_DECLARATIONS`), and before it starts `main` runs `outbox::rewrite_retired_meters`, which
  moves the queued rows themselves, per org with both guards. A rename of a meter adds its pair to
  `RETIRED_METERS` as well as writing its migration.
- **Startup checks the platform credential but never requires the platform.** One harmless
  `usage_status` call logs whether the credential was accepted (a `REJECTED` line is a
  deployment fault), and a transport failure only warns: a platform outage must not stop a
  restart.
- **`Config::from_env` never falls back quietly.** A variable that is *set* but unparseable
  is a startup error naming it, not a default. `OF_PUBLIC_URL`,
  `OF_PLATFORM_URL`, `OF_INTROSPECTION_SECRET`, `OF_PLATFORM_WEBHOOK_SECRET`, and
  `OF_ENCRYPTION_KEY` have no defaults at all, because a wrong value for any of them fails
  silently: a discovery pointer that goes nowhere, tokens validated against the wrong
  audience, or webhooks nobody can verify. `Config`'s `Debug` prints no secret.
- **Graceful shutdown outlives the server.** `axum::serve(...).with_graceful_shutdown(...)`
  returns, and only then does `watcher.shutdown().await` run — see the trap below.

## A trap in tests: the change listener holds a connection

`Watcher::spawn` takes a connection out of the pool for `LISTEN` and **detaches** it, so
dropping the pool does not reclaim it. A `#[sqlx::test]` cannot drop its throwaway database
while a session is still attached, so a test that spawns a watcher and leaves it running
hangs at teardown rather than failing. `Watcher::shutdown()` stops the task and waits for
the connection to go; `Drop` does it best-effort. Anything that needs the connection
released before it continues — graceful shutdown, a test tearing down — calls `shutdown`.

## Migrations

Append-only, one file per concern, in `crates/of-core/migrations/`. Never edit a migration
that has been applied anywhere; add a new one.

**There is exactly one exception, and it is done.** `0001_baseline.sql` is a squashed,
domain-only baseline that replaced the original `0001`–`0034` history at the platform cutover
(`savvagent/otto-factory#192`): production data was disposable, so rather than carry the
identity/auth/billing interleaving (and a three-step role rename, `df_app` → `of_app` →
`otto_app`) the history was retired and the database recreated empty. The baseline creates
`otto_app` itself if missing, tolerating a concurrent creator and a missing `CREATEROLE`.
Nothing in CI enforces append-only mechanically (it is a convention), so there was no check to
relax; CI's role setup shrank to one optional `CREATE ROLE otto_app`.
`the_schema_holds_no_identity_tables_and_no_foreign_keys_to_them` (`tests/guards.rs`) keeps the
baseline's promise: no migration may create an identity table or reference one.

The schema has **no foreign keys to anything the platform owns** (orgs, users, teams): they are
in another database. Intra-domain keys (jobs → repos, and so on) are kept. Cleanup after a
platform-side deletion is the platform's lifecycle webhooks' job (`of_core::platform_events`),
not `ON DELETE CASCADE`'s.

## Authentication

There is none here; it is the platform's, and this service is its resource server. What that
means in this repository:

- **Tokens are opaque and validated by introspection** (RFC 7662): `POST /oauth/introspect` at the
  platform, authenticated by this service's registered credential (`OF_INTROSPECTION_SECRET`,
  HTTP Basic with `OF_RESOURCE_URI` as the user). The answer carries the user, the one org the
  token opens (fixed at issuance — it cannot be pivoted), the user's role in it *today*, and the
  scopes. A token for a different resource server, or whose user has left the org, comes back
  inactive. `PlatformClient` caches a positive answer for 60 s (never past the token's own
  expiry) and an inactive one for 5 s; failures are never cached.
- **The audience is configuration, not the request.** `OF_RESOURCE_URI` is what the platform
  mints tokens for; it is never derived from a `Host` header, which an attacker controls.
- **A legacy host changes what is advertised, never what is accepted.** During a rename's
  deprecation window `OF_LEGACY_HOSTS` names the old host(s). `of_mcp::legacy::mark`, `of-server`'s
  outermost layer, reads the origin host (`X-Forwarded-Host`, which the Worker sets, else `Host`),
  stamps `Deprecation`/`Link`/`Sunset` on every response through a legacy host, and leaves a
  `Legacy` marker that discovery, the `401` challenge and `initialize` read to name the legacy URL
  and the successor. The host only selects among configured values; introspection still asks as
  `OF_RESOURCE_URI` (`a_legacy_host_never_chooses_the_audience`). The console cannot work on a legacy
  host (`/auth/login` there would set its PKCE cookie on a host the platform never sends the browser
  back to, and its cookie writes would fail CSRF), so `of-server`'s `legacy_console_redirect`, just
  inside `mark`, answers a legacy-host request for exactly `/auth/login` or for anything that would
  reach the SPA fallback (not under `API_PREFIXES`, not a health probe) with a `308` to the same path
  and query on `OF_PUBLIC_URL`. `/mcp`, discovery, `/api`, the webhook receivers and
  `/platform/webhooks` are never redirected. Production is not behind the Worker, so this is the
  redirect that runs there; the Worker's own copy applies only to a Worker-fronted deployment.
- **Discovery.** `/.well-known/oauth-protected-resource` (RFC 9728) names the platform in
  `authorization_servers` and lists `of_core::scopes::KNOWN`; the `401` challenge points at it.
  This service serves no authorization-server metadata, registration, authorize, or token endpoint.
- **Registration is an operator step at the platform** (`otto-platform-server resource register`,
  `set-webhook`), not something this binary does at startup. The scope list registered there must
  match `of_core::scopes`. See `docs/deploy/fly.md`.
- **Identity reads go through `PlatformClient`**: `member` (whoami), `team` (team checks), and
  `team_by_slug` (the console's `?team=` and the message tools' team channels, through
  `VerifiedTeam::by_slug`). Each treats "the platform could not answer"
  as a refusal, never as an empty result.
- **Passkeys, recovery, SSO, invitations, account claiming, sessions** — all the platform's.
  `docs/clients/matrix.md` records what each MCP client sent against the pre-split authorization
  server; the redirect-URI rules it documents (`http://localhost:<port>/callback` must match
  ignoring the port) are the platform's to keep (`otto-auth`).

## Metering

The billed unit is a completed job, not a tool call. Every MCP tool is free, but each call is
still recorded and classified in `of-billing::classify`, so a price can be reintroduced without
losing history. Every completed job, through `complete_job` or an inbound ticket close, writes
exactly one completion row through `of_billing::record_job_completed`, in the same transaction
as the state change: billed (`otto-tower/jobs_completed`, quantity 1) or an unmetered,
unbilled row. Nothing else writes a completion row, so `org_counters.jobs_completed_total`
equals the number of completion rows (a parity test in `of-web/tests/webhooks_http.rs`). `watch` is a continuous long poll, which is why per-call
pricing was never viable.

The platform owns plans, standing, and monthly totals; this service owns the *record*. Four
rules hold, and the first is what makes the others true:

1. **The record is written inside the work's own transaction.** `Factory::charge` is the first
   thing after `self.tx(...)` and inserts a `usage_outbox` row (with a fresh `event_id`) in that
   transaction. `complete_job` instead records its completion, billed, through
   `Meter::charge_completion`. The ticket-close path (`of-web/src/routes/webhooks.rs`) records
   one completion row after `close_from_ticket` moves the job to Completed, in the webhook's
   transaction, billed only when the job was ever claimed (`attempts > 0`: claim history, not
   the status at the close, since `repend_job` and an expired claim return a worked job to
   `pending` but keep `attempts`) and the org is not blocked (rule 4); a never-claimed job's
   close is recorded unbilled. A failed call rolls the row back with everything
   else, so it is never billed; a successful one has no second transaction to retry, so it is
   never lost. A close on a job already Completed (a later event, or a job completed through
   `complete_job`) is a no-op and writes no second row. `watch` is the one exception — it meters in a short transaction of its own, because
   holding one open across a thirty-second poll would pin a connection per idle agent.
2. **Delivery is a background task, and it is idempotent.** `of_billing::outbox::run` first
   declares the meters in `METER_DECLARATIONS` with the platform (retrying until it succeeds;
   nothing ships meanwhile, since the platform rejects an undeclared meter as `unknown_meter`),
   then claims due rows (`FOR UPDATE SKIP LOCKED`, `next_attempt_at` as claim lease and retry
   backoff), posts them with `PlatformClient::ship_usage` (a batch refused as `unknown_meter`, say
   because the platform lost the declaration, is kept queued with capped backoff, never dead-lettered, and the meters are declared again on
   every pass that sees it, with an error log each time and no row text: losing billed usage is
   worse than a growing queue, and the stall is visible in the logs and `/readyz`), and deletes what the platform answered
   for (accepted, duplicate, or rejected — all final; a rejected event is moved to
   `usage_outbox_rejected` with the platform's reason, meter and quantity and an error log, never
   dropped, and a receipt whose counts do not add up to the batch deletes nothing). The platform
   dedupes on `event_id`, so any retry, from any replica, counts once. A platform outage delays
   billing and never loses it, and never affects a tool call. Every meter a row can carry must
   be in `METER_DECLARATIONS`.
3. **A new tool must be classified.** Every tool is free, and `exhaustive_over` compares the
   router against the price list; `every_tool_has_a_price` fails when they disagree. An
   unclassified tool is treated as free and logged.
4. **The hard stop applies to both completion paths, and a lookup failure never blocks work.**
   When the platform's usage status says billable work is blocked
   (`UsageStatus::billable_blocked()`: a `blocked_reason` of `budget_exceeded` |
   `payment_required`, or a tripped budget hard stop with no reason, treated as
   `budget_exceeded`):
   - `complete_job` is **refused** (`retriable: false`); the error names `OF_UPGRADE_URL`, else
     the bare platform URL.
   - A tracker close is **applied but recorded unbilled** (the tracker already closed the
     ticket). For a `payment_required` org that means **tracker closes are free**: an org with
     no card in good standing can still complete jobs by closing their tickets, unbilled.

   Free tools are never refused, so a blocked org can still read and find out why.
   The status lookup happens before the transaction: `complete_job` calls `Meter::warm` itself
   before `Factory::tx` and hands the result, a `Warmed` (only `warm` can make one), to
   `charge_completion`, which never looks anything up; the webhook looks it up only
   for a verified close to completed of a claimed job: it stops that pass without writing, drops
   the transaction, warms, then reopens one and applies again (`Factory::tx` does not warm, so
   free tools never wait on the platform, and an unverified request never causes a lookup). It is cached
   for 60 s (a failed lookup for 5 s), and a failed lookup lets the call through. A change made
   in the console therefore takes up to a minute to apply. `OF_ENFORCE_QUOTAS` is retired: it
   has no effect and the server warns if it is set.

## Style

- Errors are written for an LLM caller that has never read the docs: say what went wrong,
  what the valid options were, and what to call next. `Error::code()` is the stable
  machine-readable branch point; `retriable()` tells an agent whether to back off or
  rethink.
- Comments explain **why**, especially where the obvious implementation is wrong (see
  `Db::begin`, `normalize_remote`, `Watcher::wait`). Do not narrate what the code says.
- No `unwrap()` outside tests. No silent fallbacks on a resolution failure — errors that
  guess are worse than errors that stop.

## Releases & versioning

otto-tower is one product — a single deployed binary plus the console it serves — not a set of
independently published crates, so it carries one SemVer version, not seven. The workspace's
`[workspace.package] version` (every crate inherits it via `version.workspace = true`) and
`web/package.json`'s `version` move together.

The version, `CHANGELOG.md`, and the git tag + GitHub Release are computed automatically by
[release-please](https://github.com/googleapis/release-please) from commit history — nobody
hand-edits a version number or writes a changelog entry. Because that computation reads a commit's
*type*, not just its scope, the PR title (which becomes the squash-merge commit, per this repo's
merge convention) must be `<type>(<scope>): <subject>` — e.g. `fix(of-core): reap expired job
claims`, `feat(of-mcp): add a repo-scoped watch filter` — where `<type>` is one of `feat`, `fix`,
`perf`, `refactor`, `docs`, `test`, `build`, `ci`, `chore`, `revert`, and `<scope>` is the existing
crate/area vocabulary (a crate directory name, or `web`/`docs`/`ci`/`release`), omittable for a
change with no single honest scope. A `pr-title` CI check enforces this on every PR.

**A breaking change to a public interface gets its version bump from the same signal the
otto-tower-development skill's Non-Negotiable Rule 6 already requires you to raise there.**
Write `<type>(<scope>)!: <subject>` or add a `BREAKING CHANGE: …` footer, and release-please cuts
a major version from it — the same marker that tells the architect reviewer to look hard is what
tells the release automation to treat it as one.

`deploy` in `.github/workflows/ci.yml` runs only when release-please has just cut a release (i.e.
its auto-maintained "chore: release X.Y.Z" PR was just merged), not on every push to `master` —
see `docs/specs/2026-09-10-semver-release-design.md` §4.
