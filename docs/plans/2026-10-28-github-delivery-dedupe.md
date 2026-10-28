# GitHub delivery dedupe — a signed GitHub webhook takes effect once per org

Goal: record every verified GitHub tracker-webhook event in a new tenant table,
`tracker_deliveries`, keyed per org by the SHA-256 of its signed body (the `X-GitHub-Delivery` id is
stored for diagnosis only), in the same transaction as its effects; reject a body whose shape does
not match its `X-GitHub-Event`; acknowledge a repeat with `200` before `apply` (and so
`inbound_decision`) runs; sweep rows after 30 days. Closes savvagent/otto-factory#265.

**Spec:** `docs/specs/2026-10-28-github-delivery-dedupe-design.md` — read it first. This plan
implements it exactly.

## Status — 2026-10-28

⬜ Not started.

## Global Constraints

- No AI self-attribution anywhere (commits, code comments, docs, PR body).
- Run `cargo fmt --all` before every Rust commit.
- Every SQL statement lives in `of-core` (`of_core::tracker_deliveries`). `of-web` only computes the
  digest and calls it; `of-trackers` only reads the header.
- New tenant table: `org_id NOT NULL`, `ENABLE`/`FORCE ROW LEVEL SECURITY`, policy
  `tracker_deliveries_tenant_isolation`, a cross-org negative test that runs as `otto_app`.
- Migrations are append-only: a new `0010_tracker_deliveries.sql` (renumber if another PR takes
  0010 first). Schema-only, so no per-org data loop.
- No `unwrap()` outside tests. A database error while recording is a `5xx`, never "not a repeat".
- Tests need `podman compose up -d` (Postgres on 15433) and `.env` (`cp .env.example .env`).
- Additive: no tool, field, route, env var or result-shape change. PR title
  `fix(of-web): deduplicate GitHub webhook deliveries`. Out of band: the migration only.

## File Structure

| File | Responsibility |
|---|---|
| `crates/of-core/migrations/0010_tracker_deliveries.sql` | **Create.** Table, indexes, two-branch RLS policy, `otto_app` grant. |
| `crates/of-core/src/tracker_deliveries.rs` | **Create.** `record` (pinned insert-if-new) and `sweep` (org-less retention delete). |
| `crates/of-core/src/lib.rs` | **Modify.** `pub mod tracker_deliveries;` |
| `crates/of-core/src/platform_events.rs` | **Modify.** `org_deleted` purges `tracker_deliveries`. |
| `crates/of-core/tests/tracker_deliveries.rs` | **Create.** Repeat detection by both keys, per-org keys, RLS cross-org test as `otto_app`, sweep as `otto_app`, org purge. |
| `crates/of-trackers/src/webhook.rs` | **Modify.** `github_delivery_id(headers)`; unit tests. |
| `crates/of-web/src/routes/webhooks.rs` | **Modify.** Record before `apply` in each pass; `200` on a repeat. |
| `crates/of-web/tests/webhooks_http.rs` | **Modify.** Delivery-id repeat, body replay, malformed id. |
| `crates/of-server/src/main.rs` | **Modify.** `sweep_loop` also sweeps `tracker_deliveries` (30 days). |

## Task Order & Rationale

Task 1 builds the storage and its isolation proof, which Task 2 consumes; Task 2 wires the header,
the route and the sweeper. Each task's gate is `cargo test` + `cargo clippy --all-targets -- -D
warnings` + `cargo fmt --all`.

## Task 1 — `tracker_deliveries` table and `of_core::tracker_deliveries` ⬜

**Files:** the migration, `tracker_deliveries.rs`, `lib.rs`, `platform_events.rs`,
`tests/tracker_deliveries.rs`.
**Interfaces:** produces `record(tx, Provider, Option<&str>, &[u8; 32]) -> Result<bool>` and
`sweep(&Db, i32) -> Result<u64>`.

- [ ] Write `crates/of-core/tests/tracker_deliveries.rs`: `record` true then false for one body;
  false for a new body with a seen delivery id; true for the same body and id in a second org;
  `rls_scopes_tracker_deliveries_to_the_pinned_org` (unpinned tx, `SET LOCAL ROLE otto_app`,
  `app.org_id` = B; unscoped `SELECT count(*)` sees 1, `DELETE ... WHERE org_id = A` deletes 0, an
  `INSERT` for A is refused); `the_sweep_reaches_every_org_where_rls_applies` (pool `SET ROLE
  otto_app`, rows aged 40 days in two orgs plus one fresh row; `sweep(30)` = 2, fresh row kept);
  `org_deleted` purges the org's rows.
- [ ] Run `cargo test -p of-core --test tracker_deliveries`. Expected: fails to compile (no module).
- [ ] Create the migration (spec §1) and `tracker_deliveries.rs` (spec §2); add the module to
  `lib.rs`; add `"tracker_deliveries"` to `org_deleted`'s table list.
- [ ] Run `cargo test -p of-core --test tracker_deliveries` and `cargo test -p of-core --test
  isolation --test platform_events --test guards`. Expected: pass (the startup isolation check
  `isolation_verifies_on_a_healthy_database` vouches for the new policy).
- [ ] Format and commit: `cargo fmt --all` then
  `git commit -m "of-core: record tracker webhook deliveries per org"`.

## Task 2 — Drop repeated GitHub deliveries in the webhook route ⬜

**Files:** `webhook.rs`, `webhooks.rs`, `webhooks_http.rs`, `main.rs`.
**Interfaces:** consumes Task 1; produces `of_trackers::webhook::github_delivery_id`.

- [ ] In `crates/of-web/tests/webhooks_http.rs` add (spec §6): `a_repeated_delivery_id_has_no_effect`,
  `a_replayed_body_under_a_new_delivery_id_has_no_effect`, `a_malformed_delivery_id_is_rejected`.
  A helper posts a comment with an optional `x-github-delivery`; a helper clears the job's
  `ticket_changed_at` with direct SQL.
- [ ] Run `cargo test -p of-web --test webhooks_http delivery`. Expected: the two repeat tests fail
  (the repeat marks the job), the malformed one fails (`200`).
- [ ] Add `github_delivery_id` with unit tests (absent, GUID, 129 bytes, non-ASCII, empty) in
  `crates/of-trackers/src/webhook.rs`.
- [ ] In `receive`, for a verified GitHub `ParsedWebhook::Event`: read the delivery id (error →
  uniform `404`), digest the body, `record` at the start of pass 1 and of pass 2; `false` → info log
  and `200 {"ok":true}` without commit (spec §4).
- [ ] In `crates/of-server/src/main.rs`, `sweep_loop` calls
  `of_core::tracker_deliveries::sweep(&db, TRACKER_DELIVERY_RETENTION_DAYS)` (30), logging as the
  platform-event sweep does, independently of it.
- [ ] Run `cargo test -p of-trackers`, `cargo test -p of-web --test webhooks_http`,
  `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`. Expected: pass.
- [ ] Out of band: migration only. `podman compose down -v && podman compose up -d` then
  `cargo test -p of-core` applies it to a fresh cluster.
- [ ] Format and commit: `cargo fmt --all` then
  `git commit -m "of-web: drop repeated GitHub webhook deliveries"`.
