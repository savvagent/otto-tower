# Tracker webhook dedupe hardening — close the backstops the #268 reviews named

Goal: make `tracker_deliveries`' insert side pinned-only through separate `SELECT` / `INSERT` /
`DELETE` policies (new migration 0011), add a positive per-event action check to `parse_github`,
return the original `received_at` from `record` on a repeat and log the repeat at `warn`, and
record the decision not to dedupe JIRA webhooks. Closes savvagent/otto-factory#273.

**Spec:** `docs/specs/2026-10-28-harden-webhook-dedupe-design.md` — read it first. This plan
implements it exactly.

## Status — 2026-10-28

✅ Shipped in PR #276 (squash 641cc4d). Review changed one decision in Task 3: an action neither
list documents parses as before instead of being ignored, because an unknown comment action reaches
`github_comment_decision`, which can mark a job stale (spec §2). The JIRA follow-up is #275; the
repeat log's flood concern is noted on #271.

## Global Constraints

- No AI self-attribution anywhere (commits, code comments, docs, PR body).
- Run `cargo fmt --all` before every Rust commit.
- Every SQL statement lives in `of-core` (`of_core::tracker_deliveries`). `of-web` only logs what
  `record` returns; `of-trackers` adds no SQL.
- Migrations are append-only: a **new** `0011_tracker_deliveries_pinned_insert.sql` (renumber if
  another PR takes 0011 first). `0010` is never edited. Schema-only, so no per-org data loop.
- The `SELECT` policy keeps the name `tracker_deliveries_tenant_isolation` (the startup check finds
  tenant tables by it).
- RLS tests `SET LOCAL ROLE otto_app` (or connect a pool as it): `#[sqlx::test]` is a superuser.
- No `unwrap()` outside tests. No body, digest, or secret in a log line.
- Tests need `podman compose up -d` (Postgres on 15433) and `.env` (`cp .env.example .env`).
- Additive: no tool, field, route, env var or result-shape change. `record`'s return type is an
  internal Rust API change (`of-core` → `of-web`). PR title
  `fix(of-web): harden tracker webhook dedupe`. Out of band: the migration only.

## File Structure

| File | Responsibility |
|---|---|
| `crates/of-core/migrations/0011_tracker_deliveries_pinned_insert.sql` | **Create.** Replace the `FOR ALL` policy with `SELECT` (two-branch, keeps the name), `INSERT` (pinned-only), `DELETE` (two-branch). |
| `crates/of-core/tests/tracker_deliveries.rs` | **Modify.** `rls_scopes_an_unpinned_tracker_delivery_insert_to_nothing`; policy-shape catalog assertion; `Recorded` assertions incl. original `received_at`. |
| `crates/of-core/src/tracker_deliveries.rs` | **Modify.** `Recorded` enum; `record` reads the original `received_at` on a conflict. |
| `crates/of-core/tests/platform_events.rs` | **Modify.** Adapt to `record`'s new return type if it asserts on it. |
| `crates/of-web/src/routes/webhooks.rs` | **Modify.** `first_delivery` matches `Recorded`, logs a repeat at `warn` with `original_received_at`. |
| `crates/of-trackers/src/webhook.rs` | **Modify.** `ISSUES_ACTIONS` / `ISSUE_COMMENT_ACTIONS`; reject an other-event action, ignore an unknown one; doc comment on the overlap; unit tests. |
| `crates/of-web/tests/webhooks_http.rs` | **Modify (only if needed).** Existing repeat test still asserts `200` + no effect. |
| `CLAUDE.md` | **Modify.** `tracker_deliveries`' insert is pinned-only; only its `SELECT`/`DELETE` have the two branches. |

## Task Order & Rationale

