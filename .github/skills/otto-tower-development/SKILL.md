---
name: otto-tower-development
description: Use when developing any feature or fix in the Savvagent Tower repository (the hosted multi-tenant MCP server for coordinating agentic coding work) end-to-end — from a GitHub issue or plain task brief, through ship and verify, fully autonomously with no mid-run questions. Bundles the plan-by-plan discipline (committed design specs in docs/specs/ and implementation plans in docs/plans/), the Rust workspace conventions (tower-core owns all SQL, the two-guard tenant isolation rule, the passkey auth spine, substrate-not-workflow scope discipline), autonomous spec generation, plan generation, task-by-task implementation, PR review loops, verification, and close-out. For other repositories use general-development; for fs-ci use ce-development.
---

# Otto-Tower Development — Autonomous End-to-End

Autonomous, plan-driven feature/fix workflow for the **Savvagent Tower** repository (the hosted,
multi-tenant MCP server for coordinating agentic coding work). Walks from intake (a GitHub issue OR
a plain task brief) through spec → plan → implement → PR → review → merge → verify → close, with no
mid-run human questions. Returns control only when the work is shipped + verified, or on a true
blocker.

This is the **Savvagent Tower-specific sibling** of `general-development`. Same spine; the
repo-agnostic convention-discovery phase is replaced by the hardcoded conventions below (Rust
workspace, plan-by-plan docs, the tenant-isolation and passkey spines), the way `ce-development`
hardcodes fs-ci's. If you are working in any other repository, use `general-development`.

## Why this shape

The spine mirrors what this repository builds: Savvagent Tower is a **coordination substrate** — a
queue of jobs anchored on repos, where the specification of work lives outside the server in the
customer's own skills. The workflow below is that separation applied to the repo itself: a design
spec and a plan document first (the specification), implementation against them (the mechanical
execution), and the repo's own test suite plus CI as the verifier.

[`CLAUDE.md`](../../../CLAUDE.md) is the load-bearing conventions document and outranks anything
here that has drifted from it. [`docs/specs/2026-09-01-otto-factory-design.md`](../../../docs/specs/2026-09-01-otto-factory-design.md)
is the design of record; [`docs/plans/2026-09-01-milestone-1.md`](../../../docs/plans/2026-09-01-milestone-1.md)
records where the build currently stands, per task, with ✅ / 🚧 / ⬜ markers and a `## Status` block
that is kept current. There is no archive directory — a shipped spec keeps its place in
`docs/specs/` with its `> **Status:**` flipped to IMPLEMENTED, and the plan's markers are updated in
place. Docs can lag the code. Read the code.

## The Iron Law

**The source of truth is the task brief OR the GitHub issue — whichever started the work.** Not
Slack. Not the PR title. Not a teammate's summary. If the work began from a plain instruction, that
instruction (captured verbatim at intake) is the contract. If an issue exists, read the issue.

**Fully autonomous means no mid-run questions.** Make the most reasonable interpretation, document
the assumption, continue. Escalate only on true blockers. "Should I continue?" is never a stop
condition.

**Green tests are not the same as work-done.** `cargo test --workspace` does not validate the
container image (`Dockerfile`/`fly.toml`), the console SPA (`web/` — its gates are `npm run check`,
`npm run lint`, `npm test`, `npm run build`), the Cloudflare Worker in front of it (`web/worker/`),
or a fresh-cluster apply of `crates/tower-core/migrations/`. Whatever this change touches, verify it
explicitly (Phase 5).

**Violating the letter of the workflow is violating the spirit.**

## Non-Negotiable Rules

These hold for every run of this skill, no exceptions, no fast-path carve-outs:

1. **All work happens in a worktree.** Never edit, commit, or stage anything in the main checkout's
   working tree. The worktree is created in Phase 0 and removed in Phase 4 step 12. There is no
   scenario in which code is written on `master` directly.
2. **`master` changes only through PRs.** Every commit to `master` lands via a reviewed, merged PR.
   Never push a commit or branch directly to `master`; never `git push origin master`; never merge
   without the PR open, reviewed, and green. The only master-branch writes this workflow performs
   are squash-merges of PRs (Phase 4 step 11).
3. **Coding agents never self-attribute.** No `Co-Authored-By` trailers, no "Generated with"
   footers, no `🤖`/`AI`/credit markers of any kind — in commit messages, PR bodies, code comments,
   docs, or READMEs. This applies to direct work and to anything delegated to a subagent. An
   otherwise-perfect commit that carries attribution is rejected and rewritten.
4. **Every PR is reviewed by a Rust expert, an architect, and an independent security agent.** No
   PR opens, merges, or is pushed through the review loop without a dedicated `rust-pro` (Rust
   expert) review, a dedicated `architect-reviewer` (architectural) review, AND a dedicated
   `security-auditor` review on record. Fast-path and trivial PRs included — there is no size-based
   carve-out. All three must pass (or their issues resolved) before merge.
5. **The security review is independent.** The `security-auditor` agent receives **only the PR
   diff** — never the spec, never the plan, never the task brief, never the PR body summary, never
   the implementer's report. Its findings must be produced from the diff alone, so it cannot be
   steered by the implementer's framing. This is deliberate: the security review is the one pass
   that evaluates what was actually built, unmediated.
6. **A public interface change is a deliberate, documented change.** The interfaces customers and
   agents bind to are the **MCP tool surface** (`tower-mcp` tool names, input schemas, and the
   one-field result envelopes in `tools::out`), the **console REST API** (`tower-web`'s
   `catalog.rs` routes and their request/response shapes, which the OpenAPI document is rendered
   from), the **OAuth/discovery endpoints**, the **config surface** (`OF_*` env vars), and the
   **database schema** (forward-only migrations). Additive changes — a new tool, a new optional
   field, a new route added to the catalog, a new env var with a default — are the normal case and
   need no version bump while every crate is `0.1.0` under the workspace `[workspace.package]`
   version. A **breaking** change — renaming or removing a tool, field, route, or env var; changing
   a result envelope's shape; editing an already-applied migration — is never an incidental
   refactor side-effect and never a fast-path change. It must be named in the spec and the plan,
   flagged explicitly to the architect reviewer, and recorded in `docs/clients/matrix.md` if it
   changes what a coding-agent client sees. **Editing an applied migration is not a breaking change
   to be documented — it is forbidden. Add a new migration.** A silent breaking change is a
   rejected PR.
7. **The three constraints in `CLAUDE.md` outrank the task brief.** Coordination is anchored on
   repos; the server is a substrate and ships no workflow opinion; every coding agent is equally
   first-class. A brief that asks for something violating one of them is a brief to escalate on
   (Stop & Escalate condition 9), not to implement.

## When to Use This Skill vs. Alternatives

| Situation                                                             | Use                                                      |
| --------------------------------------------------------------------- | -------------------------------------------------------- |
| Any feature/fix in Savvagent Tower, full lifecycle, no human in the loop | **otto-tower-development** (this skill)                |
| Work in another repository                                            | `general-development`                                    |
| Work in fs-ci / Contract Explorer                                     | `ce-development`                                         |
| Already mid-implementation, just need to address PR review comments   | the Review-Response step here (Phase 4 step 9)           |
| Spec/plan only, will hand off to a human implementer                  | Phases 1–2 of this skill                                 |
| Guided mode with human approval at each checkpoint                    | run the phases directly, stopping at each gate           |
| One-line typo fix or docs nit                                         | Fast-path below — the full spec/plan phases are overkill |

## Fast-Path: Trivial Tasks (skip the spec + critique loops)

This repo is **plan-by-plan by house style** — but genuine triviality does not need a design spec.
Skip the spec document, the spec critique, and the plan critique ONLY when **ALL** of the following
are true:

- Single-file or 1–2 logical source files (tests and lock files don't count toward the cap; a file
  and its required mirror/duplicate count as one logical file)
- No new public interface: no new MCP tool, no new console route in `catalog.rs`, no new SQL
  statement or `tower-core` function, no new `OF_*` config key, no new migration, no new crate
- No **breaking** change to the MCP tool surface, the console API, the OAuth/discovery endpoints,
  the config surface, or the schema (Non-Negotiable Rule 6) — breaking changes are never fast-path
- No change to the auth spine (`of-auth`: passkey ceremonies, OAuth 2.1 AS, token hashing, sessions,
  PATs, redirect-URI matching), to tenant isolation (`Tx`/`OrgCtx`/RLS policies), or to metering
  (`tower-billing::classify`, `Factory::charge`)
