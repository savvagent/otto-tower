# The Savvagent Tower console on Cloudflare

Closes the question in [issue #2](https://github.com/savvagent/otto-factory/issues/2):
how to put `web/` on Cloudflare without giving up the single origin the session cookie
depends on.

**One Worker serves the built SPA and proxies everything dynamic to `of-server`.** The
browser sees one hostname. `web/wrangler.jsonc` and `web/worker/index.ts` are the whole of
it; this file is the account-side setup neither of them can express, and the two traps that
a first deploy hits.

The product is **Savvagent Tower** (renamed from otto-factory). The Worker scripts keep their
old names, `otto-factory-console` and `otto-factory-console-dev`, because a Worker script name
is an identity in the Cloudflare account; the Fly origin keeps `otto-factory-mcp.fly.dev`. The
public hostname moves to `tower.savvagent.com` in the operator cutover described below.

Self-hosted deployments are unaffected. `of-server` still serves `web/build` itself, the
`Dockerfile` still bakes it in at `/srv/console`, and nothing in this document is required
to run the product — Cloudflare is a deployment choice, not an architecture.

## Why a Worker rather than Pages, and why not two origins

The console's session is an `HttpOnly`, `__Host-`-prefixed cookie. `__Host-` means the
browser refuses to store it unless it is `Secure`, has `Path=/`, and carries **no `Domain`**
— so it is bound to exactly one origin and cannot be sent to a second one. Hosting the SPA
on `console.example.com` and the API on `api.example.com` does not need a CORS header; it
needs a different authentication transport, which the design rejects.

So whatever fronts the console must *proxy* the API rather than point at it. Given that,
Pages plus a Pages Function and a Worker plus static assets do the same job, and the Worker
is one product, one config file, and one `wrangler deploy`.

A plain CDN passthrough — proxied DNS in front of Fly, cache rules, `of-server` still
serving the bundle — was the other real candidate and remains a reasonable fallback. It
needs no new pipeline and cannot suffer version skew. It also does not put the console on
Cloudflare in any meaningful sense, which is what the issue asked for.

## Deploying

```bash
cd web
npm run build                          # adapter-static → web/build
npx wrangler deploy --env production   # or: npm run deploy, which does both
```

**Naming the environment is load-bearing.** The default configuration is the development
one: a different Worker (`otto-factory-console-dev`) whose `OF_ORIGIN` is
`http://127.0.0.1:8080`. A bare `wrangler deploy` therefore cannot overwrite the Worker the
console runs on, and cannot silently point a staging build at production — it deploys
something that proxies to an origin nobody is running, which fails immediately and visibly.
`--env production` is the only path to the real hostname.

Another environment is a block in `wrangler.jsonc` beside `production`, or a one-off
override:

```bash
npx wrangler deploy --var OF_ORIGIN:https://otto-tower-staging.fly.dev
```

Then attach the hostname the humans will use (`console.example.com`) to the Worker as a
custom domain. That hostname — not the origin's — is the public origin, and every value
below follows from that.

## The Savvagent Tower hostname and the legacy host

(The old public hosts `otto-tower.savvagent.com` and `otto-factory.savvagent.com` are now redirect-only
at the origin through `OF_REDIRECT_HOSTS`, `docs/deploy/fly.md` "#135: old hosts redirect". The Worker
text below is the deprecation-window design and applies only to a Worker-fronted deployment.)

**Production is not fronted by this Worker today.** `otto-factory.savvagent.com` resolves straight
to the Fly app (DNS at Namecheap, no Cloudflare in front), and the rename cutover there is a Fly
certificate plus `A`/`AAAA` records: step 5 of the ordered runbook in `docs/deploy/fly.md`
(*The rename cutover*). Everything in this section applies **only if the deployment is fronted by
the Worker**.

The behaviour a browser sees on the legacy host does not depend on the Worker. Without it,
`of-server` itself answers the console's pages and `/auth/login` on a legacy host with a `308` to
the same path and query on `OF_PUBLIC_URL` (`legacy_console_redirect` in
`crates/of-server/src/lib.rs`), carrying the deprecation notice; with it, the Worker answers that
`308` first and the origin never sees the request. Either way `/mcp`, discovery, `/api`, the
webhook receivers, `/platform/webhooks` and the health probes are served on the old host, never
redirected.

For a Worker-fronted deployment, the production Worker serves both hostnames during the
deprecation window: the canonical one and the legacy one (until the published sunset).

1. Add the canonical hostname (`tower.savvagent.com` for this product) as a **custom domain**
   on the production Worker (in the dashboard, or a `routes` entry with `custom_domain: true` in
   `wrangler.jsonc`), alongside the existing one, in place of the Fly certificate and DNS records
   of runbook step 5. Do not remove the old custom domain: it must keep serving until the sunset.
   The zone for the new name has to be on the same Cloudflare account.
2. `OF_ORIGIN` does not change (`https://otto-factory-mcp.fly.dev`), and neither does
   `OF_ALLOWED_HOSTS` for the Fly host. Both hostnames reach the origin through the same
   subrequest, so the origin tells them apart by the `X-Forwarded-Host` header the Worker sets
   from the request URL (never from anything the client sent). The legacy host also has to be
   listed in `OF_LEGACY_HOSTS`, which adds it to the accepted hosts (step 8 of the runbook).
3. On a legacy host the Worker answers the console's SPA paths and `/auth/login` with a `308` to
   the canonical host, and proxies every origin prefix (`/mcp`, `/.well-known`, `/api`, `/auth/*`
   except login, `/platform/webhooks`, `/webhooks`, `/oauth`) unchanged. Webhook deliveries are
   never redirected, so customers' tracker webhooks keep landing until they move them. The Worker
   learns which hosts are legacy and where to send them from two vars in `env.production.vars`,
   `OF_LEGACY_HOSTS` and `OF_PUBLIC_URL`, with the same values as `of-server`'s; they are added in
   runbook step 8, after the Fly flip, and with them unset nothing is redirected. The Worker parses
   `OF_LEGACY_HOSTS` exactly as `of-server` does (bare hostnames only). If either var is unusable,
   or `OF_LEGACY_HOSTS` names `OF_PUBLIC_URL`'s own host (which `of-server` refuses at boot), it
   refuses, with `500 misconfigured`, only the console pages it can no longer route — a legacy
   host's; with a malformed host list, any host's but `OF_PUBLIC_URL`'s; with a self-naming one,
   every host's — and keeps proxying every
   origin prefix, so agents and webhooks never notice. Its `308` carries `Deprecation: true` and a
   `successor-version` `Link`; `Sunset` comes only from the origin, which is the one told the date.
   `/auth/login/` (trailing slash) is proxied, not redirected: no such route exists to redirect to.
4. After the sunset (runbook step 11), remove the old custom domain and the two vars.

Nothing here is deployed by merging the code: it is account-side, and ordered after the platform
alias exists, so a request on the new host is never answered before the platform can authenticate
this service as the new URI.

## What must change on the origin

Two environment variables on `of-server`, and neither is optional.

| Variable | Value | Why |
|---|---|---|
| `OF_PUBLIC_URL` | `https://console.example.com` | The Worker's hostname, never the origin's. The discovery pointer in a `401` and the tracker OAuth callbacks are built from it. Point it at the origin and the console tells agents to connect to a host the browser never uses. |
| `OF_ALLOWED_HOSTS` | the origin's hostname, e.g. `otto-factory-mcp.fly.dev` | **See the trap below.** Without it every authenticated MCP call fails. |

`OF_RESOURCE_URI` defaults to `$OF_PUBLIC_URL/mcp` and needs nothing here (it must still be the URI registered at the Savvagent platform). Sign-in itself is the platform's and is not proxied through this Worker.

### Trap 1 — `OF_ALLOWED_HOSTS`, or the MCP endpoint refuses every call

A Worker cannot set the `Host` header on a subrequest; it is derived from the URL being
fetched, so the origin sees `Host: otto-factory-mcp.fly.dev` while `OF_PUBLIC_URL` says
`console.example.com`. `rmcp` validates that header — the check exists because a hosted MCP
server that answers to any `Host` is DNS-rebindable — and rejects the mismatch:

```
$ curl -X POST https://console.example.com/mcp -H "authorization: Bearer otto_pat_…" …
Forbidden: Host header is not allowed
```

The failure is worth recognising on sight, because it arrives *only* on authenticated
calls. An unauthenticated `POST /mcp` still answers `401` with a correct
`WWW-Authenticate` challenge, so discovery, registration and the whole OAuth flow look
healthy right up until the first tool call.

Adding the origin's hostname to `OF_ALLOWED_HOSTS` fixes it. An entry with no port matches
any port, which is what the local verification below relies on.

### Trap 2 — the origin must refuse traffic that did not come through Cloudflare

Anyone who can reach the Fly app directly skips whatever Cloudflare enforces at the edge
(WAF rules, rate limits, bot management), so the edge protections are only as good as the
origin's refusal to talk to anyone else. (This service no longer trusts a client-address
header for anything; the per-IP login throttles that once made this urgent moved to the
platform with the login surface.)