Task 1 (migration) is independent and the highest-risk change, so it lands first with its RLS
proof. Task 2 changes `record`'s signature and its one caller together, so the workspace builds at
every commit. Task 3 is a self-contained `of-trackers` change. Task 4 is docs. Each task's gate is
`cargo test` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all`.

## Task 1 — Pinned-only insert policy ✅

**Files:** `crates/of-core/migrations/0011_tracker_deliveries_pinned_insert.sql`,
`crates/of-core/tests/tracker_deliveries.rs`.
**Interfaces:** consumes `current_org()` (baseline) and the `tracker_deliveries` table (`0010`);
produces policies `tracker_deliveries_tenant_isolation` (`FOR SELECT`), `tracker_deliveries_insert`
(`FOR INSERT`), `tracker_deliveries_sweep` (`FOR DELETE`).

- [ ] Write the failing test `rls_scopes_an_unpinned_tracker_delivery_insert_to_nothing` in
  `crates/of-core/tests/tracker_deliveries.rs`: record one delivery each for orgs A and B (pinned,
  via `record_one`). Open `db.begin_unpinned()`, `SET LOCAL ROLE otto_app`, set no `app.org_id`,
  attempt `INSERT INTO tracker_deliveries (org_id, provider, digest) VALUES ($1, 'github',
  decode(repeat('cd', 32), 'hex'))` for A; assert it is an error; roll back. In a second unpinned
  `otto_app` transaction, `DELETE FROM tracker_deliveries WHERE received_at <= now()`; assert
  `rows_affected() == 2`; roll back. Keep the predicate: a `DELETE` with no `WHERE` is not bound by
  the `SELECT` policy, so it would not pin that policy's `IS NULL` branch. Also assert, from `pg_policy` joined to `pg_class` for
  `tracker_deliveries`, that `(polname, polcmd)` is exactly
  `{(tracker_deliveries_insert, a), (tracker_deliveries_sweep, d),
  (tracker_deliveries_tenant_isolation, r)}`. Read `polcmd` as `polcmd::text`.
- [ ] Run `cargo test -p of-core --test tracker_deliveries` — expect the new test to FAIL (the
  unpinned insert succeeds under the `0010` policy; the catalog has one `*` policy).
- [ ] Create `crates/of-core/migrations/0011_tracker_deliveries_pinned_insert.sql` with a header
  comment (why: only the sweep needs the `IS NULL` branch and it never inserts; why the `SELECT`
  policy keeps the `_tenant_isolation` name; why `DELETE` needs the `SELECT` policy; no `UPDATE`
  policy; schema-only) and the three statements of spec §1 after `DROP POLICY
  tracker_deliveries_tenant_isolation ON tracker_deliveries;`.
- [ ] Run `cargo test -p of-core --test tracker_deliveries` — expect all PASS, including
  `rls_scopes_tracker_deliveries_to_the_pinned_org` (pinned cross-org insert still refused) and
  `the_sweep_reaches_every_org_where_rls_applies`.
- [ ] Run `cargo test -p of-core --test isolation --test platform_events` — expect PASS
  (`verify_tenant_isolation` still lists `tracker_deliveries`; the `org.deleted` purge still
  deletes it).
- [ ] Out-of-band reminder: this is a migration. Phase 5 re-applies it to a fresh cluster.
- [ ] Format and commit: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
  `git commit -m "of-core: make tracker_deliveries inserts pinned-only"`.

## Task 2 — `Recorded` with the original `received_at`, logged at `warn` ✅

**Files:** `crates/of-core/src/tracker_deliveries.rs`, `crates/of-core/tests/tracker_deliveries.rs`,
`crates/of-core/tests/platform_events.rs` (if needed), `crates/of-web/src/routes/webhooks.rs`.
**Interfaces:** produces `pub enum Recorded { First, Repeat { received_at:
Option<chrono::DateTime<chrono::Utc>> } }` and `record(...) -> Result<Recorded>`; consumed by
`of-web`'s `first_delivery`.

- [ ] Change `record_one` in `crates/of-core/tests/tracker_deliveries.rs` to return `Recorded` and
  update existing assertions to `matches!(…, Recorded::First)` / `Recorded::Repeat { .. }`. Add
  `a_repeat_reports_when_the_original_was_received`: record a body for an org, read its
  `received_at` (superuser `SELECT` on the pool), record it again, assert `Recorded::Repeat {
  received_at: Some(t) }` with `t` equal to the stored value.
- [ ] Run `cargo test -p of-core --test tracker_deliveries` — expect a compile FAIL (no
  `Recorded`).
- [ ] Implement in `crates/of-core/src/tracker_deliveries.rs`: `#[derive(Debug, Clone, Copy,
  PartialEq, Eq)] pub enum Recorded`; after the `INSERT … ON CONFLICT DO NOTHING`, when no row was
  inserted, run a second statement `SELECT received_at FROM tracker_deliveries WHERE org_id = $1
  AND provider = $2 AND digest = $3` with `fetch_optional` on `tx.conn()`. Doc comment: why a
  second statement and not a CTE (a new statement's snapshot sees a concurrent first delivery's
  committed row), why `None` is possible (swept in between). Update the module doc.
- [ ] Update `crates/of-web/src/routes/webhooks.rs` `first_delivery`: match `Recorded`; on
  `Repeat { received_at }` log `tracing::warn!(provider, org_id, delivery_id,
  original_received_at = received_at.map(|t| t.to_rfc3339()).as_deref().unwrap_or("-"), "repeated
  webhook delivery; acknowledged without effect")` and return `false`. Update the doc comment.
- [ ] Fix any other caller (`crates/of-core/tests/platform_events.rs` calls `record` and only
  `unwrap`s; confirm it compiles).
- [ ] Run `cargo test -p of-core --test tracker_deliveries --test platform_events` and
  `cargo test -p of-web --test webhooks_http` — expect PASS.
- [ ] Format and commit: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
  `git commit -m "of-web: log a repeated tracker delivery at warn with its original time"`.

## Task 3 — Positive per-event action check in `parse_github` ✅

**Files:** `crates/of-trackers/src/webhook.rs`.
**Interfaces:** `parse_github` (private); `verify_and_parse`'s observable result changes only for
bodies whose action does not fit their event.

- [ ] Write failing unit tests in `webhook.rs`'s test module, using the existing fixtures and
  `serde_json::Value` edits:
  - `a_comment_shaped_action_under_issues_is_rejected_without_a_comment`: the `issues` fixture with
    `action` set to `created`, sent as `issues` → `Err(InvalidWebhook)` (it carries no `comment`, so
    only the action check can reject it).
  - `an_issue_shaped_action_under_issue_comment_is_rejected`: the comment fixture with `action` set
    to `opened`, sent as `issue_comment` → `Err(InvalidWebhook)`.
  - `an_issues_body_cannot_be_sent_as_issue_comment`: the `issues` fixture plus a `comment` object,
    action `opened`, sent as `issue_comment` → `Err` (keeps the cross-check of spec §2's table).
  - `an_action_neither_event_documents_is_ignored`: `issues` fixture with action `frobnicated` →
    `Ok(ParsedWebhook::Ignored(_))`; the same for the comment fixture as `issue_comment`.
  - `every_documented_action_parses_under_its_own_event`: loop over each set, edit the fixture's
    action, assert `ParsedWebhook::Event(_)`.
  Use the existing `verify_and_parse` / signature helpers the module's tests already use (sign the
  edited body).
- [ ] Run `cargo test -p of-trackers webhook` — expect the reject and ignore tests to FAIL.
- [ ] Implement: `const ISSUES_ACTIONS: &[&str]` and `const ISSUE_COMMENT_ACTIONS: &[&str]` (spec
  §2 lists, with a comment naming GitHub's webhook reference and the 2026-10 snapshot). A small
  helper `fn action_fit(action, own, other) -> ActionFit { Own, OtherOnly, Unknown }`. In each
  branch after deserialization (and, for `issues`, after the `comment` check): `OtherOnly` →
  `Error::InvalidWebhook("GitHub {event} payload carried the action {action:?}, which only
  {other_event} sends; its X-GitHub-Event header does not match its body")` — the action is a
  short GitHub-controlled string from a signed body; bound it with `{:.32}` anyway; `Unknown` →
  `ParsedWebhook::Ignored(IgnoredWebhook { provider, connection_external_id, event })`. Update the
  doc comment on `parse_github` and the `issues` branch comment: two checks per direction, and the
  `edited`/`deleted` overlap where the structural check is the only one.
- [ ] Run `cargo test -p of-trackers` and `cargo test -p of-web --test webhooks_http` — expect
  PASS (fixture actions are documented ones).
- [ ] Format and commit: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
  `git commit -m "of-trackers: check a GitHub webhook's action against its event"`.

## Task 4 — Docs ✅

**Files:** `CLAUDE.md`, the spec (JIRA decision is already §4).

- [ ] File the JIRA follow-up (spec §4): `gh issue create --repo savvagent/otto-factory --label
  enhancement --title "Move JIRA tracker webhooks to a body-signed transport" --body-file <file>`
  (body: why the shared secret cannot support replay protection, what a signed transport enables,
  link to spec §4 and #273). Write its number into spec §4.
- [ ] In `CLAUDE.md`'s tenant-isolation section, change the sentence naming `tracker_deliveries`
  among the two-branch policies: its `SELECT` and `DELETE` policies have the two branches; its
  `INSERT` is pinned-only (`0011_tracker_deliveries_pinned_insert.sql`), since no org-less code
  inserts.
- [ ] Commit: `git commit -m "docs: tracker_deliveries inserts are pinned-only"`.