- No change to crate boundaries (no crate gains a dependency edge; no SQL appears outside `tower-core`)
- No behavior change on a code path covered by tests (a type-only fix is fine; a logic change that
  alters runtime behavior is not)
- No change to deploy/distribution shape (`Dockerfile`, `fly.toml`, `.github/workflows/`, `web/`,
  `web/worker/`, `crates/tower-core/migrations/`)
- No new file under `.github/skills/`, and no change to dispatch/orchestration logic in an
  existing one, however small — a prose or typo fix inside one stays eligible
- The acceptance criterion fits in one sentence

Concrete examples that qualify:

- Fix a type error with no behavior delta
- Fix a typo in a string / comment / docstring
- Rename a local variable
- Delete demonstrably dead code (verified zero production callers)
- Run `cargo fmt --all` over a crate
- Update a hardcoded constant the brief names verbatim

**Even when fast-pathing, the plan document is not skipped.** Per house style, the plan lives in the
repo — a fast-path ticket still lands a minimal single-task plan at
`docs/plans/YYYY-MM-DD-<slug>.md` in the plan's task form (Goal + one `## Task` with `- [ ]` steps +
TDD + commit step). The design spec and both critique loops are the parts that are skipped. Add to
the PR body: `Fast-path: no design spec per otto-tower-development trivial-task criteria — <reason>.`

If you find yourself rationalizing into the fast-path on something that touches 3+ source files,
introduces a new interface, touches the auth spine or tenant isolation, adds a migration, adds a
file or changes dispatch/orchestration logic under `.github/skills/` (a prose or typo fix stays
eligible), or has more than a one-sentence AC → STOP. Write the spec. The fast-path is for genuine
triviality, not "I think this is small."

| Fast-path rationalization                                  | Reality                                                                                                                                                                     |
| ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| "It's only 3 files"                                        | Fast-path caps at 2. Three files → spec.                                                                                                                                    |
| "The new MCP tool is tiny"                                 | A new tool is a new public interface — and it must be priced in `tower-billing::classify` and described for an LLM that has never read the docs. Spec.                         |
| "The new console route is tiny"                            | A route means a `catalog.rs` entry, an `OrgCtx` authorization decision, and an OpenAPI summary. Spec.                                                                       |
| "I'll just add the query in `tower-web` instead of `tower-core`" | Every SQL statement lives in `tower-core`; a query elsewhere bypasses the `Tx` pinning RLS depends on. A design defect, not a nit.                                             |
| "I'll tweak the existing migration rather than add one"    | Migrations are forward-only. Editing an applied one is forbidden outright (Rule 6).                                                                                         |
| "The type fix incidentally fixes a bug"                    | If behavior changes, you need the spec to record what it changed and why.                                                                                                   |
| "I'll fast-path the first sub-change and spec the rest"    | If the work splits into sub-changes, write the spec. Multi-step work doesn't fast-path.                                                                                     |
| "The new skill content is tiny — just a couple of files"   | Process docs that will drive future autonomous runs are themselves architecture. PR #150 fast-pathed a 511-line-at-the-time skill-file pair on "two logical files" and needed five rounds of trio review before it hardened. Spec first. |
| "It's only one bullet, hardly a substantial rewrite"       | The rule doesn't turn on size — any change to dispatch/orchestration logic under `.github/skills/` disqualifies, however small. Grading your own edit as "not substantial" is the exact rationalization this section exists to refuse. Spec first (a prose/typo fix is the only carve-out). |
| "No spec, but I'll still write a one-line plan"            | Either the work needs a plan (then write the spec too) or it doesn't (then it doesn't need the plan either — and per house style, even fast-path keeps a minimal plan doc). |

## Repository Conventions (Savvagent Tower)