Lock the origin to Cloudflare. In rough order of strength:

1. **Cloudflare Tunnel** (`cloudflared` alongside `of-server`), so the origin has no public
   address at all.
2. **Authenticated Origin Pulls**, so the origin accepts only TLS clients presenting
   Cloudflare's certificate.
3. **Firewall rules** limiting ingress to Cloudflare's published address ranges.

None of these is wired up here: they need the account, the deployed origin, and a domain,
all of which arrive with the first live deploy (milestone 1, task 13). Until then the
choice is written down rather than made — but the deploy is not finished without it.

## Verified locally

`wrangler dev` in front of `cargo run -p of-server`, which is the same shape as the real
thing minus Cloudflare's own edge:

```bash
# origin
OF_PUBLIC_URL=http://localhost:8788 OF_BIND=127.0.0.1:8080 \
OF_ALLOWED_HOSTS=127.0.0.1 \
  cargo run -p of-server

# edge
cd web && npm run build
npx wrangler dev --port 8788 --var OF_ORIGIN:http://127.0.0.1:8080
```

What that run established:

- **Routing matches the server's own rule.** `/`, `/o/acme/queue`, `/apiary` and
  `/mcp-guide` all render the SPA; `/api` and `/api/no/such/thing` answer the origin's JSON
  `404`; `/healthz` and `/readyz` answer JSON, not HTML. `/apiary` is the case that matters:
  it is a legal org slug, and a prefix test that is not segment-aware sends it to the origin.
