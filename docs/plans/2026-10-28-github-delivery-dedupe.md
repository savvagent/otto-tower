# GitHub delivery dedupe — a signed GitHub webhook takes effect once per org

Goal: record every verified GitHub tracker-webhook event in a new tenant table,
`tracker_deliveries`, keyed per org by the SHA-256 of its signed body (the `X-GitHub-Delivery` id is
stored for diagnosis only), in the same transaction as its effects; reject a body whose shape does
not match its `X-GitHub-Event`; acknowledge a repeat with `200` before `apply` (and so
`inbound_decision`) runs; sweep rows after 30 days. Closes savvagent/otto-factory#265.

**Spec:** `docs/specs/2026-10-28-github-delivery-dedupe-design.md` — read it first. This plan
implements it exactly.

## Status — 2026-10-28

✅ Shipped in PR #268 (squash bb7c0b3). Review moved the key to the SHA-256 of the signed body
alone (the delivery id is a non-unique diagnostic column) and added the `issues`-payload shape
check. Deferred findings: #271 (replay past the retention window, installation moves) and #273
(pinned-only `WITH CHECK`, a positive event-shape check, JIRA replay, the repeat-branch log level).

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
| `crates/of-core/tests/tracker_deliveries.rs` | **Create.** Repeat detection by body digest, per-org keys, RLS cross-org test as `otto_app`, sweep as `otto_app`, org purge. |
| `crates/of-trackers/src/webhook.rs` | **Modify.** `github_delivery(headers, body)` (digest + diagnostic id); `issues` payload shape check; unit tests. |
| `crates/of-web/src/routes/webhooks.rs` | **Modify.** Record before `apply` in each pass; `200` on a repeat. |
| `crates/of-web/tests/webhooks_http.rs` | **Modify.** Repeated delivery, body replay, wrong event, second-pass record, malformed id. |
| `crates/of-server/src/main.rs` | **Modify.** `sweep_loop` also sweeps `tracker_deliveries` (30 days). |

## Task Order & Rationale

Task 1 builds the storage and its isolation proof, which Task 2 consumes; Task 2 wires the header,
the route and the sweeper. Each task's gate is `cargo test` + `cargo clippy --all-targets -- -D
warnings` + `cargo fmt --all`.

## Task 1 — `tracker_deliveries` table and `of_core::tracker_deliveries` ✅

**Files:** the migration, `tracker_deliveries.rs`, `lib.rs`, `platform_events.rs`,
`tests/tracker_deliveries.rs`.
**Interfaces:** produces `record(tx, Provider, Option<&str>, &[u8; 32]) -> Result<bool>` and
`sweep(&Db, i32) -> Result<u64>`.

- [x] Write `crates/of-core/tests/tracker_deliveries.rs`:
  `the_same_body_is_a_repeat_whatever_delivery_id_it_carries`,
  `a_seen_delivery_id_never_makes_a_different_body_a_repeat`,
  `a_delivery_rolled_back_with_its_transaction_is_not_a_repeat`, `deliveries_do_not_cross_orgs`,
  `rls_scopes_tracker_deliveries_to_the_pinned_org` (unpinned tx, `SET LOCAL ROLE otto_app`,
  `app.org_id` = B; unscoped `SELECT count(*)` sees 1, `DELETE ... WHERE org_id = A` deletes 0, an
  `INSERT` for A is refused) and `the_sweep_reaches_every_org_where_rls_applies` (pool `SET ROLE
  otto_app`, rows aged 40 days in two orgs plus one fresh row; `sweep(30)` = 2, fresh row kept).
  The `org_deleted` purge is covered by `org_deleted_purges_that_org_and_only_that_org` in
  `tests/platform_events.rs`.
- [x] Run `cargo test -p of-core --test tracker_deliveries`. Expected: fails to compile (no module).
- [x] Create the migration (spec §1) and `tracker_deliveries.rs` (spec §2); add the module to
  `lib.rs`; add `"tracker_deliveries"` to `org_deleted`'s table list.
- [x] Run `cargo test -p of-core --test tracker_deliveries` and `cargo test -p of-core --test
  isolation --test platform_events --test guards`. Expected: pass (the startup isolation check
  `isolation_verifies_on_a_healthy_database` vouches for the new policy).
- [x] Format and commit: `cargo fmt --all` then
  `git commit -m "of-core: record tracker webhook deliveries per org"`.

## Task 2 — Drop repeated GitHub deliveries in the webhook route ✅

**Files:** `webhook.rs`, `webhooks.rs`, `webhooks_http.rs`, `main.rs`.
**Interfaces:** consumes Task 1; produces `of_trackers::webhook::github_delivery`.

- [x] In `crates/of-web/tests/webhooks_http.rs` add (spec §6):
  `a_repeated_delivery_is_acknowledged_without_effect_but_a_reused_id_is_not_a_key`,
  `a_replayed_body_under_a_new_delivery_id_is_acknowledged_without_effect`,
  `a_body_sent_under_the_wrong_event_is_rejected_and_does_not_pre_empt_the_genuine_delivery`,
  `a_billed_close_records_its_delivery_in_the_second_pass` and `a_malformed_delivery_id_is_rejected`.
  Helpers post a comment with an optional `x-github-delivery` and forget recorded deliveries, so
  the older redelivery tests still exercise the revision and `ticket_changed_at` guards.
- [x] Run `cargo test -p of-web --test webhooks_http delivery`. Expected: the two repeat tests fail
  (the repeat marks the job), the malformed one fails (`200`).
- [x] Add `github_delivery` in `crates/of-trackers/src/webhook.rs` with unit tests
  `a_missing_delivery_id_is_none`, `a_guid_delivery_id_is_read`, `a_malformed_delivery_id_is_rejected`
  (empty, 129 bytes, non-ASCII), `the_delivery_digest_is_the_signed_body_alone` and
  `a_github_body_under_the_wrong_event_is_rejected`.
- [x] In `receive`, for a verified GitHub `ParsedWebhook::Event`: read the delivery id (error →
  uniform `404`), digest the body, `record` at the start of pass 1 and of pass 2; `false` → info log
  and `200 {"ok":true}` without commit (spec §4).
- [x] In `crates/of-server/src/main.rs`, `sweep_loop` calls
  `of_core::tracker_deliveries::sweep(&db, TRACKER_DELIVERY_RETENTION_DAYS)` (30), logging as the
  platform-event sweep does, independently of it.
- [x] Run `cargo test -p of-trackers`, `cargo test -p of-web --test webhooks_http`,
  `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`. Expected: pass.
- [ ] Out of band: migration only. `podman compose down -v && podman compose up -d` then
  `cargo test -p of-core` applies it to a fresh cluster. **Not run:** the local cluster was shared
  with other in-flight work. Every `#[sqlx::test]` applies `0010` to a fresh database, in CI and
  locally.
- [x] Format and commit: `cargo fmt --all` then
  `git commit -m "of-web: drop repeated GitHub webhook deliveries"`.