| Convention                 | Value                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Repo                       | `savvagent/otto-factory` — pass `--repo savvagent/otto-factory` on `gh` commands run from a worktree or outside the checkout. The product is Savvagent Tower but the GitHub repo is still named `savvagent/otto-factory` until a human renames it (GitHub redirects the old name afterwards); update these slugs then.                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Trunk                      | `master`. **All** work happens in a worktree; **all** master-branch changes land via merged PRs. No direct commits, pushes, or merges to `master` outside a PR (Non-Negotiable Rules 1–2).                                                                                                                                                                                                                                                                                                                                                                  |
| Worktree                   | **Required.** `git worktree add .worktrees/<branch> -b <branch> origin/master` — the worktrees live **inside the repo** at `.worktrees/<branch>`, not as sibling directories. Confirm `.worktrees/` is gitignored before creating the first one — **never `git add .`** in the main checkout (a nested worktree's contents, including a developer-local `.env`, could be staged). Branch off `origin/master`, never local `master` (see Phase 0 trunk-sync). Every code edit, commit, and push happens from inside this worktree.                           |
| Branch name                | `<area>/<kebab-slug>`, matching the repo's history — `passkeys/webauthn`, `deploy/isolation-startup-check`, `ci/github-actions`. Area is a crate short name (`core`, `auth`, `mcp`, `billing`, `trackers`, `web`, `server`) or a theme (`deploy`, `ci`, `docs`).                                                                                                                                                                                                                                                                                            |
| Commit format              | `<scope>: <subject>` — scope is a crate directory or area: `tower-core:`, `of-auth:`, `tower-mcp:`, `tower-billing:`, `tower-trackers:`, `tower-web:`, `tower-server:`, `web:` (the SvelteKit console), `docs:`, `ci:`. Squash-merge to master via PR.                                                                                                                                                                                                                                                                                                                        |
| AI attribution             | **Never.** No `Co-Authored-By`, no "Generated with", no `🤖`/AI credit markers in commits, PR bodies, comments, or docs — direct work or subagent work (Non-Negotiable Rule 3).                                                                                                                                                                                                                                                                                                                                                                             |
| Spec storage               | **Repo file, committed.** `docs/specs/YYYY-MM-DD-<slug>-design.md` — never a tracker comment, never uncommitted.                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Plan storage               | **Repo file, committed.** `docs/plans/YYYY-MM-DD-<slug>.md` — same rule.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Plan format                | Read [`docs/plans/2026-09-01-milestone-1.md`](../../../docs/plans/2026-09-01-milestone-1.md) first and match it: a Goal paragraph, a `## Status — <date>` block, then one `## Task N — <name>` per task carrying a ✅ / 🚧 / ⬜ marker, with `- [ ]` steps in failing-test-first order, exact file paths, exact commands, and a final format-and-commit step. Each task's gate is `cargo test` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all`. Where the plan implements a committed spec, open with a `**Spec:** … read it first` line. |
| Record-as-shipped          | On completion, flip the spec's `> **Status:**` to IMPLEMENTED, update the relevant plan task's marker (✅, or 🚧 with a **Remaining** note) and the plan's `## Status` block, and commit as `docs: record <…> as shipped`. There is no archive directory — do not move the files.                                                                                                                                                                                                                                                                           |
| Conventions of record      | [`CLAUDE.md`](../../../CLAUDE.md) at the repo root is the load-bearing document — read it before any non-trivial change, and treat it as outranking anything here that has drifted. `README.md` carries the crate-state table, `docs/specs/2026-09-01-otto-factory-design.md` is the design of record, `docs/plans/2026-09-01-milestone-1.md` records where the build stands, and `docs/clients/matrix.md` records what each coding-agent client actually sends. Docs can still lag the code. Read the code.                                                |
| Test command               | `cargo test --workspace` (plain `cargo test` from the root is equivalent). Per suite: `cargo test -p tower-core --test isolation`, `--test queue`, `cargo test -p tower-mcp --test tools`. **Tests need a real Postgres** — `podman compose up -d` (Postgres 16 on host port 15433) and a `.env` with `DATABASE_URL` (`cp .env.example .env`). There are no database mocks, on purpose; `#[sqlx::test]` gives each test a fresh throwaway database with migrations applied.                                                                                       |
| Lint / format              | `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all`. **Run `cargo fmt --all` before every Rust commit** (`rust-toolchain.toml` pins the stable channel with rustfmt + clippy).                                                                                                                                                                                                                                                                                                                                                                |
| Console (`web/`)           | SvelteKit 2 / Svelte 5 runes / Tailwind v4, built with `adapter-static` — **not a Cargo crate**, so `cargo build/test --workspace` must never require node. Gates: `npm run check` (svelte-check + tsc over `worker/`), `npm run lint` (prettier), `npm test` (vitest over the Cloudflare Worker), `npm run build`. `tower-server` serves `web/build`, so an unbuilt console answers 404 on every page while the API works.                                                                                                                                    |
| CI                         | `.github/workflows/ci.yml` — two jobs: `rust` (fmt → clippy → `cargo test --workspace` against a Postgres 16 service on port 15433) and `web` (`npm ci` → check → lint → test). Runs on every PR and on pushes to `master`. **The merge gate is the CI run YOUR merge commit triggered, by run ID** — never "the latest run".                                                                                                                                                                                                                               |
| Deploy                     | Fly.io for the server (`Dockerfile`, `fly.toml`, [`docs/deploy/fly.md`](../../../docs/deploy/fly.md)) with a Cloudflare Worker in front (`web/wrangler.jsonc`, [`docs/deploy/cloudflare.md`](../../../docs/deploy/cloudflare.md)). Automatic: the `deploy` job in `.github/workflows/ci.yml` runs `flyctl deploy --remote-only -a otto-factory-mcp` on every push to `master` that passes `rust`+`web`+`docker-build`; `fly deploy -a otto-factory-mcp` remains the manual escape hatch for an out-of-band deploy.                                                                                                                                                          |
| Known pre-existing failure | None known at the time of writing (2026-09). Do not treat a pre-existing failure as a regression you caused; if CI was green on `master` before your branch, a new failure is yours.                                                                                                                                                                                                                                                                                                                                                                        |
| Versioning                 | Every crate shares the workspace `0.1.0` version and `rust-version = "1.88"`. Public-interface changes are governed by Non-Negotiable Rule 6: additive is the normal case and needs no bump; a breaking change is named in the spec and plan and flagged to the architect reviewer; an applied migration is never edited.                                                                                                                                                                                                                                   |

Full convention reference is [`CLAUDE.md`](../../../CLAUDE.md) at the repo root. The above is the
load-bearing subset for this workflow.

## Load-Bearing Invariants (get these right — every review step below checks them)

These are not style rules; they are the invariants this repository is built around. `CLAUDE.md`
explains the reasoning behind each at length — read it, and treat the list here as the checklist.

1. **Tenant isolation has two independent guards, and both are required.** Guard 1 is the API shape:
   tenant data is reachable only through `Tx`, which cannot be constructed without an `OrgId`, and
   every statement carries `org_id = $1` explicitly. Guard 2 is row-level security: `Db::begin`
   issues `SET LOCAL ROLE otto_app` **and** `SET LOCAL app.org_id`, and on managed Postgres (where
   `otto_app` cannot be created) `FORCE ROW LEVEL SECURITY` carries the guarantee instead —
   `Db::verify_tenant_isolation` reads back which shape it is in, and `tower-server` refuses to bind a
   port unless one of them holds. A new tenant table needs a `NOT NULL org_id`, an entry in the
   `tenant_tables` array in `0001_baseline.sql`, a policy named exactly `<table>_tenant_isolation`, and a
   **cross-org negative test**. Without the negative test it is not done.
2. **Ordinary cross-org tests pass on guard 1 alone.** The tests that actually exercise RLS are the
   `rls_scopes_*` ones in `crates/tower-core/tests/isolation.rs`, which issue deliberately unscoped SQL
   inside a pinned transaction. `#[sqlx::test]` connects as a superuser and bypasses RLS, so a test
   of a policy **must** `SET LOCAL ROLE otto_app` explicitly or it passes against no policy at all. A
   privilege granted to or revoked from `otto_app` is not a protection — express the rule as a policy.
3. **Every SQL statement lives in `tower-core`.** A query in `tower-mcp`, `tower-web`, `of-auth`, or
   `tower-billing` is a bug: it bypasses the `Tx` pinning that guard 2 depends on. `tower-core` has no
   HTTP and no auth; every tenant-scoped function takes an `OrgId`. **One exception:** the usage
   outbox's SQL lives in `tower-billing/src/outbox.rs`, because the row must be written in the
   tool's own pinned `Tx` by the crate that owns metering, and the shipper drains it across all
   orgs. It writes only through the caller's `Tx`; nothing else in `tower-billing` touches SQL, and
   the exception does not extend to any other table.
4. **Auth is passwordless and mail-less.** Passkeys (WebAuthn) for individuals, enterprise OIDC
   federation for orgs. **No password is ever accepted or stored, and no email is ever sent** —
   `users.email` is a unique key and an audit label, never a destination. A `Mailer` trait
   reappearing means somebody has reintroduced a dependency the design removed on purpose. Recovery
   is a second passkey: there are no recovery codes, and `passkeys::remove` refuses to delete the
   last credential. Signup takes **no request body at all**, which is what closes the
   account-enumeration oracle — never add a branch that reveals whether an account exists. Assisted
   recovery clears the passkeys and issues a claim code in one operation; splitting those two is an
   account-takeover race.
5. **The two `webauthn-rs` overrides are on the challenge, never on the verification state.**
   `require_resident_key(false)` and forcing `mediation: conditional` are what make usernameless,
   promptless sign-in work; removing either silently breaks it. Account resolution is by
   **credential ID**, never the user handle — the handle is the one field an authenticator may omit,
   and the signature is the only evidence.
6. **Tokens are opaque, stored only as SHA-256 hashes, and their org is fixed at issuance.** A token
   cannot be pivoted to another org. Redirect-URI matching is exact, with the RFC 8252 §7.3 loopback
   carve-out (`127.0.0.1` / `[::1]` / `localhost` ignore the port) — do not tighten that without
   checking `docs/clients/matrix.md` against a real client; removing it once silently killed Claude
   Code's OAuth path entirely.
7. **In `tower-mcp`, the caller comes from the request and the org comes from the token.** An MCP
   session spans many HTTP requests, so the `Principal` is introspected per request and never cached
   on the service — that is what makes revocation take effect on the next call. No tool takes an org
   argument. Every result is a one-field object defined in `tools::out`. Tool descriptions are the
   documentation, written for an LLM that has never read these docs, and `tests/tools.rs` asserts
   both the tool list and that every tool describes itself.
8. **In `tower-web`, authorization is an extractor, not a handler's first line.** `OrgCtx` resolves
   caller, org, and role before any handler body runs; `require_admin()` / `require_owner()` narrow
   it. **An org you are not in is `404`, never `403`** — a `403` turns any signed-in account into a
   directory of who uses the product. The router and the OpenAPI document are built from one list in
   `catalog.rs`, so a route not in the catalog is unreachable on purpose. **No credential is ever
   spent on a `GET`**: single-use redemptions are `POST`s behind a page with a button, and
   `every_single_use_redemption_is_a_post` asserts it. The session cookie's attributes
   (`HttpOnly`, `Secure`, `Path=/`, `SameSite=Lax`, `__Host-`) are asserted for the same reason.
9. **The console API is read-only over the queue.** Every job write belongs to `tower-mcp` — the agent
   doing the work is the only party that can say when it is done. `the_queue_is_read_only_over_the_console`
   fails if a write ever appears under `/jobs`.
10. **`web/` is a static SPA for a security reason.** The `__Host-` session cookie is bound to one
    origin, so no SvelteKit server may hold it; `adapter-static` with an `index.html` fallback keeps
    it in the browser, and `vite.config.ts` _proxies_ `/api`, `/oauth`, and `/.well-known` in
    development because a cross-port `fetch` would not carry the cookie. Nothing about the deployment
    is baked into the bundle — the MCP endpoint and grantable scopes are read at runtime from
    `/.well-known/oauth-protected-resource`. Runes only: `$state` / `$derived` / `$props` / `$effect`,
    no Svelte 4 stores, no `export let`. Every coding agent gets the same shape from
    `src/lib/clients.ts`; a bespoke wizard for one client is where agent-agnosticism breaks first.
11. **Metering bills completed jobs, inside the work's own transaction.** Every tool is free;
    every completed job (`complete_job` or an inbound ticket close) writes exactly one completion
    row through `tower_billing::record_job_completed`, in the same transaction, so a failed call is
    never billed and a successful one never double-billed (a repeat close is a no-op). A ticket
    close is billed only when the job was ever claimed (`attempts > 0`) and the org is not
    blocked; otherwise its row is unbilled. The shipper
    declares every meter in `METER_DECLARATIONS` before it ships. **A new tool must be classified
    in `tower-billing::classify`** — `exhaustive_over` and `every_tool_has_a_price` fail when the
    router and the price list disagree. Only `complete_job` can be refused, on the platform's
    `blocked_reason` (`budget_exceeded` | `payment_required`); a blocked org's tracker close is
    applied but unbilled. The status is looked up before the transaction (`Meter::warm`, on both
    paths), and a failed lookup lets the call through. `OF_ENFORCE_QUOTAS` is retired.
12. **Migrations are append-only, one file per concern, in `crates/tower-core/migrations/`.** Never
    edit a migration that has been applied anywhere — add a new one. (`0001_baseline.sql` is a
    one-time squash, done at the platform cutover; the schema is domain-only and holds no foreign
    key to anything the platform owns — `tests/guards.rs` enforces that.)
13. **`Watcher::spawn` detaches a connection for `LISTEN`, and dropping the pool does not reclaim
    it.** A `#[sqlx::test]` that spawns a watcher and never calls `Watcher::shutdown()` hangs at
    teardown instead of failing. `tower-server`'s graceful shutdown runs `watcher.shutdown().await`
    only after `axum::serve(...)` returns — keep that ordering.
14. **`tower-server` assembly has failures only it can produce.** Route collisions are a startup panic,
    and `the_whole_router_assembles` reaches it before a deployment does. The SPA fallback must never
    answer under `/api`, `/oauth`, `/mcp`, `/.well-known`, `/platform`, or `/webhooks`. `/healthz`
    never touches the database and `/readyz` always does (neither touches the platform). There is
    one shared `PlatformClient`, and a platform failure is a `503`, never a `401`.
15. **`Config::from_env` never falls back quietly.** A variable that is set but unparseable is a
    startup error naming it, not a default. `OF_PUBLIC_URL`, `OF_PLATFORM_URL`,
    `OF_INTROSPECTION_SECRET`, `OF_PLATFORM_WEBHOOK_SECRET`, and `OF_ENCRYPTION_KEY` have no
    defaults at all. Never log a secret, a token, or `OF_ENCRYPTION_KEY`; never echo one in an error or a
    response; never commit one.
16. **Errors are written for an LLM caller that has never read the docs**: what went wrong, what the
    valid options were, what to call next. `Error::code()` is the stable machine-readable branch
    point and `retriable()` tells an agent whether to back off. **No `unwrap()` outside tests**, and
    no silent fallback on a resolution failure — an unresolvable repo is an error naming the
    registered slugs, never a guess. Comments explain **why**, especially where the obvious
    implementation is wrong.

## Tracker abstraction (GitHub Issues or ticketless)

Savvagent Tower uses **GitHub Issues when an issue exists, ticketless otherwise.** There is no JIRA.
Resolve once at intake and stay on that path for the whole task.

> **One-time bootstrap.** The `status:*` tracker labels below are NOT GitHub defaults, and this repo
> currently carries only the stock label set. Create them once before the first tracked issue
> (`gh label create status:in-progress --repo savvagent/otto-factory`, likewise `status:in-review`),
> or the `--add-label` transitions will error ("could not find label").

| Lifecycle step     | GitHub Issues                                                                     | Ticketless                                                                |
| ------------------ | --------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| Ref form           | `savvagent/otto-factory#123` (`#123` short)                                       | the captured task brief                                                   |
| Intake / read AC   | `gh issue view <n> --repo savvagent/otto-factory --json title,body,labels`        | the user's instruction, captured verbatim in working memory + the PR body |
| → In Progress      | `gh issue edit <n> --repo savvagent/otto-factory --add-label status:in-progress`  | n/a — capture `T_impl_start` only                                         |
| → In Review        | `gh issue edit <n> --repo savvagent/otto-factory --add-label status:in-review`    | n/a                                                                       |
| Spec / Plan record | committed to `docs/specs/` / `docs/plans/`; reference the paths from the PR body  | same — the repo docs ARE the durable record                               |
| Close              | `gh issue close <n> --comment "…"` (the merged PR's `Closes #N` may have done it) | n/a — the Phase 6 summary is the close-out                                |
| Branch             | `<area>/<slug>`                                                                   | `<area>/<slug>`                                                           |
| Commit subject     | `<scope>: <subject>`                                                              | `<scope>: <subject>`                                                      |
| PR linkage         | body contains `Closes #N`                                                         | body restates the task brief as the AC                                    |

On the ticketless path there is no worklog sink, so the Phase 6 summary's timeline is the only time
record. `T_*` timestamps are still captured at phase boundaries to feed it.

## Phase 0 — Pre-flight (fresh context)

Intake reads ("what does this work want?") get corrupted by prior conversation cruft — stale paths,
abandoned plans, half-finished refactors.

**Two valid paths to fresh context:**

1. **Subagent dispatch** (default mid-conversation). Use the `Agent` tool with a self-contained
   prompt: issue number or task brief + "follow otto-tower-development end-to-end" + any caller
   constraints. The subagent's context is fresh by construction; the parent session sees only the
   summary string. **When this path is used, see "Adaptation: when this skill runs inside a
   subagent" below** — interior reviewer dispatches collapse to named inline passes because a
   subagent cannot recursively dispatch.
2. **`/clear` + re-invoke** when staying in-session is preferable. **Preferred path when the highest
   review quality matters** — interior `Agent` dispatches work as designed only when this skill runs
   in the main thread.

**Not clean context:** hooks, memory entries, `additionalContext`, new files. Only a fresh process
or a fresh subagent qualifies.

**Branch + worktree safety:**

1. Run `git branch --show-current` in the current working directory.
2. **Trunk-sync check (mandatory).** Before any worktree creation, verify local trunk is in sync with
   origin:
   ```bash
   git fetch origin
   git rev-list origin/master..master            # MUST be empty
   ```
   If it returns commits, **local master is ahead of origin** — those commits will silently inherit
   into the new branch and contaminate the PR diff against `origin/master`. Surface them to the user
   (commits + their file paths); do NOT discard. They represent unpushed work that needs handoff
   BEFORE the new worktree is created.
3. If on `master` (or any trunk), create the worktree branching from `origin/master` explicitly (NOT
   local `master`) — belt-and-suspenders against step 2's check ever drifting:
   ```bash
   git worktree add .worktrees/<branch> -b <branch> origin/master
   ```
   Worktrees live inside the repo at `.worktrees/<branch>`. Never code on `master` directly.
4. If already on a feature branch in a worktree → proceed there.
5. `git status --porcelain` must be clean in the worktree before any code edit. Surface unexpected
   uncommitted changes; do NOT discard them.
6. **`master` is write-protected by policy.** The only way a commit reaches `master` is a reviewed,
   merged PR. Never commit to the main checkout's branch, never `git push origin master`, never merge
   a branch into `master` by hand. If the work needs to touch `master` (e.g. the record-as-shipped
   commit), it does so through its own worktree + PR like everything else.

Create TodoWrite todos for each phase (1–6) and check them off as you go.

## Adaptation: when this skill runs inside a subagent

Phase 0 lists subagent dispatch as a valid path to fresh context. But **a subagent's deferred-tool
set does NOT include the `Agent` tool** — a subagent cannot recursively dispatch further subagents.
This affects every interior reviewer/implementer dispatch (Phase 1 step 4, Phase 2 step 6, Phase 3
steps A/C/E/H, Phase 4 step 9).

When the orchestrator IS itself a subagent (Phase 0 path 1), apply these adaptations:

**Interior reviews run as named inline passes.** The orchestrator-subagent itself produces the spec
critique, plan critique, implementer report, spec-compliance report, and code-quality report — each
as a clearly delimited section written to the same prompt-template specification the `Agent` dispatch
would have used. The named output and acceptance criteria from `agent-prompts.md` still apply; only
the dispatch mechanism changes.

**Implementer "dispatch" collapses to direct execution.** The orchestrator-subagent is the
implementer. Apply the AUTONOMOUS MODE block to itself. Model-selection rules lose force — the
orchestrator runs at whatever model the parent dispatched it with.

**pr-review-toolkit dispatches are deferred to the parent.** The orchestrator-subagent cannot
dispatch `pr-review-toolkit:*` agents. It attaches the automated reviewer via `gh` CLI, then reports
the list of agents the parent must dispatch as a `Reviewers to dispatch from parent:` field in its
final report.

**The mandatory review trio (Phase 4 step 8) is also deferred to the parent.** A subagent cannot
dispatch `rust-pro` / `architect-reviewer` / `security-auditor`. The orchestrator-subagent MUST list
all three as required `Reviewers to dispatch from parent:` and MUST NOT merge — or declare the PR
mergeable — until the parent confirms all three reviews cleared. Non-Negotiable Rules 4–5 admit no
exception for the subagent path; deferring the trio is how they are honored there.

**Review-response subagent (Phase 4 step 9) is also deferred.** If the parent specified a halt point
at step 8, the orchestrator stops there cleanly. If invoked end-to-end with no parent halt point, it
MUST run the review-response work inline — same inline-pass discipline.

**Tradeoff.** Inline review by the same orchestrator that did the work loses the fresh-context
isolation that's the point of separate reviewer subagents. This is documented and acceptable, but not
equivalent. **For the highest-quality reviews, prefer Phase 0 path 2 (`/clear` + re-invoke in the
main thread).**

## Adaptation: when the dispatch tool exposes only generic subagent types

Some runtimes give the orchestrator a fixed, small set of subagent types — e.g. opencode's `task`
tool exposes `explore` and `general` only, with no `rust-pro`, `architect-reviewer`,
`security-auditor`, `code-reviewer`, or `pr-review-toolkit:*` — even though the reviewer agent
definitions exist on disk (`~/.claude/agents/*.md`). When that is the case, the review trio and
the pr-review-toolkit passes are still run; only the dispatch mechanism changes:

- **Dispatch each named reviewer as a `general` subagent carrying that reviewer's prompt body
  verbatim** from `agent-prompts.md` (the "Mandatory Review Trio" section). The reviewer's
  identity lives in the prompt, not in the `subagent_type` string. Fill `<N>` / `<ref>` exactly
  as the template says.
- **The independent security review stays blind by construction.** The `general` security
  subagent is still given ONLY `gh pr diff <N>` — never the spec, plan, task brief, PR-body
  summary, or implementer's report (Non-Negotiable Rule 5). State it explicitly in that
  dispatch: "Do not read the PR description, issue, spec, or plan." Keep the "blind" instruction
  attached to the review itself so the fallback cannot silently drop it.
- **`pr-review-toolkit:*` passes collapse to `general` subagents** carrying each toolkit agent's
  responsibility inline (from the Phase 4 step 8 trigger table), or are dropped when a `gh`
  automated reviewer already covers the same ground.
- **`general-purpose` vs `general`:** the skill's `subagent_type: general-purpose` (and
  `code-reviewer`) is Claude Code's name; map it to whatever the runtime actually calls its
  generic agent type (`general` in opencode). The prompt content, not the type string, is
  load-bearing.

This is NOT the "runs inside a subagent" adaptation above — here the orchestrator is the main
agent; its subagent tool simply lacks the named types. Review content and the blind rule are
preserved; only the type name changes.

## Phase 1 — Intake + spec

> **Fast-path note:** Steps 1 + 2 always run. Steps 3 + 4 (spec draft + critique) are skipped if the
> work qualifies under "Fast-Path: Trivial Tasks." When fast-pathing, jump from step 2 directly to
> Phase 2 step 5 and write the minimal single-task plan.

### Step 1: Read the source directly

- **GitHub:** `gh issue view <n> --repo savvagent/otto-factory --json title,body,labels` — the body is the AC.
- **Ticketless:** capture the user's instruction verbatim in working memory. That string is the AC
  for the rest of the run; restate it in the PR body so the contract is durable.

If a teammate summarized it, still read the source — summaries lose AC.

### Step 2: Transition / mark In Progress

- **GitHub:** `gh issue edit <n> --repo savvagent/otto-factory --add-label status:in-progress`.
- **Ticketless:** nothing to transition.

**Capture `T_impl_start = now`** in ISO-8601 with explicit timezone offset. Hold for the Phase 6
summary timeline.

### Step 3: Spec draft (committed, no human review)

**Unlike general-development, the spec IS a repo file in this repository** — the plan-by-plan
convention requires it. Create `docs/specs/YYYY-MM-DD-<slug>-design.md`, following the structure of
[`docs/specs/2026-09-01-otto-factory-design.md`](../../../docs/specs/2026-09-01-otto-factory-design.md)
(read it, or the most recent spec, first):

- Title: `# <Change> design` — descriptive, sentence case
- Status blockquote at top, e.g. `> **Status:** DRAFT — <one-line summary>` (flipped to IMPLEMENTED at close-out)
- Optional `> **Implements:**`, `> **Depends on:**`, `> **Blocks:**` links when the change is a phase of a larger plan
- **Premise corrections** — if the task brief's premises do not survive contact with the repository
  (common here; `CLAUDE.md`, `README.md`, and the milestone plan can each be ahead of or behind the
  code), record the corrections explicitly instead of silently building to the wrong premise
- **Scope** with **In:** and **Out:** — explicit non-goals. Check the change against the three
  constraints in `CLAUDE.md` here: repo-anchored coordination, substrate-not-workflow, coding-agent
  agnostic. A capability that could live in a customer's skill instead of the server belongs in the
  skill, and saying so in **Out:** is the point of this section
- Numbered sections (§1, §2, …) for each component: shape, configuration, security properties,
  testing. Cite `file:line` references to existing code where the design touches it
- **Tenant isolation** — if the change adds or touches a tenant table, name the `org_id` column, the
  `0001_baseline.sql` registration, the `<table>_tenant_isolation` policy, and the cross-org negative test
- **Metering** — if the change adds an MCP tool, name its `tower-billing::classify` classification

Required sections, wherever they fit: **Assumptions** (every choice made without asking, each with a
one-line rationale — the highest-value section), **Goal & Success Criteria** (one paragraph + 3–5
measurable bullets), **Error Handling & Edge Cases**, **Risks & Open Questions**.

**Commit the spec draft** as `docs: add <slug> design spec` before critique. The critique loop below
revises the _committed file_ in place (new commits per round), never a working-memory-only copy.

### Step 4: Spec critique subagent

Dispatch using the **`Spec Critique`** template in [`agent-prompts.md`](agent-prompts.md) — **read
that file now and paste the template verbatim; do not improvise the prompt body.**
`subagent_type: general-purpose`. Fill `<PASTE FULL SPEC TEXT>` from the committed spec and cite the
source ref. In the Repo Profile placeholder, paste the "Load-Bearing Invariants" section above and
the relevant Repository Conventions.

**Maximum 2 revision rounds (3 reviewer dispatches total).** On Issues Found, revise the spec in the
committed file and redispatch with the updated text inline. If issues remain after the third pass,
append them to `Risks & Open Questions` and continue. Do NOT loop further.

**When the loop converges, commit the approved version** (once) and note the spec path in the plan
and the PR body.

## Phase 2 — Plan

> **Fast-path note:** On a fast-path ticket, write the minimal single-task plan directly — skip the
> critique loop.

### Step 5: Plan draft (committed, no human review)

Write the plan in this repository's established format — read
[`docs/plans/2026-09-01-milestone-1.md`](../../../docs/plans/2026-09-01-milestone-1.md) first and
match it:

- Title: `# <Change> — <one-line goal>`
- **Goal** paragraph, plus a `## Status — <date>` block stating what is done and what remains
- **Spec:** line pointing at the committed design spec — "read it first. This plan implements it exactly."
- **Global Constraints** — the invariants that hold for every task: the applicable Load-Bearing
  Invariants above, "no AI self-attribution", "run `cargo fmt --all` before every Rust commit",
  "every SQL statement lives in `tower-core`", "tests need `podman compose up -d` and a `.env`"
- **File Structure** table — `File | Responsibility`, each row prefixed **Create.**/**Modify.**
- **Task Order & Rationale** — why the tasks run in this order
- One `## Task N — <name>` per task, carrying a ⬜ marker and listing **Files:** and **Interfaces:**
  (consumes/produces), then `- [ ]` steps in **failing-test-first order**: write failing test → run
  → implement → run → commit. Include exact file paths, exact commands
  (`cargo test -p tower-core --test isolation`, `cargo test -p tower-mcp --test tools`, `npm run check`),
  and the expected result of each run

Every task MUST include:

- Exact file paths in THIS repo's layout
- **Reminders for the out-of-band artifacts** from Phase 5 that the task touches (container image,
  console bundle, Cloudflare Worker, migrations)
- **A cross-org negative test step whenever the task touches a tenant table or a tenant-scoped
  function** — per Load-Bearing Invariant 1, without it the task is not done
- **A `tower-billing::classify` step whenever the task adds an MCP tool** — `every_tool_has_a_price`
  fails otherwise
- **A breaking-change step when the task changes a public interface in a non-additive way:** a
  `- [ ]` step recording the break in the spec, in `docs/clients/matrix.md` where a client sees it,
  and in the PR body for the architect reviewer (Non-Negotiable Rule 6). Additive changes need none,
  and an applied migration is never edited — add a new one
- The repo's actual test + lint commands for the TDD steps
- A final "Format and commit" step: `cargo fmt --all` + `git commit -m "<scope>: <subject>"`

**Commit the plan draft** as `docs: add <slug> implementation plan` before critique.

### Step 6: Plan critique subagent

Dispatch using the **`Plan Critique`** template in [`agent-prompts.md`](agent-prompts.md) — **read
that file now and paste the template verbatim.** `subagent_type: general-purpose`. Fill `<PASTE FULL
PLAN TEXT>`, `<PASTE FULL SPEC TEXT>`, and the Repo Profile (Load-Bearing Invariants + conventions +
test/lint commands).

Same revision-loop shape as step 4 (revise the committed file, redispatch with the updated text
inline). **Maximum 2 revision rounds.** If unresolved issues remain, prepend a `## Known Plan Gaps`
section and continue.

**When the loop converges, commit the approved version** (once).

## Phase 3 — Implement

Read the plan ONCE, then extract every task's full text + context into your own working memory.
Create one TodoWrite entry per task. **Do NOT make implementer subagents re-read the plan** —
provide them the full task text inline.

**Sequential, not parallel.** Implementer subagents on the same branch will conflict on the working
tree. Parallelism happens across separate worktrees on separate features, not within one run.

For each task in plan order:

### A. Dispatch implementer

Dispatch using the **`Implementer Dispatch`** template in [`agent-prompts.md`](agent-prompts.md) —
**read that file now and paste the template verbatim** (the `## AUTONOMOUS MODE` block and `## Report
Format` are load-bearing). `subagent_type: general-purpose`. Fill the task text, the Context block
(include the relevant Load-Bearing Invariants + the repo's test/lint commands), the source ref, and
the worktree path.

**Model selection:**

- Mechanical 1–2-file tasks with complete specs → `model: "haiku"`
- Multi-file integration work → omit (inherit parent)
- Design-judgment tasks the plan explicitly flags → `model: "opus"`

### B. Handle implementer status

| Status               | Action                                                                                                                                  |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `DONE`               | Proceed to spec compliance review (step C)                                                                                              |
| `DONE_WITH_CONCERNS` | If correctness/scope: dispatch fix subagent now with the specific concern as the new task. If minor: log in per-task ledger and proceed |
| `NEEDS_CONTEXT`      | If discoverable in the repo, re-dispatch with the context filled in. If genuinely unknowable: treat as BLOCKED                          |
| `BLOCKED`            | See Stop & Escalate below                                                                                                               |

### C. Spec compliance review

Dispatch using the **`Spec Compliance Review`** template in [`agent-prompts.md`](agent-prompts.md) —
**read that file now and paste the template verbatim** (the `## CRITICAL: Do Not Trust The Report`
block is load-bearing — the reviewer must read the actual commits, not the implementer's claims).
`subagent_type: general-purpose`. Fill the task text and the implementer's report verbatim.

### D. Spec fix loop (max 2 fix dispatches)

- ✅ → quality review (step E).
- ❌ → re-dispatch implementer with status `FIX_SPEC_ISSUES`, supplying the reviewer's findings as
  the new task. Re-run spec review.
- **Three failed spec reviews in a row → escalate.**

### E. Code quality review

Capture commit boundaries:

- `BASE_SHA = git rev-parse HEAD~<N>` where N = commits this task produced
- `HEAD_SHA = git rev-parse HEAD`

Dispatch using the **`Code Quality Review`** template in [`agent-prompts.md`](agent-prompts.md) —
**read that file now and paste the template verbatim.** `subagent_type: code-reviewer` (NOT
general-purpose). Fill `<BASE_SHA>`/`<HEAD_SHA>` and the task text.

### F. Quality fix loop (max 2 fix dispatches)

- No Critical/Important → mark task complete in TodoWrite; record any Minor issues in per-task ledger.
- Critical/Important → re-dispatch fixer with those specific findings. Re-run quality review.
- **Three failed quality reviews in a row → escalate.**

**Pragmatism rule:** Minor / optional suggestions ≠ blockers. Treat code-quality "Approved with
suggestions" as DONE; do not auto-dispatch fix loops for non-blocking suggestions.

### G. Per-task ledger

Maintain an internal running record per task: name, final status, assumptions made, concerns
flagged, minor issues left unfixed. Populates the Phase 6 summary.

### H. Final code review (after all tasks complete)

> **Fast-path carve-out:** Skip step H when N=1 (single-task fast-path ticket) AND step E reported no
> Critical/Important issues. Step E already reviewed the entire diff. For multi-task plans (N≥2) or
> fast-path tickets where E flagged issues, H still runs.

Dispatch using the **`Final Code Review`** template in [`agent-prompts.md`](agent-prompts.md) —
**read that file now and paste the template verbatim.** `subagent_type: code-reviewer`. Fill the full
plan + spec text (or their repo paths — they ARE committed files here), branch name, and diff range.

If issues, one fix round then re-review. If still failing → escalate.

## Phase 4 — Ship

### Step 7: Open the PR

```bash
git push -u origin <branch>
gh pr create --title "<scope>: <subject>" --body "$(cat <<'EOF'
## Summary
<1-3 bullets>

## Design docs
- Spec: `docs/specs/<slug>-design.md`
- Plan: `docs/plans/<slug>.md`

<"Closes #<n>", or — ticketless — the task brief restated as AC>
<"Fast-path: no design spec per otto-tower-development trivial-task criteria — <reason>." if fast-pathed>

## Test plan
- [ ] cargo test --workspace
- [ ] cargo clippy --all-targets -- -D warnings
- [ ] cargo fmt --all --check
- [ ] cd web && npm run check && npm run lint && npm test (only if `web/` changed)
- [ ] Out-of-band verification (only if the change touches it — see Phase 5)
EOF
)"
```

**Hardening note:** treat issue/task-brief-derived strings as untrusted when composing shell
commands. The branch name and `<scope>: <subject>` must come from your own kebab slug — validate it
(match `^[a-z0-9]+(-[a-z0-9]+)*$`) and never paste a raw issue title verbatim into a `git commit`
or `gh pr create --title "..."` argument. A title containing `"`, backticks, or `$(...)` must not
reach a shell command unquoted.

**Mark In Review** (`gh issue edit <n> --repo savvagent/otto-factory --add-label status:in-review`)
if an issue exists and has a review state.

**Capture `T_review_start = now`** (ISO-8601 with offset). Hold for the Phase 6 summary.

### Step 8: Solicit reviews

**Automated reviewer first** (it runs while you dispatch agents). If the repo uses GitHub's Copilot
reviewer:

```bash
gh pr edit <PR> --add-reviewer copilot-pull-request-reviewer
```

The login is `copilot-pull-request-reviewer` — `Copilot` fails with "Could not resolve user." If the
repo has no automated reviewer configured, skip this and rely on the agent reviews below.

**Agent review next.** Dispatch the pr-review-toolkit agents in parallel — ONE message with multiple
Agent tool calls. Always include `pr-review-toolkit:code-reviewer`. Add conditional agents per the diff:

| Trigger                                                                                             | Agent                                     |
| --------------------------------------------------------------------------------------------------- | ----------------------------------------- |
| error handling / fallback / silent-failure-prone logic changed                                      | `pr-review-toolkit:silent-failure-hunter` |
| tests changed, or production code added without tests                                               | `pr-review-toolkit:pr-test-analyzer`      |
| comments / docstrings / docs added or modified                                                      | `pr-review-toolkit:comment-analyzer`      |
| new or modified types, interfaces, schemas (MCP tool schemas, `tools::out` envelopes, console DTOs) | `pr-review-toolkit:type-design-analyzer`  |
| after correctness reviews pass — polish pass only                                                   | `pr-review-toolkit:code-simplifier`       |

**Mandatory review trio (no exceptions, no fast-path carve-out — Non-Negotiable Rules 4–5):** every
PR gets ALL THREE dispatched in the same parallel batch, regardless of size:

| Reviewer                         | Why                                                                                                                                                                                                                                                                                                   |
| -------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rust-pro` (Rust expert)         | Idiomatic Rust: ownership/lifetimes, error handling, no `unwrap()` outside tests, no panics on untrusted input, `Send + Sync` async seams, sqlx usage, test placement — against this repo's conventions (`#[sqlx::test]` against real Postgres, no database mocks, errors written for an LLM caller). |
| `architect-reviewer` (architect) | Architectural consistency: all SQL in `tower-core`, crate boundaries (`tower-core` → `of-auth`/`tower-billing` → `tower-mcp`/`tower-web` → `tower-server`), the two tenant-isolation guards, the console API staying read-only over the queue, the three constraints in `CLAUDE.md`, spec/plan alignment.               |
| `security-auditor` (independent) | Security of the actual diff. **Receives ONLY the diff** — never the spec/plan/brief/PR-body summary (Non-Negotiable Rule 5). See the Independent Security Review template in `agent-prompts.md`.                                                                                                      |

The rust-pro and architect-reviewer reviews the actual diff (commit range), not the summary. Treat
their findings like any other review: Critical/Important must be fixed or explicitly dismissed before
merge; all three must clear before merge (step 11). If the subagent tool has no named reviewer
types, fall back to `general` subagents per "Adaptation: when the dispatch tool exposes only
generic subagent types" — the trio is still mandatory, never skipped.

Add ad-hoc domain agents (`frontend-developer` for `web/`) on top by topic if the change warrants
it. The independent `security-auditor` pass above already covers every PR; for changes touching
tenant isolation (`Tx`, `OrgCtx`, RLS policies, a new tenant table), the auth spine (passkey
ceremonies, the OAuth AS, token hashing, sessions, PATs, redirect-URI matching), or the MCP
resource-server middleware, give it the extra instruction to scrutinize those invariants hardest.

Aggregate the agent reports into a single PR comment grouped by **Critical / Important / Suggestions
/ Strengths** so review threads stay flat instead of one comment per agent.

### Step 9: Fork a review-response subagent

Once reviews have posted, immediately dispatch a dedicated review-response subagent with fresh
context to avoid polluting the main thread with review-fix churn.

Dispatch using the **`Review-Response Subagent`** template in [`agent-prompts.md`](agent-prompts.md)
— **read that file now and paste the template verbatim** (the fix-or-dismiss + thread-resolve
mutation + inline-reply requirements are load-bearing). `subagent_type: general-purpose`. Fill the PR
number `<N>` and the source ref.

### Step 10: PR review loop

Human reviewers post on their own schedule. Each new round forks a NEW review-response subagent.
Continue until merged.

**Idempotency required.** Every iteration must be safe to no-op. First action of any loop iteration:
enumerate unresolved threads:

```bash
gh pr view <PR> --json reviewDecision,reviews,statusCheckRollup
gh api repos/savvagent/otto-factory/pulls/<PR>/comments
# GraphQL: pullRequest.reviewThreads(first: 100) { nodes { id isResolved comments { ... } } }
```

A comment is "new since last pass" if its thread is unresolved AND your last reply (if any) is older
than the latest comment in that thread. If zero new, exit cleanly — no commits, no replies, no
tracker writes.

> **The merge gate is the CI run YOUR merge commit triggered, by run ID.** `.github/workflows/ci.yml`
> runs the `rust` job (fmt → clippy → `cargo test --workspace` against a Postgres service) and the
> `web` job (check → lint → test) on every PR. Capture the run id
> (`gh run list --repo savvagent/otto-factory --branch <branch> --limit 5`) and track THAT id —
> "the latest run" is a teammate's merge seconds after yours.

| Iteration state                                                              | Action                                                     |
| ---------------------------------------------------------------------------- | ---------------------------------------------------------- |
| All threads resolved + your CI run green + approval present                  | Merge (step 11)                                            |
| All threads resolved + your CI run green + awaiting human approval           | Idle. Next iteration no-ops                                |
| Open threads you cannot address (security / missing requirement / ambiguous) | Escalate per Stop & Escalate                               |
| Same unresolved thread across multiple iterations after you replied          | Wake the human reviewer. Do NOT silently retry             |
| Your CI run failed                                                           | Treat as "fix this" comment. Address before next iteration |

### Step 11: Merge

**Run `gh pr merge` from the main checkout, NOT from inside the worktree.** From-worktree merge can
corrupt main-checkout staging.

```bash
cd <main-checkout-root>
gh pr merge <PR> --squash --delete-branch    # or --merge per repo convention
```

**Capture `T_pipeline_start = now`** (ISO-8601 with offset).

### Step 12: Clean up + record-as-shipped

```bash
cd <main-checkout-root>
git checkout master && git pull --ff-only
git branch -d <branch>                        # -D only if force needed and intentional
git worktree remove .worktrees/<branch>       # required — worktrees live inside the repo
```

**Record-as-shipped (mandatory, do not skip):** in a fresh worktree off the updated `master` — the
same `.worktrees/<branch>` + PR flow as the feature itself, never a direct commit to the main
checkout — flip the spec's `> **Status:**` to `IMPLEMENTED`, update the affected plan task's marker
(✅, or 🚧 with a **Remaining** note) and the plan's `## Status` block, then commit as
`docs: record <…> as shipped` and merge via PR. The files stay where they are; there is no archive.
This is how the plan-by-plan record stays current — the docs ARE the project's history.

## Phase 5 — Deploy, verify, close

**CI exists; a deploy pipeline does not.** Savvagent Tower is one Axum binary (`tower-server`) deployed to
Fly.io from `Dockerfile` + `fly.toml`, with a Cloudflare Worker in front (`web/wrangler.jsonc`) and
PostgreSQL behind it, migrated at startup by `tower-core`'s embedded migrations. `.github/workflows/ci.yml`
gates every PR and every push to `master`; deploys are manual and out of band. Phase 5 is: confirm
YOUR CI run is green, run the out-of-band checklist for what the change touched, then close.

### Step 13: Confirm YOUR merge commit's CI run is green

```bash
gh run list --repo savvagent/otto-factory --branch master --limit 5   # find the run for YOUR merge SHA
gh run watch <run-id> --repo savvagent/otto-factory
```

Track the run by ID, never "the latest run" — a teammate's merge seconds after yours steals it. Both
jobs (`rust` and `web`) must pass.

**Capture `T_verify_start = now`.**

### Step 14: Out-of-band artifact verification

CI does NOT apply these. Verify whatever the change touched, explicitly:

- **Container image** — if `Dockerfile` or `fly.toml` changed: build it. **No docker on this machine;
  podman is available** (`podman build -t otto-tower .`). The image has a console stage, a rust
  stage, and a slim runtime; confirm the binary starts, answers `/healthz` and `/readyz`, and serves
  the console (the console stage must have produced `web/build`, or every page 404s).
- **Console bundle** — if `web/` changed: `cd web && npm run check && npm run lint && npm test &&
npm run build`. Confirm nothing about the deployment got baked into the bundle (the MCP endpoint
  and grantable scopes are read at runtime from `/.well-known/oauth-protected-resource`), and that
  the dev proxy in `vite.config.ts` still covers `/api`, `/oauth`, and `/.well-known`.
- **Cloudflare Worker** — if `web/worker/` or `web/wrangler.jsonc` changed: `npm test` (vitest is the
  Worker's routing gate) and re-read [`docs/deploy/cloudflare.md`](../../../docs/deploy/cloudflare.md).
- **Database migrations** — if `crates/tower-core/migrations/` changed: confirm the new file is
  _additive and new_ (an already-applied migration is never edited), that `0001_baseline.sql` still runs
  last, and that a fresh cluster applies cleanly — `podman compose down -v && podman compose up -d`
  then `cargo test -p tower-core`. A new tenant table must appear in `0001_baseline.sql`'s `tenant_tables`
  with a `<table>_tenant_isolation` policy and a cross-org negative test, or
  `Db::verify_tenant_isolation` will not vouch for it at startup.
- **CI** — if `.github/workflows/` changed: confirm the workflow parses and the jobs actually ran on
  this PR (`gh run list --repo savvagent/otto-factory --branch <branch>`).
- **Config surface** — if a `OF_*` variable was added or changed: update `.env.example` with the
  _why_, and confirm `Config::from_env` errors (never silently defaults) on an unparseable value.

A vacuously-satisfied item ("no deploy/distribution change in this PR") is satisfied, not skipped —
state it explicitly.

### Step 15: Production / target verification

The target is the local workspace plus `master`'s own CI. For a change that shipped, the smoke is:
your merge commit's CI run green, plus the step-14 out-of-band items. For user-facing surfaces, run
the binary once (`podman compose up -d && cargo run -p tower-server`, with `web/` built) and exercise
the changed surface: the console page for a `web/` or `tower-web` change, an MCP tool call over a
personal access token for a `tower-mcp` change, a sign-in ceremony for a `of-auth` change.

### Step 16: Close

- **GitHub:** `gh issue close <n> --repo savvagent/otto-factory --comment "<summary>"` (the merged
  PR's `Closes #N` may have closed it already — verify with
  `gh issue view <n> --repo savvagent/otto-factory --json state`).
- **Ticketless:** no close action — the Phase 6 summary is the close-out.

Close-out summary:

```
Shipped.

PR: <url>
Out-of-band applied: <image/console-bundle/worker/migrations/config, or "none">
Smoke: <one-line outcome>
```

## Phase 6 — Final summary

Output a single concise message:

```
otto-tower-development complete.

Source: <issue #> / task brief — <title> — Closed
PR: <url>
Branch: <branch> (deleted, worktree removed)
Spec: docs/specs/<slug>-design.md
Plan: docs/plans/<slug>.md
Tasks completed: N / N
Commits: <count>
Timeline: <T_impl_start → T_review_start → T_pipeline_start → T_verify_start, or "n/a">

Out-of-band applied: <list, or "none">

Assumptions worth reviewing (from spec + per-task ledger):
- <bullet>
(up to 5)

Minor issues left unaddressed (intentional, low-priority):
- <bullet, or "none">

Final reviewer assessment: <Ready / Needs follow-up — details>
```

Then STOP. Do not pick up the next task. Do not offer to chain another run.

## Stop & Escalate

Stop the pipeline and return control to the developer when ANY of these is true:

1. A task is BLOCKED and re-dispatching with more context did not unblock it after one retry.
2. A task fails spec review three times in a row.
3. A task fails quality review three times in a row (with Critical or Important issues).
4. Test infrastructure is broken in a way that prevents verifying any task.
5. The plan has internal inconsistencies (a later task assumes a structure earlier tasks didn't produce).
6. The pipeline has run for unreasonable wall-clock time and is making no progress.
7. The AC contradicts the spec/plan you built (mid-flight requirements change).
8. An agent review surfaces a security finding (auth, injection, secrets, PII) — especially one
   touching tenant isolation, the passkey ceremonies, the OAuth authorization server, token hashing,
   session cookies, or the MCP resource-server middleware.
9. A proposed change would cross a tenant boundary, add SQL outside `tower-core`, add a tenant table
   without an RLS policy and a cross-org negative test, answer `403` where the product answers `404`,
   spend a credential on a `GET`, leak a secret into logs or a response, persist a raw token, edit an
   applied migration, reintroduce a mailer or a password, or add a client-specific dependency — these
   are not negotiable design choices. Neither is a change that violates one of the three constraints
   in `CLAUDE.md`.
10. Out-of-band verification (Phase 5 step 14) fails after a green merge.
11. The same bug pattern is discovered elsewhere — file a follow-up issue; do NOT silently widen scope.
12. An out-of-band prerequisite from another change (an unmerged plan, an unbuilt image, an unshipped
    migration) is missing.

On escalation, output:

```
otto-tower-development halted at Phase <N> — <step name>.

Reason: <one of the conditions above, with specifics>
Source: <issue/brief>
Branch: <branch>
Worktree: .worktrees/<branch>
PR: <url, if open>
Last successful step: <step name>
Commits so far: <git log --oneline since branch point>
Recommended next step: <suggestion>
```

Then STOP. Do not push, do not open a PR, do not merge, do not close.

Speed pressure does not eliminate any step. It can require escalation; it never authorizes skipping.

## Calibration vs. Skipping

Within each step you may calibrate effort to risk. You may NEVER eliminate a step.

| Step                                                  | Cheapest valid form for a small change                                                           | Skip?                                                |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------ | ---------------------------------------------------- |
| Convention reference                                  | Skim CLAUDE.md + the latest plan + test command                                                  | NEVER                                                |
| Read source                                           | 20-second skim of description + AC                                                               | NEVER                                                |
| Spec draft                                            | 1-page spec with Assumptions + Brief + Scope (committed)                                         | **Only** if fast-path criteria met                   |
| Spec critique                                         | 1 reviewer dispatch                                                                              | **Only** when the spec is skipped under fast-path    |
| Plan draft                                            | Minimal single-task plan (committed)                                                             | NEVER — house style keeps the plan even on fast-path |
| Plan critique                                         | 1 reviewer dispatch                                                                              | **Only** when the plan is fast-pathed (trivial task) |
| Worktree at start                                     | `git worktree add .worktrees/<branch> -b <branch> origin/master`                                 | NEVER (never code on master)                         |
| Master-branch writes                                  | reviewed, merged PR (squash) — incl. record-as-shipped                                           | NEVER (no direct push/hand-merge)                    |
| Implementer dispatch                                  | 1 Agent call with full task text inline                                                          | NEVER                                                |
| Spec compliance review                                | 1 reviewer dispatch reading actual commits                                                       | NEVER                                                |
| Quality review                                        | 1 code-reviewer Agent dispatch                                                                   | NEVER                                                |
| Automated reviewer                                    | 1 `gh pr edit --add-reviewer …`                                                                  | Only if the repo has none configured                 |
| Rust expert + architect + independent security review | 1 parallel Agent dispatch each (`rust-pro`, `architect-reviewer`, blind-diff `security-auditor`) | NEVER                                                |
| pr-review-toolkit agents                              | 1 parallel Agent dispatch                                                                        | NEVER                                                |
| Review-response subagent                              | 1 Agent dispatch with PR# + ref                                                                  | NEVER                                                |
| Branch + worktree cleanup                             | `git branch -d` + `git worktree remove`                                                          | NEVER                                                |
| Record-as-shipped                                     | flip spec Status + update the plan's task marker; `docs:` commit                                 | NEVER                                                |
| Out-of-band verification (#14)                        | 30-second check per touched surface                                                              | NEVER (vacuous is fine)                              |
| Target verification (#15)                             | YOUR merge commit's CI run green, by run ID                                                      | NEVER                                                |
| Tracker transitions                                   | 1 call per transition                                                                            | Only on the ticketless path                          |

A vacuously-satisfied step ("no out-of-band surface touched") is satisfied, not skipped. State it
explicitly.

## Common Rationalizations (All Are Violations)

| Excuse                                                             | Reality                                                                                                                                          |
| ------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| "It's a small change, skip the spec/plan"                          | Only skip the _spec_ if ALL fast-path criteria hold. The plan doc is not skippable.                                                              |
| "I'll ask the user mid-run about an ambiguity"                     | Fully autonomous. Make the most reasonable interpretation, document under Assumptions, continue.                                                 |
| "Slack/the PR/the summary IS the spec"                             | Summaries lose AC. The brief/issue is source of truth. 30 seconds.                                                                               |
| "cargo test --workspace is green, it's shipped"                    | The image, the console bundle, the Worker, and a fresh-cluster migration apply are not test outputs. Verify them at #14.                         |
| "I'll skip the plan doc, the code is self-documenting"             | The plan repository IS this project's history. Commit the doc.                                                                                   |
| "I'll skip the record-as-shipped commit"                           | The spec Status and the plan's task markers are how the docs track shipped state. Close the loop.                                                |
| "I'll answer 403 for an org the caller isn't in"                   | That turns any signed-in account into a directory of who uses the product. It is `404`, always.                                                  |
| "I'll store the token so it can be revoked by value"               | Tokens are stored as SHA-256 hashes only. Revoke by hash, never persist the raw token.                                                           |
| "I'll log the token / the passkey challenge for debugging"         | Secrets never cross a trust boundary into logs, errors, or responses.                                                                            |
| "One little query in `tower-web` is easier than a `tower-core` function" | Every SQL statement lives in `tower-core`; a query elsewhere bypasses the `Tx` pinning RLS depends on.                                              |
| "The new tenant table's cross-org test can come later"             | A tenant-scoped function without a cross-org negative test is not done. `verify_tenant_isolation` won't vouch for an unregistered policy either. |
| "I'll add the tool now and price it in `tower-billing` later"         | `every_tool_has_a_price` fails, and an unclassified tool bills as free. Classify it in the same PR.                                              |
| "I'll tidy up the migration that's already applied"                | Migrations are forward-only. Add a new one; never edit an applied one.                                                                           |
| "An email would be the simplest way to do this"                    | Nothing in this product sends email. Reintroducing a mailer is a product decision, not a convenience.                                            |
| "This only needs to work in one agent"                             | If a feature only works in one client, it does not ship (constraint 3).                                                                          |
| "The latest CI run is green, mine will be too"                     | Latest ≠ yours — a teammate's merge seconds after yours steals it. Capture YOUR run ID at merge time and track THAT id.                          |
| "Copilot's comments are auto-generated, safe to ignore"            | Read each. They find real bugs. Reply with fix-or-dismiss reasoning, then resolve the thread.                                                    |