- **The session cookie survives the edge intact** — `__Host-of_session; Path=/; HttpOnly;
  Secure; SameSite=Lax; Max-Age=1209600`, which is the whole of issue #2's third checkbox.
  Sign-up, emailed verification link, TOTP enrolment, org creation and PAT minting were all
  driven through the Worker. (This run predates the removal of email and TOTP — see the
  milestone plan's Task 6 for what replaced them; a re-run today would drive a passkey
  ceremony instead.)
- **The OAuth issuer names the edge**, because it is built from `OF_PUBLIC_URL`. The minted
  PAT's audience came back as `http://localhost:8788/mcp`.
- **`303`s pass through.** `POST /oauth/authorize` returned `303` with
  `Location: http://localhost:3118/callback?code=…` rather than a followed body — the Worker
  sets `redirect: 'manual'`, without which Cloudflare would fetch the client's callback
  itself, burn the single-use code, and leave the agent waiting forever.
- **A forged client address does not reach the origin.** A request carrying
  `cf-connecting-ip: 9.9.9.9` was recorded by the origin under the throttle bucket
  `link:127.0.0.1`.
- **Trap 1 reproduces and the fix works.** Without `OF_ALLOWED_HOSTS`, `whoami` over MCP
  returned `Forbidden: Host header is not allowed`; with it, `org: edge | kind: pat`.

## Caching

The Worker forwards origin paths with `cf: { cacheTtlByStatus: { '200-599': -1 } }` — a
negative TTL, which is the documented way to say "never", where `cacheTtl: 0` only means
"already expired". Nothing under `/api`, `/oauth`, `/.well-known` or `/mcp` is cacheable and
some of it is per-session: a heuristically cached `GET /api/me` is one user's identity
served to another.

Static assets are a different matter and need no configuration. `adapter-static` emits
content-hashed filenames under `_app/immutable/`, and Workers static assets serve and cache
them on Cloudflare's network without the Worker being involved in the response body.

## What is still open

- The origin lock (trap 2) is documented, not implemented.
- There is no CI step that builds or deploys the Worker, because there is no CI
  (milestone 1, task 1).
- The Worker's copy of the bundle and the copy inside the `of-server` image are deployed
  separately, so they can drift. Whoever wires up CI should deploy both from one commit.
- `worker/index.test.ts` asserts the Worker's prefix rule against the same cases the Rust
  side asserts, but nothing mechanically ties the two lists together. A third copy would
  need the same treatment.
