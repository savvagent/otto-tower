# `web/` — the Savvagent Tower console

SvelteKit 2 · Svelte 5 (runes) · Tailwind v4 · TypeScript, strict.

What a human touches in Savvagent Tower: repos and their live leases, a read-only queue,
tracker connections, and the audit log. Sign-in, members, teams, SSO, usage, and tokens are
the Savvagent platform's; the console signs in through it and links to it.

```bash
npm install
npm run dev      # Vite on :5173, proxying /api /auth /oauth /.well-known to OF_API_ORIGIN
npm run check    # compile messages, check completeness, then svelte-check + tsc
npm run lint     # prettier --check
npm test         # vitest — the Worker's routing rule, locale resolution, the error map
npm run build    # static bundle in build/
npm run deploy   # build, then deploy the production Worker — docs/deploy/cloudflare.md
```

`npm run dev` needs a server behind it. Set `OF_API_ORIGIN` if it is not on
`http://127.0.0.1:8080`. **Until task 13 binds a port there is nothing to proxy to**, so
the dev server renders the shell and every request 502s.

## Why it is a single-page app

Not a performance choice. The console's session is an `HttpOnly`, `__Host-`-prefixed
cookie, and `__Host-` means the browser refuses to store it unless it is `Secure`, has
`Path=/`, and carries **no `Domain`** — so the cookie is bound to one origin and cannot be
sent anywhere else.

A SvelteKit server rendering these pages would therefore have to hold that credential to
fetch on the user's behalf: a second process with the keys to every console session, for
pages that are behind a login and cannot be cached anyway. Building to static files that
`tower-server` serves beside `/api` keeps the cookie in exactly one place — the browser — and
makes CORS a non-question, because there is no second origin.

The same fact drives `vite.config.ts`. Dev proxies `/api`, `/auth`, `/oauth`, and
`/.well-known` rather than pointing `fetch` at another port, because a cross-port request
would not carry the session and no CORS header could rescue it. To sign in under `npm run
dev`, run the server with `OF_PUBLIC_URL=http://localhost:5173` and have
`http://localhost:5173/auth/callback` registered as a redirect URI for the console client at
the platform.

## How sign-in works

The console does not sign anyone in. **Sign in** is a full navigation to `/auth/login`
(never `goto`: it is a server route that redirects to another origin, so links to it carry
`data-sveltekit-reload`), which sends the browser to the platform; the platform sends it
back to `/auth/callback`, where `tower-web` redeems the code, stores the platform's token pair
server-side, and sets `__Host-of_session`. The browser holds only that opaque cookie.
`GET /api/session` answers who is signed in, to which org, with what role.

A session is signed in to **exactly one org** (the platform's token is bound to it). So
`/o/{slug}` for any other org is a sign-in for that org (`/auth/login?org={slug}`), and an
API `401 unauthenticated` / `org_session_mismatch` anywhere does the same (`$lib/api`,
`$lib/login`). One attempt per org per minute: if the platform signs the visitor in to a
different org anyway, they are not a member of the one asked for, and the page says "no such
organization" rather than looping.

Everything identity-shaped — members, teams, SSO, usage, tokens, the account, switching
org — is a link into the platform's console (`$lib/platform`, from the `platformUrl` the
session reports). The old identity pages and API calls are gone.

## What holds across the app

**No credential is spent on a `GET`.** The tracker callback page (`/trackers/callback`)
receives a single-use authorization code in its URL and `POST`s it from script; a mail
scanner or link-preview fetcher that follows the URL loads a page and burns nothing.
`/auth/callback` is the exception that proves the rule: it is a `GET` that spends a code,
but it is bound to the browser that started the flow by a sealed cookie and the PKCE
verifier, so a fetcher holding only the URL gets `login_error=invalid_state`.

**An org you are not in renders as "no such organization".** The API answers `404` for both
a nonexistent org and one the caller is not in, precisely so the two cannot be told apart.
A console that helpfully said "you don't have access to acme" would undo that from the
client side.

**Roles decide what is _shown_, never what is _allowed_.** `OrgContext.isAdmin` hides
buttons that would fail. Every one of them is still refused by the server, on every
request, by `OrgCtx`.

