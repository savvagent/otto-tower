# Hosting: Fly.io

> **Naming.** The product is **Savvagent Tower** (formerly otto-tower, before that otto-factory). The
> Fly app `otto-factory-mcp`, its `fly.dev` host, the `otto_factory` database and the `OF_*` variables
> keep their old names on purpose (phase 4, savvagent/otto-tower#280). The canonical host is
> `tower.savvagent.com`, reached by [Runbook: move to tower.savvagent.com](#runbook-move-to-towersavvagentcom);
> until you have run it, the live host, resource URI, console client and secrets are still the old
> ones. [The rename cutover](#the-rename-cutover) below is the earlier otto-factory to otto-tower
> sequence, kept as the historical record.

Savvagent Tower runs as a single persistent machine on Fly.io — no scale-to-zero, because
`Watcher::spawn` (tower-core) holds a detached `LISTEN` connection that a request-scoped
serverless model would fight. See `CLAUDE.md` for why the process must stay warm.

## Topology

Savvagent Tower is a **resource server** of the Savvagent platform (savvagent/otto-factory#192).
Two Fly apps, two databases on one Postgres instance, one direction of dependency:

```
  otto-platform   (https://savvagent.com)        otto-factory-mcp  (https://tower.savvagent.com)
  OAuth AS, login, passkeys, orgs, teams, billing     POST /mcp       bearer tokens, introspected at the platform
  db: otto_platform  ◄────────── same instance ─────► db: otto_factory  repos, jobs, leases, messages, trackers
        ▲   POST /oauth/introspect, /internal/*  ◄────────────────  per request (cached 60 s)
        │   POST /internal/usage                 ◄────────────────  background shipper (usage_outbox)
        └─► POST /platform/webhooks  (org.deleted, team.deleted, member.removed) ──────────────►
```

Savvagent Tower's database holds **no identity**: no users, orgs, teams, memberships, tokens,
sessions, plans, or usage totals, and no foreign key to any of them. Every `org_id`,
`team_id`, and user id in it is a uuid the platform issued.

## Org and infra (provisioned)

- **Org**: `savvagent`.
- **App**: `otto-factory-mcp`, freshly created rather than renamed in place — Fly does
  not support renaming an app's slug, and the previous `dark-factory-mcp` app predates
  this rename.
- **Database**: database `otto_factory` on `otto-db` (region `iad`), a standalone
  (unmanaged) Fly Postgres app **shared with the platform**, which keeps its identity
  data in database `otto_platform` on the same instance.
  - Unmanaged because the migration creates the `otto_app` role, which needs `CREATEROLE`
    that the managed `savvagent-pg` cluster (used by `nels-api`) does not grant to a
    tenant app's own role.
  - Shared for cost. Each app's attach role is a **superuser on the whole instance**, so
    either app's credential can reach the other's database. Split onto dedicated
    instances before real customers.
  - Connecting role: the one `fly postgres attach` created for this app (a
    **superuser**).
  - `DATABASE_URL` is staged as an app secret.
- **Migrations are Savvagent Tower's own.** `tower_core::migrate` applies
  `crates/tower-core/migrations` to `otto_factory`. It must never call
  `savvagent_tenant::Db::migrate`, which applies the platform's own, differently numbered
  history (`tests/guards.rs` fails the build if anything does).

### Secrets

`fly secrets set` (never in `fly.toml`):

| Secret | What it is | Where it comes from |
|---|---|---|
| `DATABASE_URL` | Postgres URL for `otto_factory` | `fly postgres attach` |
| `OF_ENCRYPTION_KEY` | 32 bytes base64; seals tracker credentials at rest | `openssl rand -base64 32`. Fed unchanged to `savvagent_tenant::crypto::Cipher`. |
| `OF_INTROSPECTION_SECRET` | `sav_rs_…`; authenticates every call this service makes to the platform | Printed once by `resource rotate-secret` (`resource register` issues none) |
| `OF_PLATFORM_WEBHOOK_SECRET` | `sav_whsec_…`; verifies the platform's lifecycle webhooks | Printed once by `resource set-webhook` |

Non-secret settings (`OF_PUBLIC_URL`, `OF_RESOURCE_URI`, `OF_PLATFORM_URL`,
`OF_CONSOLE_CLIENT_ID`, …) live in `fly.toml`'s `[env]`.

## Registering with the platform

This is the first-time setup for a new deployment, written for the Savvagent Tower URI. Production
was registered under the old URI before the rename; it is moved by *The rename cutover* below
(`resource rename-service`, `resource rename`), not re-registered.

The platform's authorization server validates every authorize, token, refresh, and PAT
request against a registered resource, and authenticates this service's calls with the
credential `resource rotate-secret` issues for it. Registration is an **operator step on the platform**
(it replaces the startup upsert the service used to do against its own database):

```bash
# On the platform (a machine with savvagent-platform-server and its OTTO_* environment):
savvagent-platform-server resource register https://tower.savvagent.com/mcp \
  --name "Savvagent Tower" \
  --scopes jobs:read,jobs:write,repos:read,repos:write,messages,trackers,org:admin \
  --default-scopes jobs:read,repos:read

savvagent-platform-server resource rotate-secret https://tower.savvagent.com/mcp
#   -> prints the introspection secret ONCE: set it as OF_INTROSPECTION_SECRET
#      (`register` issues no credential; `rotate-secret` is also how to replace it)

savvagent-platform-server resource set-webhook https://tower.savvagent.com/mcp \
  https://tower.savvagent.com/platform/webhooks
#   -> prints the webhook signing secret ONCE: set it as OF_PLATFORM_WEBHOOK_SECRET
```

The scope list is `tower_core::scopes::KNOWN` and the defaults are `tower_core::scopes::DEFAULT`;
keep them in step when a release adds a scope (the service's own check and its
`/.well-known/oauth-protected-resource` document read from the same constants, but the
platform's registered list is what it will actually issue). `OF_RESOURCE_URI` must be
**exactly** the URI registered here: it is the HTTP Basic user on every platform call and
the audience every token must carry.

At startup `tower-server` makes one harmless platform call and logs whether the platform
accepted its credential (`REJECTED this service's credential` means a wrong secret or
URI). It does not refuse to start on failure: a platform outage must not stop a restart
that would otherwise serve cached tokens.

## Registering the console's sign-in client

The console signs people in with an OAuth authorization-code + PKCE flow against the
platform, and then keeps its own session cookie (`__Host-of_session`) over a platform token
pair it holds server-side. For that the platform needs to know the console as an OAuth
client: a **public, first-party** client (no secret, no dynamic registration) whose only
redirect URI is `{OF_PUBLIC_URL}/auth/callback`. Like the resource registration above, this
is an operator step on the platform:

```bash
# Run on the platform's machine (it needs the platform's OTTO_* environment):
fly ssh console -a otto-platform -C \
  'savvagent-platform-server client register --name "Savvagent Tower console" \
     --redirect-uri https://tower.savvagent.com/auth/callback --first-party'
#   -> prints the client_id: set it as OF_CONSOLE_CLIENT_ID
```

The `client_id` is not a secret (a public client has nothing else to authenticate with, and
PKCE is what binds a code to the browser that asked for it), so it goes in `fly.toml`'s
`[env]`, beside `OF_PUBLIC_URL`. The redirect URI is registered **exactly**: a deployment
that moves to another hostname registers a new one.

`OF_CONSOLE_CLIENT_ID` is optional. While it is unset the server boots and serves everything
else, and `GET /auth/login` answers `503 console_login_disabled`; the console then shows
its sign-in failing with that message rather than redirecting somewhere that cannot work.
Bearer tokens (scripts, CI) and the MCP surface do not depend on it.

Sessions live in the `console_sessions` table (access and refresh tokens sealed with
`OF_ENCRYPTION_KEY`; the cookie itself is stored only as a SHA-256). Because the key seals
them, **rotating `OF_ENCRYPTION_KEY` signs every console user out**. The platform rotates a
refresh token on every use and revokes the whole login if a spent one is replayed, so
Savvagent Tower refreshes under a row lock and each refresh happens exactly once, even across
machines.

## Usage metering

Every tool call writes a row to `usage_outbox` in the tool's own transaction; a successful
`complete_job`, and a ticket close to Completed of a job that was ever claimed, carry the
`otto-tower/jobs_completed` meter (quantity 1), which is what is billed. A close of a
never-claimed job, or any close while the org is blocked, is recorded unbilled. A background task in `tower-server` first **declares the
meter** with the platform, then ships rows to `/internal/usage` in batches of up to 500, deleting
what the platform accepted (it dedupes on the row's `event_id`, so retries are safe). Nothing ships
until the declaration succeeds; it retries with backoff and logs an error about once a minute with
the platform's reason and the rows waiting. While the platform is unreachable rows accumulate and
nothing user-visible breaks; they drain, oldest first, when it returns. To watch the backlog:

```sql
SELECT count(*), min(occurred_at), max(attempts), max(last_error) FROM usage_outbox;
```

**Checking for a stalled declaration without the logs.** `GET /readyz` carries a `usage` object:

```json
{"status":"ready","usage":{"meters_declared":false}}
```

`meters_declared: false` means this process has not declared its meters, so it is shipping
nothing, free rows included. It is `false` for a moment after every start and is expected to flip
to `true` within seconds; `false` that persists is the stall. The backlog's depth is deliberately
not in this body (the route is unauthenticated and the figure spans every tenant): read it with the
`SELECT count(*) … FROM usage_outbox` above, or from the shipper's `rows_waiting=N` log line. Readiness stays `200` on
purpose: every replica shares the platform's answer, so failing the probe would take the API out
of rotation over a billing delay that loses nothing. Alert on the body (for example
`curl -s https://tower.savvagent.com/readyz | jq -e '.usage.meters_declared'` exits non-zero
when it is `false`), not the status code. Fly's check on `/readyz` is unaffected. The reason
(`service_not_set`, a refused meter) is in the logs.

A backlog that keeps growing with `last_error` mentioning `credential` means
`OF_INTROSPECTION_SECRET` is wrong. A declaration error `service_not_set` means the platform side
is not prepared: the operator prerequisite is the platform's `resource set-service` for this
resource server, which is already done for production (slug `otto-factory` until the cutover's
`resource rename-service` step renames it to `otto-tower`).

**The meter key's `otto-tower/` prefix must match the service slug the platform assigned to
this resource server.** The key is a constant (`tower_billing::METER_JOBS_COMPLETED`), not
configuration, so a deployment registered under another slug (a staging resource server set to
`otto-tower-staging`, say) has its declaration and its metered events refused. Set the slug to
`otto-tower` for every environment that runs this binary, or the meter does not bill there.

**Before rolling back across the release that added the meter** (migration `0005`), drain or
check the metered rows:

```sql
SELECT count(*) FROM usage_outbox WHERE meter IS NOT NULL;
```

The previous release's shipper does not read `meter`/`quantity`, so a metered row it ships goes
out as a plain call and its meter unit is lost. The same can happen during the brief overlap of
a rolling deploy (production runs one machine). Wait for the count to reach zero (the current
release ships them within seconds while the platform is up), then roll back. With no prices set
the cost today is zero, but the completed-job count is lost.

`OF_ENFORCE_QUOTAS` is retired and has no effect (the server warns if it is set); enforcement is
always on. `complete_job` is refused, and a tracker close is applied but recorded unbilled, when
the platform reports the org blocked (`budget_exceeded` or `payment_required`),
judged from a status cached for a minute, so a console change takes up to a minute to apply; if
the platform cannot answer, the call is allowed (and still recorded). The refusal points at
`OF_UPGRADE_URL`, else `OF_PLATFORM_URL`.

## Tenant isolation on the shared Postgres instance

The connecting role **is a superuser** (`fly postgres attach` creates it that way on an
unmanaged instance). The baseline migration creates the `otto_app` role if it is missing
and `GRANT otto_app TO CURRENT_USER`; both need `CREATEROLE`, which a superuser has
unconditionally. Expect at startup:

```
INFO tower_server: tenant isolation enforced as role "otto_app"
                (assumed via SET LOCAL ROLE); 12 tenant tables, 12 forced
```

So this deployment runs in the same configuration as local development and
`#[sqlx::test]`: `otto_app` exists, `Db::begin` issues `SET LOCAL ROLE otto_app` for
every tenant transaction, and the connecting superuser is never the role a request
actually runs as. `Db::verify_tenant_isolation` re-derives this from the catalog at
startup and `tower-server` refuses to bind a port otherwise.

**Roles are cluster-scoped, and `otto-db` is one cluster.** `otto_app` is a single role
shared by `otto_factory` and `otto_platform`; its grants are per database. Whichever
app migrates first creates it, and the other finds it present and only re-grants.

> An earlier version of this section described a managed-Postgres deployment where
> `CREATEROLE` was unavailable and isolation instead relied on `FORCE ROW LEVEL
> SECURITY` applying to a non-superuser table owner. That is not this deployment, but it
> remains the fallback path `tower-server` supports if this ever moves to managed Postgres.

## The rename cutover

The product was renamed from otto-factory to otto-tower (epic `savvagent/otto-platform#81`). The
code that ships the rename changes **no deployed value**: not `fly.toml`'s `[env]`, not the Worker
routes, certificates, DNS or the platform's registrations. Merging and deploying it ahead of the
cutover is safe. The cutover itself is a human-run sequence of production writes, and **the order
below is load-bearing**. Read it end to end before starting. Each of steps 3 to 9 ends with a
**Verify** line and a **Stop if** condition: if a stop condition holds, do not go on to the next
step (see *Rollback*). Placeholders:

```bash
OLD=https://otto-factory.savvagent.com/mcp     # the resource URI registered today
NEW=https://otto-tower.savvagent.com/mcp       # the canonical URI from here on
SUNSET=<RFC 3339 date, e.g. 2027-01-31T00:00:00Z>   # when the old host stops working; chosen by a human
```

Pick `SUNSET` before step 6 and publish it. Tokens already issued are rewritten to the new URI by
the rename, so they stay valid past the sunset; only clients that still *address* the old URL stop
working then.

### 1. Release and deploy otto-platform

The platform release carries the guarded service rename (`resource rename-service`), the
time-boxed URI alias (`resource rename`, `drop-alias`) and OAuth alias resolution. Nothing below
works without it. Check `savvagent-platform-server resource show $OLD` runs on the deployed binary (it is read-only; `resource describe` is not, since it replaces the consent-screen scope descriptions).

### 2. Release and deploy otto-factory (this repo)

The platform release from step 1 must be live first. Merging this repo's release PR deploys
automatically (the `deploy` job runs `flyctl deploy`). **This release must include the
`OF_LEGACY_HOSTS` / `OF_LEGACY_SUNSET` support (#231)**; step 8 sets those variables and an older
binary would ignore them. The new binary declares the meter `otto-tower/jobs_completed`. The platform
still has this resource server's service slug as `otto-factory`, so it **refuses the declaration**
(the key is outside the service's namespace). That is expected and nothing is lost: the shipper
retries with backoff and pauses shipping while usage rows stay queued in `usage_outbox`. Expect this
error to repeat about once a minute until step 4:

```
ERROR could not declare the usage meters; no usage ships until they are declared
      (rows stay queued and nothing is lost). Retrying   reason=… rows_waiting=N
```

The rest of the service is unaffected: MCP, the console and webhooks keep working on the old host.
Do not leave this state longer than needed, because the backlog only grows.

### 3. Preflight

`rename-service` is itself the authoritative guard: it refuses if any metered usage or Stripe
object exists under the old slug. This read-only check tells you beforehand, so you are not
surprised mid-sequence. It runs against the **platform's** database, `otto_platform`. Open a
console on it and paste the block as a whole (`fly postgres connect` takes no command flag; its
flags are `-a`, `-d`, `-u`, `-p`, `-c` for a config file). The connecting role is a superuser on
the whole instance, so the paste is wrapped in a read-only transaction that is rolled back: a
stray edit in the paste cannot write anything.

```bash
fly postgres connect -a otto-db -d otto_platform
```

```sql
BEGIN READ ONLY;
SELECT
  (SELECT count(*) FROM usage_events           WHERE meter LIKE 'otto-factory/%')       AS usage_events,
  (SELECT count(*) FROM org_meter_period_usage WHERE meter LIKE 'otto-factory/%')       AS org_meter_period_usage,
  (SELECT count(*) FROM stripe_meters          WHERE meter LIKE 'otto-factory/%')       AS stripe_meters,
  (SELECT count(*) FROM stripe_prices          WHERE meter LIKE 'otto-factory/%')       AS stripe_prices,
  (SELECT count(*) FROM stripe_meter_outbox    WHERE event_name LIKE 'otto\_factory\_\_%'
                                                OR event_name LIKE 'otto\_hfactory\_\_\_%') AS stripe_meter_outbox;
ROLLBACK;
```

(`event_name` is `otto_factory__<meter>`: the meter key with `-` turned into `_` and `/` into `__`.
A meter containing `__` uses the escaped form instead, `otto_hfactory___<meter with _ as _u>`; see
`otto-platform/crates/otto-billing/src/stripe/event_name.rs`.)
Also run, on the platform's machine. Both commands are read-only: `show` prints the service, its
declared meters, its aliases and the rename blocker counts, and `--dry-run` runs the real guard in a
transaction that is always rolled back. Do **not** use `resource describe` here: it replaces the
consent-screen scope descriptions.

```bash
savvagent-platform-server resource show $OLD                                  # service slug is otto-factory; blockers all 0
savvagent-platform-server resource rename-service $OLD otto-tower --dry-run   # reports it would rename, changes nothing; exits non-zero if refused or busy
savvagent-platform-server resource list                                       # $NEW is not registered or aliased yet
```

**Verify:** all five counts are 0; `show` gives the slug `otto-factory` and zero blockers; the dry run
says it would rename; `list` has no entry for `$NEW`.
**Stop if:** any count is non-zero or the dry run refuses (usage or Stripe data already exists under the old slug and
`rename-service` will refuse by design: resolve that at the platform first), or `$NEW` already appears.

### 4. Rename the billing service, immediately

```bash
savvagent-platform-server resource rename-service $OLD otto-tower
```

Do this straight after step 3 and before anything else, and only while usage shipping is paused
(it is, from step 2 until this step: the new binary's meter declaration is refused until the
slug changes). A batch the platform accepted mid-rename could otherwise land under the old slug. The shipper's next retry declares
`otto-tower/jobs_completed`, logs `declared usage meters`, and drains the queued rows (oldest
first). If the platform later forgets the meter and refuses events as `unknown_meter`, the
shipper never dead-letters them: it keeps those rows in `usage_outbox` with capped backoff,
declares the meters again on every pass that sees the refusal, and logs
`the platform refused usage events as unknown_meter` at error level each time (no row text). A
persistent refusal therefore grows the queue instead of losing billed usage; the repeating error
(and `/readyz`) is how you notice. Watch the logs (`fly logs -a otto-factory-mcp`) and the backlog. The backlog lives in
**this service's** database, `otto_factory`, not in `otto_platform` where step 3 ran:

```bash
fly postgres connect -a otto-db -d otto_factory
```

```sql
BEGIN READ ONLY;
SELECT count(*), min(occurred_at), max(attempts), max(last_error) FROM usage_outbox;
ROLLBACK;
```

**Verify:** the command succeeded; `declared usage meters` appears in the log; the count falls to 0;
re-running the step 3 query on the platform database (`otto_platform`) still gives five zeros.
**Stop if:** `rename-service` errors (do not continue; the platform changed nothing, so the old name
stays in force: find out why and retry), or the backlog does not drain / `last_error` stays
non-empty after a few minutes.

### 5. Set up the new host

Add the hostname without removing the old one: both serve until the sunset. Production is **not**
fronted by the Cloudflare Worker: both public names resolve straight to the Fly app, whose
certificate and DNS records are the whole of this step.

1. **Certificate.**
   ```bash
   fly certs add otto-tower.savvagent.com -a otto-factory-mcp
   ```
2. **Addresses.** Read them from Fly at the time you do this, never from a document (including
   this one): the app's shared IPv4 and its dedicated IPv6.
   ```bash
   fly ips list -a otto-factory-mcp      # v4 "public ingress (shared)", v6 "public ingress (dedicated)"
   ```
3. **DNS.** At the zone's **current** DNS provider (check with `dig +short NS savvagent.com`; it is
   Namecheap today), add an `A` record for `otto-tower.savvagent.com` to that IPv4 and an `AAAA`
   record to that IPv6. They must be the same addresses the old name resolves to
   (`dig +short otto-factory.savvagent.com A` / `AAAA`); if they differ, stop and find out why
   before going on.
4. **Only if the deployment is fronted by the Worker** (it is not today): instead of the records
   above, add `otto-tower.savvagent.com` as a custom domain on the production Worker
   (`docs/deploy/cloudflare.md`), keeping the old custom domain. The Worker's script name does not
   change.

**Verify:** `dig +short otto-tower.savvagent.com A` and `AAAA` give the `fly ips list` addresses;
`fly certs check otto-tower.savvagent.com -a otto-factory-mcp` reports the certificate as issued
(Ready); `curl -fsS https://otto-tower.savvagent.com/healthz` answers. Until step 8 the app still
treats the old host as canonical, so only the health probes are meaningful on the new host.
**Stop if:** the records do not resolve to the `fly ips list` addresses, the certificate stays
pending (DNS not propagated), or `/healthz` does not answer on the new name.

### 6. Rename the resource URI, keeping the old one as an alias

```bash
savvagent-platform-server resource rename $OLD $NEW --alias-until $SUNSET --name otto-tower
savvagent-platform-server resource list      # confirm: $NEW canonical, $OLD alias until $SUNSET
```

`--name` sets the name the consent screen shows. The rename also rewrites existing tokens and
authorization codes from `$OLD` to `$NEW`. From here the running app (still configured with
`OF_RESOURCE_URI=$OLD`) authenticates to the platform as the alias and keeps working with no
configuration change.

Run it at low traffic: it briefly holds the old and new URIs' usage-ledger locks, so ingest for this
service (only) waits, and a batch caught mid-rename gets a retriable 503. A `busy` refusal changed
nothing; run it again.

**Verify:** `resource list` shows `$NEW` canonical and `$OLD` as a live alias with the sunset; the old
host's `GET /readyz` and an MCP call still work.
**Stop if:** the alias is not shown in `resource list`. **STOP: do not do step 8.** Flipping the app
to `$NEW` without the alias makes introspection authentication fail and the server answers 503 everywhere.

### 7. Register the console client for the new host

```bash
fly ssh console -a otto-platform -C \
  'savvagent-platform-server client register --name "otto-tower console" \
     --redirect-uri https://otto-tower.savvagent.com/auth/callback --first-party'
```

It prints a `client_id`. **Record it**: it becomes `OF_CONSOLE_CLIENT_ID` in step 8.

**Verify:** a `client_id` (`otto_client_…`) was printed and you have noted it.
**Stop if:** the command errors or prints no `client_id`.

### 8. Flip the Fly environment

Only now, with the alias confirmed in step 6. These are non-secret values, so they live in
`fly.toml`'s `[env]`: edit the file, merge the change through a PR, and let the deploy run.

```toml
OF_PUBLIC_URL        = "https://otto-tower.savvagent.com"
OF_RESOURCE_URI      = "https://otto-tower.savvagent.com/mcp"
OF_CONSOLE_CLIENT_ID = "<the client_id printed in step 7>"
OF_LEGACY_HOSTS      = "otto-factory.savvagent.com"
OF_LEGACY_SUNSET     = "<the same RFC 3339 $SUNSET>"
```

`OF_LEGACY_HOSTS` is a comma-separated list of extra, deprecated bare hostnames; `OF_LEGACY_SUNSET` is
optional but you should set it (an RFC 3339 date-time, or a date meaning midnight UTC). Both fail the
boot when set but unparseable, as does a sunset with no legacy host or a legacy host equal to
`OF_PUBLIC_URL`'s. Past the sunset the app keeps serving the old host until step 11, says in its
notice that the host was retired, and logs a warning at every startup.

From that deploy on, the app itself sends a browser on the old host to the new one: the console's
pages and `/auth/login` answer `308` to the same path and query on `OF_PUBLIC_URL`, carrying the
deprecation notice. That is what keeps console sign-in working (a sign-in started on the old host
would set its PKCE cookie where the platform's redirect back to the new host never arrives).
Everything a machine calls stays where it is.

**Only if the deployment is fronted by the Worker** (production is not today): once that deploy is
live, give the production Worker the same two facts, in `web/wrangler.jsonc`'s
`env.production.vars`, and redeploy it (`npm run deploy` in `web/`):

```jsonc
"OF_PUBLIC_URL": "https://otto-tower.savvagent.com",
"OF_LEGACY_HOSTS": "otto-factory.savvagent.com"
```

The Worker then makes the same `308` at the edge, before the origin sees the request; it already
sets `X-Forwarded-Host` on everything it proxies, which is how the origin knows which host a
request used. Set them before the Fly flip and browsers are redirected to a console that cannot
sign them in yet.

**Verify:**
- `GET https://otto-tower.savvagent.com/.well-known/oauth-protected-resource` names the new
  resource; an MCP client added with the new URL signs in and `whoami` says otto-tower.
- A request through the old host still works and carries `Deprecation: true`, a
  `Link: <https://otto-tower.savvagent.com/…>; rel="successor-version"` and, with a sunset
  configured, a `Sunset:` header. The browser console on the old host (its pages and `/auth/login`)
  answers `308` to the same path and query on the new host:
  `curl -sI 'https://otto-factory.savvagent.com/auth/login?org=acme'` shows
  `location: https://otto-tower.savvagent.com/auth/login?org=acme`. `/mcp`, `/.well-known`, `/api`,
  the rest of `/auth`, `/oauth`, `/platform/webhooks`, `/webhooks` and the health probes are served
  on the old host, never redirected (`curl -sI https://otto-factory.savvagent.com/healthz` is `200`).
- `GET https://otto-factory.savvagent.com/.well-known/oauth-protected-resource` names
  `https://otto-factory.savvagent.com/mcp` as `resource` (so a fresh sign-in on the old URL still
  passes the client's resource check), and an MCP client still configured with the old URL sees
  instructions that open with a deprecation paragraph naming the new URL and the sunset.
- `https://otto-tower.savvagent.com/` signs in (the new console client) and the header reads otto-tower.

**Stop if:** the app answers 503, fails to boot (an unparseable `OF_LEGACY_*` value), or any check
above fails. Roll back (see *Rollback*) before doing anything else.

### 8a. Flip the documentation URLs

Only after step 8's verification passes, merge the prepared URL-flip PRs in `savvagent/otto-factory`
and `savvagent/otto-platform`. They move public links to `https://otto-tower.savvagent.com`. Until
then the user-facing docs (README, `docs/clients/matrix.md`, `client-skills/`, the design of
record's intro, and the live-host sections of this file) keep their links on the live host
`https://otto-factory.savvagent.com`, so every command a reader copies works on the day the code
ships. The otto-factory flip PR also changes `.github/workflows/ci.yml`'s `environment.url`.

### 9. Point the lifecycle webhook at the new host

```bash
savvagent-platform-server resource set-webhook $NEW https://otto-tower.savvagent.com/platform/webhooks
#   -> prints a NEW signing secret ONCE
```

Set it **immediately**, without putting it on a command line (where it would land in shell
history and be visible in the process list). `read -rs` takes it without echoing it, and
`fly secrets import` reads `NAME=VALUE` from stdin and redeploys the app with it:

```bash
read -rs SECRET          # paste the otto_whsec_… value, then Enter
printf 'OF_PLATFORM_WEBHOOK_SECRET=%s\n' "$SECRET" | fly secrets import -a otto-factory-mcp
unset SECRET
fly secrets list -a otto-factory-mcp     # OF_PLATFORM_WEBHOOK_SECRET shows as Deployed
```

Between `set-webhook` and the end of that redeploy the platform signs with the new secret while
the running app still verifies with the old one, so **a short window of rejected webhook
deliveries is expected**. The platform retries them, so nothing is lost provided the window stays
short: that is why the secret step follows straight on. Confirm the redeploy finished (`fly secrets
list` says Deployed, and `fly status -a otto-factory-mcp` shows the machine started on the new
release) before you Verify.

**Verify:** delete a throwaway team at the platform and check for a `platform.team.deleted` row
(`GET /api/orgs/{org}/audit?actionPrefix=platform.`).
**Stop if:** no such row appears: the secret was not set, the redeploy has not finished, or the URL
is wrong. The old host keeps
receiving nothing new, so fix this before telling customers anything.

### 10. Tell customers

Before the sunset, customers must:

- **Move their tracker webhooks** (GitHub, JIRA) from `https://otto-factory.savvagent.com/webhooks/…`
  to the same path on `https://otto-tower.savvagent.com`. Those deliveries are proxied (never
  redirected) on the old host until the sunset, then stop.
- **Re-add the MCP server** under the new name and URL so the tools appear as `mcp__otto-tower__*`:
  ```bash
  claude mcp remove otto-factory
  claude mcp add --transport http otto-tower https://otto-tower.savvagent.com/mcp
  claude mcp login otto-tower        # or /mcp inside a session; interactive terminal required
  ```
  Other clients: re-add per `docs/clients/matrix.md`. Update `~/.claude/otto-tower-repos` if they use
  the session-start template (the old file name is still read).
- Update any hard-coded `https://otto-factory.savvagent.com` links.

Clients that do nothing keep working through the deprecation notice until the sunset.

### 11. After the sunset: drop the alias

```bash
savvagent-platform-server resource drop-alias https://otto-factory.savvagent.com/mcp
```

The alias row is kept as a tombstone, so the old URI can never be registered to a different server.
The drop also revokes any token or code still bound to the old URI string (the command prints the
counts); clients still pointed at the old URL must re-add the server.
Then clear `OF_LEGACY_HOSTS` and `OF_LEGACY_SUNSET` from `fly.toml`, redeploy, and remove the old
hostname's DNS records (at the current DNS provider) and certificate
(`fly certs remove otto-factory.savvagent.com -a otto-factory-mcp`). Only if the deployment is
fronted by the Worker: also remove `OF_PUBLIC_URL` and `OF_LEGACY_HOSTS` from the Worker's
`env.production.vars`, redeploy it, and remove the old custom domain.

### Rollback

- **Before step 8** (the Fly environment still names the old host): swap the URIs back at the
  platform, and nothing else has changed for users:
  ```bash
  savvagent-platform-server resource rename $NEW $OLD --alias-until $SUNSET
  ```
  (Run with `$NEW` as the first argument: when `$OLD` is a live alias of `$NEW`, the rename swaps the
  two.) The new host's certificate and DNS records (or, Worker-fronted, its custom domain) can stay
  or be removed. The unregistered new
  console client is harmless.
- **After step 8**: first restore `OF_PUBLIC_URL`, `OF_RESOURCE_URI` and `OF_CONSOLE_CLIENT_ID` in
  `fly.toml` to the old values (the old `OF_CONSOLE_CLIENT_ID` is in `fly.toml`'s git history),
  remove `OF_LEGACY_*`, and deploy (that deploy also stops the app sending the old host's browsers
  to the new one). Only if the deployment is fronted by the Worker: remove the Worker's
  `OF_PUBLIC_URL` and `OF_LEGACY_HOSTS` vars and redeploy it, or it keeps sending them there. The app keeps working
  meanwhile because the old URI is still a live alias. Then swap the URIs back as above. If the URL-flip PRs (8a) were merged, revert them too.
- **If step 9 had run**, also point the webhook back with
  `resource set-webhook $OLD https://otto-factory.savvagent.com/platform/webhooks` and set the secret
  it prints as `OF_PLATFORM_WEBHOOK_SECRET` the same way as in step 9, straight away and never on
  the command line:
  ```bash
  read -rs SECRET
  printf 'OF_PLATFORM_WEBHOOK_SECRET=%s\n' "$SECRET" | fly secrets import -a otto-factory-mcp
  unset SECRET
  ```
  Expect the same short window of rejected (and retried) deliveries until that redeploy finishes.
  If step 9 had not run, leave the webhook alone.
- **`rename-service` (step 4) is not rolled back.** Once metering is declared under `otto-tower/`,
  keep it: the binary that is deployed declares only that key. Rolling the *image* back to a pre-rename
  release would declare `otto-factory/jobs_completed`, which the platform then refuses; its shipper pauses
  safely, but do not stay there.
- **Rolling the image forward again is safe for billing.** While an older image runs it enqueues
  completed jobs under `otto-factory/jobs_completed`, after `0006_meter_otto_tower.sql` has already
  been applied, so that migration never rewrites them. The usage shipper therefore bills every row
  on a retired key (`tower_billing::RETIRED_METERS`) under its successor, on every batch, including
  rows an older replica writes while two versions run at once. Every `tower-server` start also runs
  `tower_billing::outbox::rewrite_retired_meters` before the shipper declares its meters: it moves every
  queued `usage_outbox` row still on a retired key to its successor (per org, safe under either RLS
  shape), logs `moved queued usage rows from a retired meter key` with the count when there were
  any, and is a no-op otherwise. Without these those rows would ship under an undeclared meter and
  be refused as `unknown_meter` (and, since the shipper keeps such rows queued, retried
  indefinitely). It does not touch `usage_outbox_rejected`; a row an older image already had
  refused stays there as history. To check after the roll forward (on `otto_factory`):
  ```sql
  BEGIN READ ONLY;
  SELECT meter, count(*) FROM usage_outbox GROUP BY meter;   -- no otto-factory/ rows
  ROLLBACK;
  ```

## Historical: the platform-split reset (one-time, already done)

The platform split replaces the 34 migrations with a single domain-only baseline
(`0001_baseline.sql`) and moves identity to the platform. **Production data was disposable,
so nothing is migrated: the otto_factory database is recreated empty, accounts are created fresh
at the platform, and passkeys and OAuth clients are re-registered.** This is the only time
the migration history has been rewritten; it is append-only again from the baseline.

**There is no rollback to the previous image.** The old binary expects identity tables in
its own database and a different `_sqlx_migrations` history. Rolling back means
re-creating the old database from a snapshot taken first.

Order matters; the platform goes first.

1. **Platform up.** `otto-platform` is deployed at `https://otto.savvagent.com`, its
   `/readyz` is green, and you can sign in and create your org there. (Its `rp_id` is the
   platform host, so the old passkeys are gone by design.)
2. **Snapshot** `otto_factory` (and take note of anything you need from it; it is about to
   be dropped).
3. **Register the resource** at the platform (see above) and keep the two secrets it prints.
4. **Recreate the `otto_factory` database empty.** On `otto-db`:
   ```sql
   DROP DATABASE otto_factory;   -- after stopping otto-factory-mcp, or with FORCE
   CREATE DATABASE otto_factory;
   ```
   (`fly postgres connect -a otto-db`). Do not touch `otto_platform`.
5. **Stage the secrets** on `otto-factory-mcp`:
   ```bash
   fly secrets set -a otto-factory-mcp \
     OF_INTROSPECTION_SECRET='otto_rs_…' \
     OF_PLATFORM_WEBHOOK_SECRET='otto_whsec_…'
   ```
   `DATABASE_URL` and `OF_ENCRYPTION_KEY` are unchanged. (Tracker credentials in the old
   database are gone with it; the key only has to stay stable from here on.)
6. **Deploy the factory** (merge the release PR; or `fly deploy -a otto-factory-mcp`). It
   applies the baseline to the empty database, verifies tenant isolation, checks its
   credential against the platform, and binds a port. Look for
   `the otto platform accepted this service's credential`.
7. **Verify**: `GET /.well-known/oauth-protected-resource` names the platform in
   `authorization_servers`; an MCP client pointed at `https://otto-tower.savvagent.com/mcp`
   is sent to the platform to sign in, and `whoami` answers with the platform's records.
   `usage_outbox` drains (`SELECT count(*) FROM usage_outbox` goes to 0).
8. **Webhooks**: delete a throwaway team at the platform and check `audit_events` for a
   `platform.team.deleted` row (`GET /api/orgs/{org}/audit?actionPrefix=platform.`).
9. **Update client configuration**: MCP clients re-authorize against the platform (their old
   tokens and registrations were the old server's and are gone). `docs/clients/matrix.md` and
   `client-skills/` need only the MCP URL, which is unchanged.

10. **Register the console's sign-in client** at the platform and set
    `OF_CONSOLE_CLIENT_ID` (see *Registering the console's sign-in client* above), then
    redeploy. The console is not usable until this is done: `/auth/login` answers
    `503 console_login_disabled` and the UI shows its sign-in failing. Verify by opening
    `https://otto-tower.savvagent.com/` in a browser: **Sign in** goes to the platform,
    comes back to `/o/<your org>`, and the header shows your name and a **Manage** menu
    linking into the platform's console. Agents (`/mcp`) never depended on this step.

## What's scaffolded

- `Dockerfile` — a console stage that builds `web/` into `/srv/console`, a Rust
  stage that builds `tower-server`, and a slim non-root runtime holding both. The
  migrations are not copied in: `tower_core::migrate` uses the `sqlx::migrate!` macro,
  which embeds them in the binary at compile time.
- `fly.toml` — `otto-factory-mcp` app, region `iad` (co-located with its Postgres
  instance), `min_machines_running = 1` with `auto_stop_machines = "suspend"` so a
  `watch` long poll never pays a cold start, health check on `GET /readyz`.

## What's assembled

`tower-server/src/main.rs` assembles the real server:

1. The Axum router merges tower-mcp (`/mcp`, bearer-authenticated, plus
   `/.well-known/oauth-protected-resource` naming the platform as the authorization
   server) and tower-web (the console API, tracker webhooks, and `/platform/webhooks`), bound
   to `OF_BIND`.
2. `/healthz` (liveness, no DB check) and `/readyz` (readiness — `SELECT 1`
   against the pool) are mounted directly in `tower-server`. Neither depends on the platform:
   a platform outage must not take a replica out of rotation.
3. Migrations run at startup via `tower_core::migrate`, which already takes a Postgres
   advisory lock for the duration (`sqlx::migrate!`'s built-in behavior), so
   several machines starting concurrently on a fresh database wait rather than
   racing through the same DDL.
4. Structured (JSON) logging via `tracing-subscriber`, filtered by `RUST_LOG`.
5. The usage shipper (`tower_billing::outbox::run`) declares the meters, then drains `usage_outbox` to the platform.
6. Graceful shutdown on `SIGTERM`/Ctrl+C: stops accepting new connections,
   waits for in-flight requests, makes a final usage flush (skipped if the shipper is still
   waiting for its meter declaration), then calls
   `Watcher::shutdown()` to release its detached `LISTEN` connection before the
   process exits.
7. `Db::verify_tenant_isolation` runs after the migrations and **before the port
   is bound**, so a database that cannot enforce tenant isolation stops the
   process instead of serving.

## What a deploy can and cannot do

`/readyz` passes and the API works end to end for an MCP client once the resource is
registered at the platform and its two secrets are set; the console additionally needs
`OF_CONSOLE_CLIENT_ID`. The product sends no email and holds no accounts: signing in,
passkeys, recovery, invitations, SSO, members, and teams are the platform's, and the
console links there. See the platform-split checklist for first-time setup and the move-to-`tower.savvagent.com` runbook for the host.

## The hostname

The hostname below is the Savvagent Tower one. The move to it is the runbook at the end of this
file. `otto-tower.savvagent.com` and `otto-factory.savvagent.com` are old hosts that only redirect
to it (`OF_REDIRECT_HOSTS`; see "#135: old hosts redirect" at the end). `OF_LEGACY_HOSTS` is not set.

The public hostname is **`tower.savvagent.com`**, and the app answers on
`otto-factory-mcp.fly.dev` without advertising it. No Cloudflare Worker or proxy sits in front:
the name resolves straight to the Fly app. DNS is at Namecheap, with an `A` record to the app's
shared IPv4 and an `AAAA` record to its dedicated IPv6. Do not copy the addresses from here; read
them from Fly, which is the source of truth:

```bash
fly ips list -a otto-factory-mcp          # v4 public ingress (shared), v6 public ingress (dedicated)
dig +short tower.savvagent.com A   # must match the v4
dig +short tower.savvagent.com AAAA   # must match the v6
```

(As of 2026-10-21 they are `66.241.125.222` and `2a09:8280:1::1aa:4d42:0`; an address this file
once listed had gone stale, which is why it no longer pins them.)

`fly certs add tower.savvagent.com -a otto-factory-mcp` issues and renews the
certificate; nothing else in DNS is needed, because the dedicated IPv6 is what Fly
validates ownership against.

`OF_PUBLIC_URL` is the origin of the discovery pointer in a `401` and of the tracker OAuth
callbacks; `OF_RESOURCE_URI` is the token audience. **Passkeys are no longer bound to this
hostname** — they belong to the platform's host — so, unlike before the split, moving it
does not invalidate anyone's credentials (it does require re-registering the resource at the
platform under the new URI, and re-authorizing clients).

A later move behind a Cloudflare Worker (not made today) would keep this same hostname: a Worker custom
domain serves `tower.savvagent.com` directly and proxies to the Fly app, so
`OF_ORIGIN` becomes `otto-factory-mcp.fly.dev` and `OF_PUBLIC_URL` does not
change. See `cloudflare.md`, whose `OF_ALLOWED_HOSTS` trap is exactly about that
split.

## Deploying

Deploys are automatic: the `deploy` job in `.github/workflows/ci.yml` runs
`flyctl deploy --remote-only -a otto-factory-mcp` when the release-please-maintained
"chore: release X.Y.Z" PR is merged to `master` — i.e., when release-please has just cut a
release — authenticated via `FLY_API_TOKEN`, a secret of the GitHub `production` environment
(an app-scoped Fly deploy token, minted with `fly tokens create deploy -a otto-factory-mcp` and
never valid for any other app on the `savvagent` org). The environment admits only `master`, so
no other branch's run can read the token, whatever its copy of the workflow says; never store it
as a repository secret. That protection reaches exactly as far as the `master` ruleset does: code
that lands on `master` runs with the token, so whoever can merge a change to
`.github/workflows/` can use it (CODEOWNERS for that path is #126). To rotate: mint a new token
with an expiry (`fly tokens create deploy -a otto-factory-mcp --expiry 8760h`), `gh secret set
FLY_API_TOKEN --env production -R savvagent/otto-factory`, then revoke the old one (`fly tokens
list -a otto-factory-mcp` / `fly tokens revoke <id>`) — revoking is the step that makes a
rotation mean anything, not an optional tidy-up. A failed `flyctl deploy` leaves the previously-running
machine serving traffic, since Fly's own rolling-deploy health check (`/readyz`) never cuts
traffic to a machine that hasn't passed it.

A push that fails `rust`, `web`, or `docker-build` never reaches `deploy` — `release-please`
(which cuts the tag/Release `deploy` gates on) itself needs all three to pass first, so a red
check anywhere upstream skips the whole chain rather than attempting a deploy against broken CI.

For an out-of-band deploy — re-deploying without a new commit, or deploying a specific
historical SHA — the manual command still works exactly as before:

```bash
fly deploy -a otto-factory-mcp
```

which will build the Dockerfile, run migrations on startup, and pass the `/readyz`
check before routing traffic to the machine.

## Operational notes

- `usage_outbox_rejected` holds usage events the platform refused (with its reason, and the meter and
  quantity when the event carried one); a row there
  is billing the platform was never told about, and each one is also logged at error level.
- `platform_events` markers older than 30 days and expired `removed_members` tombstones are swept
  hourly. `deleted_orgs` is permanent (the platform never reuses an org id).

## Runbook: move to tower.savvagent.com (#279, human-only, Rob)

Phase 3 of the Savvagent rebrand (savvagent/otto-platform#128) for this service. The PR that
closes #279 changes the code and `fly.toml`; the steps below are the human part. After them the
service is Savvagent Tower at `https://tower.savvagent.com/mcp`, its MCP server calls itself
`savvagent-tower`, and it authenticates to the platform with `sav_rs_` and `sav_whsec_` secrets.
The billing service slug (`otto-tower`) and the meter `otto-tower/jobs_completed` do not change;
neither do the `OF_*` variable names, the Fly app (`otto-factory-mcp`), the database or the
crates (phase 4, #280).

What the platform already has (phase 2): the resource registry lists this service as
`https://tower.savvagent.com/mcp` ("Savvagent Tower"), with `https://otto-tower.savvagent.com/mcp`
kept as a live alias until 2027-06-30, so the platform accepts the service under either URI and
the current `otto_rs_` credential keeps working across the deploy. The `otto-factory` alias is
dropped, which is why this change removes `OF_LEGACY_HOSTS` and `OF_LEGACY_SUNSET` (the sunset,
2026-10-22, has passed; the legacy-host support stays in `tower-server`, only the values go). The
`tower.savvagent.com` certificate is issued.

**R0. Look first (read-only).**

```bash
curl -fsS https://tower.savvagent.com/healthz                     # ok: DNS and certificate
fly certs list -a otto-factory-mcp                                # tower.savvagent.com Issued, and the old host
fly secrets list -a otto-factory-mcp   # no OF_PUBLIC_URL / OF_RESOURCE_URI / OF_ALLOWED_HOSTS secret overriding fly.toml
fly ssh console -a otto-platform -C 'savvagent-platform-server resource show https://tower.savvagent.com/mcp'
                                    # the canonical URI, its old one a `live` alias, the webhook URL
```

**R1. Flip the Fly env: merge the PR.** This is a release-please repository: merging the feature PR
does not deploy. The deploy happens when the release PR that carries it merges, so do R1 by
merging that release PR (CI deploys it). The deploy changes only `fly.toml` `[env]`:

| Variable | Before | After |
|---|---|---|
| `OF_PUBLIC_URL` | `https://otto-tower.savvagent.com` | `https://tower.savvagent.com` |
| `OF_RESOURCE_URI` | `https://otto-tower.savvagent.com/mcp` | `https://tower.savvagent.com/mcp` |
| `OF_ALLOWED_HOSTS` | (unset) | `otto-factory-mcp.fly.dev,otto-tower.savvagent.com` |
| `OF_LEGACY_HOSTS` | `otto-factory.savvagent.com` | (removed) |
| `OF_LEGACY_SUNSET` | `2026-10-22T00:00:00Z` | (removed) |
| `OF_CONSOLE_CLIENT_ID` | `otto_client_8ClmY61CJPC5BJHULc1Kug15hbSvw0vAy6CxUuXMZUY` | `sav_client_XRACC-VcMXZQpX7jG_8Oplm9IQNTV-KrK6KdxJE-MCk` |

`otto-tower.savvagent.com` stays an allowed `/mcp` host, so clients still configured with it keep
working until the old hosts become redirects (savvagent/otto-platform#135, below). The
`fly.dev` host is not needed today (no Cloudflare Worker fronts this app, so nothing reaches it with
that `Host`), but it is our own name and the Worker trap in `docs/deploy/cloudflare.md` makes it
mandatory the day a Worker is put in front, so it is listed. The new console client is the
public client registered at the platform as "Savvagent Tower console", redirect
`https://tower.savvagent.com/auth/callback`. Then check:

```bash
curl -fsS https://tower.savvagent.com/readyz                      # ready
curl -fsS https://tower.savvagent.com/.well-known/oauth-protected-resource
  # "resource": "https://tower.savvagent.com/mcp", "resource_name": "Savvagent Tower"
fly logs -a otto-factory-mcp --no-tail   # "the Savvagent platform accepted this service's credential"
```

The protected-resource metadata names the new URI on either host. A client that signs in afresh
through the old URL and checks that the metadata's `resource` matches the URL it called (RFC 9728
section 3.3) refuses it; step R3 moves the client to the new URL.

**R2. Rotate both secrets to `sav_`.** Both commands invalidate the old value the moment they print
the new one, so run them back to back and set the two secrets in one `fly secrets set` (one
restart). Until the restart, calls fail (introspection refused) and webhook deliveries fail their
signature check; the platform retries deliveries with backoff, so none is lost. The webhook must
name the new host before #135: the platform does not follow redirects.

```bash
platform() { fly ssh console --quiet -a otto-platform -C "savvagent-platform-server resource $*" | tr -d '\r'; }

WHSEC="$(platform set-webhook https://tower.savvagent.com/mcp https://tower.savvagent.com/platform/webhooks | grep -E '^sav_whsec_')"
RS="$(platform rotate-secret https://tower.savvagent.com/mcp | grep -E '^sav_rs_')"
[ -n "$WHSEC" ] && [ -n "$RS" ] && echo both captured || echo STOP: run the missing one again

fly secrets set -a otto-factory-mcp OF_PLATFORM_WEBHOOK_SECRET="$WHSEC" OF_INTROSPECTION_SECRET="$RS"
unset WHSEC RS

fly secrets list -a otto-factory-mcp             # both Deployed, new digests
curl -fsS https://tower.savvagent.com/readyz     # ready (a rejected secret is a 503 here)
fly ssh console -a otto-platform -C 'savvagent-platform-server resource show https://tower.savvagent.com/mcp'
                                                 # webhook=https://tower.savvagent.com/platform/webhooks
```

If a capture comes back empty, the command still ran and invalidated the old value: run that one
command again (each run issues a fresh secret) and set it. Neither secret can be read back. The
values live only in the shell variables, so they never reach shell history or the process list of
anything but `fly`.

**R3. Move your MCP client (`~/.claude.json`).** The server name changes with the MCP server name,
so the tools become `mcp__savvagent-tower__*`. Everything that names `mcp__otto-tower__*` must change
in the same sitting, or it silently stops matching:

- your `~/.claude/CLAUDE.md` and any project `CLAUDE.md` that names the tools;
- the hook matchers in `~/.claude/settings.json` (for example a `PreToolUse` matcher on
  `mcp__otto-tower__*`);
- `~/.claude/hooks/tower-lease-check.sh`;
- the session-start hook (`client-skills/claude-code/session-start-hook.sh` and its
  instructions) and any plugin that carries Tower skills (rbh-plugins). Rename the opt-in file
  `~/.claude/otto-tower-repos` to `~/.claude/savvagent-tower-repos` when convenient; the hook reads
  the old name when the new one is absent.

```bash
claude mcp remove otto-tower --scope user
claude mcp add --transport http --scope user savvagent-tower https://tower.savvagent.com/mcp
```

Then `/mcp` in a Claude Code session, pick `savvagent-tower`, and **Authenticate**. The agent doing
the switch loses its Tower tools until the client restarts, so do not do this mid-job: release your
claims and leases first, and finish the checklist above from a session that does not need them.

**R4. Verify `whoami` through the new host.** Restart the client and call the `savvagent-tower`
`whoami` tool: it answers with your org and user through `https://tower.savvagent.com/mcp`.

**GitHub App.** If the tracker GitHub App has a setup or callback URL configured
(`/trackers/callback`), move it to `https://tower.savvagent.com/trackers/callback` in the App's
settings. The old host keeps serving until #135, so an install started before the change still
completes.

**Rolling back.** Revert the release: the previous release serves `otto-tower.savvagent.com/mcp`
again, which the platform accepts as an alias until 2027-06-30. The `sav_` secrets belong to the
server, not to a URI, so they keep working on either release; nothing in R2 needs undoing. Move the
webhook back only if the old host is still served directly (before #135). `OF_CONSOLE_CLIENT_ID`
reverts with `fly.toml`; its old client keeps the old redirect URI.

## #135: old hosts redirect

`OF_REDIRECT_HOSTS = "otto-tower.savvagent.com,otto-factory.savvagent.com"` in `fly.toml` makes both
old hosts redirect-only. A request whose `Host` is one of them (port ignored) gets a bare
`308 Permanent Redirect` to `https://tower.savvagent.com` + the same path and query, for every method and
path, before allow-listing or auth: `/mcp`, the console, `/auth/*`, `/webhooks/*`, `/platform/webhooks`,
`/.well-known/*` and `/healthz`. A `308` keeps the method and body, so a client that follows redirects
replays its `POST` against the canonical host. `OF_ALLOWED_HOSTS` is now only the Fly origin host.
Startup refuses a redirect host that is also in `OF_ALLOWED_HOSTS` or `OF_LEGACY_HOSTS`, or is
`OF_PUBLIC_URL`'s host. If a Cloudflare Worker ever fronts these hosts, the redirect must be done there
too (the origin sees the Worker's own `Host`; it reads `Host`, never `X-Forwarded-Host`).

**Before the deploy (Rob, human).** Nothing that cannot follow a redirect may still point at an old
host. GitHub follows no redirects for webhook deliveries, and most senders do not:

- Each customer tracker webhook (GitHub, JIRA) still posted to `https://otto-tower.savvagent.com/webhooks/…`
  or `https://otto-factory.savvagent.com/webhooks/…` must move to the same path on
  `https://tower.savvagent.com` (step 10 asked customers to). Redirected deliveries are lost.
- The tracker GitHub App's webhook URL and setup/callback URL (`/trackers/callback`), in the App's settings,
  must name `https://tower.savvagent.com`.
- Confirm the platform's lifecycle webhook already names the canonical host:
  `fly ssh console -a otto-platform -C 'savvagent-platform-server resource show https://tower.savvagent.com/mcp'`
  shows `webhook=https://tower.savvagent.com/platform/webhooks`. If not, run `resource set-webhook` (R2).
- Agents still registered as `https://otto-tower.savvagent.com/mcp` must re-add the server at
  `https://tower.savvagent.com/mcp` (`claude mcp add --transport http savvagent-tower …`); an MCP client
  that pins its OAuth resource to the old URL will not follow the redirect.

**Certs and DNS stay.** Keep both Fly certificates and DNS records
(`fly certs list -a otto-factory-mcp`): the redirect is served by this app, which needs a valid
certificate on each old host. Only remove one when nothing resolves to it any more.

**Then drop the platform alias.** After the deploy is verified, remove the alias so the platform stops
accepting the service under the old URI:

```bash
fly ssh console -a otto-platform -C 'savvagent-platform-server resource drop-alias https://otto-tower.savvagent.com/mcp'
```

**Verify.**

```bash
curl -sI 'https://otto-tower.savvagent.com/mcp?x=1'      # HTTP/2 308, location: https://tower.savvagent.com/mcp?x=1
curl -sI 'https://otto-factory.savvagent.com/healthz'    # HTTP/2 308, location: https://tower.savvagent.com/healthz
curl -sI -X POST https://otto-tower.savvagent.com/platform/webhooks   # 308 too
curl -sI https://tower.savvagent.com/healthz             # HTTP/2 200, no redirect
```

**Rolling back.** Put `otto-tower.savvagent.com` back in `OF_ALLOWED_HOSTS` and clear `OF_REDIRECT_HOSTS`
(never both: startup refuses it), then redeploy. Re-add the platform alias if it was dropped.