**The queue is read-only.** There is no button here that changes a job. A job is created
and finished by the agent doing the work, over MCP; a human pressing "mark complete" would
be telling the queue something they cannot observe, and the audit trail would record it as
fact.

**Nothing about the deployment is baked into the bundle.** The MCP endpoint and the
grantable scopes come from `/.well-known/oauth-protected-resource` at runtime. A hard-coded
MCP URL is how a staging or self-hosted deployment ends up printing a connect command that
points at production.

## Messages, and what a new string costs

The console ships in **English, Spanish, German, French, Italian and Hindi**. Messages are
compiled, not looked up at runtime: [Paraglide JS](https://inlang.com/m/gerre34r) turns
`messages/{locale}.json` into tree-shaken functions under `src/lib/paraglide/`, which is what
lets an i18n layer exist here at all without a SvelteKit server.

```
project.inlang/settings.json   the six locales and the base locale
messages/{en,es,de,fr,it,hi}.json   the catalogs
scripts/check-messages.mjs     the completeness gate, wired into `npm run check`
src/lib/locale.ts              which language this document is in, and how it got there
src/lib/errors.ts              ApiError.code / WebauthnError.code → a sentence
src/lib/labels.ts              JobStatus / role → a word (the wire value never changes)
src/lib/paraglide/**           generated. git-ignored, prettier-ignored, never edited.
```

**Adding a user-visible string costs six catalog entries, not one.** `npm run check` fails if
any locale is missing a key, has a key the base locale does not, drops a `{placeholder}`, or
gets its plural categories wrong. That is deliberate: Paraglide _silently_ falls back to the
base locale for a missing key, so without the check a half-translated release looks fine in
development and reaches a customer as half a page in the wrong language.

```svelte
<script lang="ts">
  import { m } from '$lib/paraglide/messages';
</script>

<h1>{m.queue_title()}</h1><p>{m.queue_jobs_shown({ count: jobs.length })}</p>
```

Three things about the toolchain that the obvious reading gets wrong, all verified against
`@inlang/paraglide-js` rather than assumed:

- **Plurals use the variant form, not the ICU one-liner.** `"{count, plural, one {…} other {…}}"`
  belongs to a different plugin and compiles here to the literal string `undefined other }`.
  Write `[{ "declarations": ["input count", "local countPlural = count: plural"], "selectors":
["countPlural"], "match": { "countPlural=one": "# job", "countPlural=other": "# jobs" } }]`.
- **The required plural categories differ per locale, and the check enforces both directions.**
  `en`, `de` and `hi` need exactly `one` and `other`; `es`, `fr` and `it` also need `many`
  (Spanish 1 000 000 is "un millón _de_ trabajos"). Declaring `many` where the locale never
  selects it fails too.
- **`--emit-ts-declarations` is what makes a key a type error.** Without it Paraglide emits no
  `.d.ts` at all and `m.no_such_key()` type-checks clean. It is in the `paraglide:compile`
  script for that reason, not for tidiness.

`src/lib/paraglide/` is generated and git-ignored, so `check` and `build` compile it first. A
bare `npx svelte-check` in a fresh clone reports missing modules until `npm run paraglide:compile`
has run once.

**What is not translated, on purpose.** The MCP surface — `tower-mcp` tool descriptions and
`tower-core` error messages — stays English: its reader is an LLM that has never seen these docs,
and translating it would fragment the one audience it has. Commands, JSON/TOML snippets, config
paths, product names, and every wire value stay verbatim; a translated `--transport http` is a
broken command. The two server-rendered pages (`/oauth/authorize`'s consent screen and its error
page) are localized, but by a hand-written table in `crates/tower-web/src/i18n.rs`, because they
have no client-side JS to swap strings and share no keys with these catalogs.

## Which language, and how it is remembered

Two tiers now, each with one job:

| Tier                        | Holds                         | Authority                        |
| --------------------------- | ----------------------------- | -------------------------------- |
| `localStorage['of.locale']` | a choice made in this browser | wins when present                |
| `navigator.languages`       | the browser's preference      | fallback when nothing was chosen |

There used to be a third, the account's `users.locale` on Savvagent Tower's own user row, which was
the source of truth so a choice followed the person between devices. Accounts are the
platform's now and its API reports no locale, so nothing in the console sets the language
today beyond the browser's preference. `locale.ts` keeps `reconcile` and `applyLocale` for when
the platform exposes one (or a picker returns); nothing calls them yet.

A change of language **reloads the document**. Paraglide's `m.*()` are plain calls, not reactive
reads, so Svelte has no dependency to invalidate when the locale changes underneath them.
`locale.ts`'s `needsReload` is split out so the termination argument is a test rather than a
comment: the cache is written _before_ the reload, so the next boot resolves to exactly the
value that triggered it.

`<html lang>` is set from script at boot. `app.html` ships `lang="en"` and is not templated per
locale — `adapter-static` emits one shell for every route and there is no server to pick a
language for it.

## Layout

| Path                        | What it is                                                                               |
| --------------------------- | ---------------------------------------------------------------------------------------- |
| `src/lib/api.ts`            | The only place that talks to `tower-web`. `ApiError` carries the stable `code`.          |
| `src/lib/types.ts`          | The wire types, transcribed from `tower-web`'s OpenAPI document.                         |
| `src/lib/session.svelte.ts` | Who is signed in. A rune module, not a store.                                            |
| `src/lib/org.svelte.ts`     | The org the current route is about, via context.                                         |
| `src/lib/login.ts`          | Starting sign-in, and the loop guard.                                                    |
| `src/lib/platform.ts`       | Links into the platform's own console.                                                   |
| `src/lib/poll.svelte.ts`    | Polling a page hands to an `$effect`. The overview and `/queue` use it; `/repos` should. |
| `src/routes/`               | Public pages at the root; org pages under `/o/[org]`.                                    |
| `worker/index.ts`           | The Cloudflare Worker: serves this bundle, proxies the API to `tower-server`.            |
| `wrangler.jsonc`            | That Worker's config. `worker/tsconfig.json` type-checks it separately.                  |

Org pages live under `/o/[org]` rather than `/[org]` so that no org slug can ever collide
with a page name. The paths the _server_ names — `/auth/login`, `/auth/callback`,
`/auth/logout`, `/trackers/callback`, and a sign-in's default landing page `/o/{slug}` — are fixed
by what the server redirects to, and must not be renamed here alone.

## Deploying to Cloudflare

`npm run deploy` uploads `build/` and `worker/index.ts` as one Worker: Cloudflare serves the
SPA from its own network and the Worker forwards `/api`, `/auth`, `/oauth`, `/.well-known`, `/mcp`,
`/platform`, `/webhooks`, `/healthz` and `/readyz` to `tower-server`.

**It deploys `--env production`, and that is not a formality.** The default configuration
names a _different_ Worker (`otto-factory-console-dev`) pointing at `http://127.0.0.1:8080`,
so a bare `wrangler deploy` cannot land on the Worker the console actually runs on, and a
deploy that forgets which environment it is in proxies to something that is not listening
rather than quietly serving production traffic.

It proxies rather than redirecting for the same reason the app is a SPA at all — the
`__Host-` cookie is bound to one origin, so the API cannot live on a second hostname. The
prefix list in `worker/index.ts` mirrors `API_PREFIXES` in `crates/tower-server/src/lib.rs` and
must not drift from it — `worker/index.test.ts` is the half of that check which lives here,
and `api_prefixes_do_not_match_by_string_prefix_alone` is the other half.

Two things on the `tower-server` side are not optional and are easy to miss:
`OF_ALLOWED_HOSTS` must name the origin's own hostname, or every authenticated MCP call
fails while everything else looks healthy; and the origin must refuse traffic that did not
come through Cloudflare, because `OF_CLIENT_IP_HEADER=cf-connecting-ip` is only trustworthy
while Cloudflare is the one writing it. [`docs/deploy/cloudflare.md`](../docs/deploy/cloudflare.md)
has both, and what a local `wrangler dev` run proved about each.

None of this is required to run the console. `tower-server` still serves `build/` itself, which
is what a self-hosted deployment does.

## Adding a client to the connect page

Add an entry to `CLIENTS` in `src/lib/clients.ts`. Every client gets the same two forms —
OAuth and access token — because Savvagent Tower is coding-agent agnostic by constraint, and a
console that gave one agent a bespoke wizard and the rest a footnote would be the first
place that promise quietly broke.
