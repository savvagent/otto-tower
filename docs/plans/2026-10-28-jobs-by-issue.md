# Jobs by issue: address jobs by their issue, and flag drift

Goal: let every MCP argument that takes a job id also take `#N` / `owner/repo#N`, add a `ticket`
filter to `list_jobs`, show `jobTicketRef` on `list_leases`, have `sync_ticket` turn "Blocked by #N"
lines into dependencies, and mark a job `ticketStale` when its issue has moved on since the job
was written. Closes savvagent/otto-factory#257.

**Spec:** `docs/specs/2026-10-28-jobs-by-issue-design.md`. Read it first: this plan implements
it exactly.

## Status — 2026-10-28

⬜ Not started.

## Global Constraints

- No AI self-attribution anywhere (commits, code comments, docs, PR body).
- Run `cargo fmt --all` before every Rust commit. Each task's gate is `cargo test` (the crates
  named in the task) + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all --check`.
- Every SQL statement lives in `of-core`. `of-mcp`, `of-web` and `of-trackers` get none.
- Every new tenant-scoped function carries `org_id = $1` and gets a cross-org negative test. The
  lease join carries `org_id` in its join predicate.
- Errors are written for an LLM caller. They have stable codes, name the valid forms, quote
  caller input with `{:?}` (capped), and name only job ids the caller may see.
- No `unwrap()` outside tests. Comments explain why, not what.
- Additive only (spec): no tool, field, route or env var renamed or removed. The new migration
  `0009_job_ticket_drift.sql` is schema-only (`ADD COLUMN`, nullable, no data rewrite). No
  applied migration is edited.
- No new MCP tool, so `of-billing::classify` is unchanged. `every_tool_has_a_price` must still
  pass.
- Tests need `podman compose up -d` (Postgres on 15433) and `.env`.
- PR title: `feat(of-mcp): address jobs by their issue, and flag drift from it`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/of-core/src/ticket_ref.rs` | **Create.** `JobRef`, `TicketQuery`, `parse`, `ticket_match_sql`. Pure apart from the SQL fragment. |
| `crates/of-core/src/lib.rs` | **Modify.** `pub mod ticket_ref;`. |
| `crates/of-core/src/error.rs` | **Modify.** `NoJobForTicket(String)` (code `job_not_found`), `AmbiguousTicket { ticket, jobs, more }` (code `ambiguous_ticket_ref`). |
| `crates/of-core/src/jobs.rs` | **Modify.** `resolve_job_ref(s)`, `JobFilter.ticket`, `JOB_COLS` + `Job` drift fields, `note_ticket_changed`, `described_at` writes in `update_job`/`update_from_ticket`, `add_ticket_dependencies` + `BlockerOutcome`. |
| `crates/of-core/src/leases.rs` | **Modify.** `Lease.job_ticket_ref` (`#[sqlx(default)]`), the `list_leases` join, `hide_unseen_jobs` blanking it. |
| `crates/of-core/migrations/0009_job_ticket_drift.sql` | **Create.** Two nullable columns on `jobs`. |
| `crates/of-web/src/error.rs` | **Modify.** Statuses for the two new variants (`404`, `409`). |
| `crates/of-trackers/src/sync.rs` | **Modify.** `blocked_by(body)`, `InboundDecision::Changed`, comment handling in `inbound_decision`. |
| `crates/of-trackers/src/webhook.rs` | **Modify.** `CommentSnapshot.author_is_bot` from `comment.user.type`. |
| `crates/of-trackers/src/github.rs` | **Modify.** `get_issue` → `IssueView { body, updated_at }`. `with_api_base` made `pub` (doc: tests only). |
| `crates/of-web/src/routes/webhooks.rs` | **Modify.** Apply `InboundDecision::Changed`. |
| `crates/of-mcp/src/tools/mod.rs` | **Modify.** `job_ref` / `job_refs` helpers. |
| `crates/of-mcp/src/tools/jobs.rs` | **Modify.** Every job argument resolved through `job_ref`. `list_jobs.ticket`, the `sync_ticket` pull, pending allowed, and descriptions. |
| `crates/of-mcp/src/tools/coord.rs` | **Modify.** `send_message.job`, `acquire_lease.job` through `job_ref`. |
| `crates/of-mcp/src/tools/out.rs` | **Modify.** `SyncTicketOut { job, blockers }`. |
| `crates/of-mcp/src/server.rs`, `crates/of-mcp/src/lib.rs` | **Modify.** `TrackerSyncConfig.github_api_base: Option<String>` (always `None` from `of-mcp::lib`). It is used when building `GithubAppClient`. |
| `crates/of-core/tests/jobs.rs`, `crates/of-core/tests/isolation.rs`, `crates/of-mcp/tests/tools.rs`, `crates/of-web/tests/webhooks_http.rs` | **Modify.** Tests per task. |

## Task Order & Rationale

Task 1 builds the reference grammar and resolver in `of-core`, the substrate for everything else.
Task 2 wires it through the MCP surface and adds the filter and the lease field (spec §1–§3: the
first four ACs plus `list_leases`). Task 3 adds the drift columns and the webhook signal (§5,
inbound). Task 4 extends `sync_ticket` with the pull, which needs both Task 1's resolution and
Task 3's `note_ticket_changed` (§4 and the §5 pull signal). Each task leaves the workspace
green.

## Task 1 — References and resolution in `of-core` ⬜

**Files:** `ticket_ref.rs` (create), `lib.rs`, `error.rs`, `jobs.rs`, `leases.rs`,
`crates/of-web/src/error.rs`, `crates/of-core/tests/jobs.rs`, `crates/of-core/tests/isolation.rs`.
**Interfaces:** produces `ticket_ref::{JobRef, TicketQuery}`, `JobsExt::resolve_job_ref`,
`JobsExt::resolve_job_refs`, `JobFilter.ticket: Option<TicketQuery>`, `Lease.job_ticket_ref`,
`Error::{NoJobForTicket, AmbiguousTicket}`.

- [ ] Write failing unit tests in `ticket_ref.rs` (`#[cfg(test)]`). `parse` accepts `job-42` (→ `Id`),
      `#928`, ` #928 `, `#0928` (number 928), `acme/api#928`, `Acme.X/a_b-c#1` (owner_repo lowercased),
      and refuses `#`, `#abc`, `#-1`, `a#1`, `a/b/c#1`, `/a#1`, `a/#1`, a 19-digit number, a 101-char
      owner, and `#1 2`. Refusal messages name both forms and quote the input (capped at 200 chars).
      `exact()`/`suffix_like()` give `#928`/`Some("%/%#928")` for short and `acme/api#928`/`None` for full.
- [ ] Run `cargo test -p of-core --lib ticket_ref`: it fails (no module yet).
- [ ] Implement `ticket_ref.rs` with a module doc on why `#` separates the two namespaces (spec
      premise 5) and why the `LIKE` pattern is safe (validated digits only). Add `ticket_match_sql`.
- [ ] Add the two `Error` variants. Messages follow spec §1. `AmbiguousTicket` lists the ids and
      says "and more" when `more`. `code()` returns `job_not_found` / `ambiguous_ticket_ref`, and
      neither is retriable. Map them to `NOT_FOUND` / `CONFLICT` in `crates/of-web/src/error.rs`
      (the match there is exhaustive, so the build forces it).
- [ ] Write failing integration tests in `crates/of-core/tests/jobs.rs`:
      `resolve_job_ref` short/full/case-insensitive/leading-zero → id. Zero matches →
      `job_not_found`. Two matches → `ambiguous_ticket_ref` naming both ids. `job-7` passes
      through without a query, even for a job that does not exist (unchanged downstream
      errors). **Cross-org negative:** org B's job with `ticketRef` `acme/api#5` is never resolved
      for org A and is absent from A's ambiguity list. **Team negative:** a job on a team-scoped
      repo the scope excludes is not resolved and does not make a reference ambiguous.
      `list_jobs` with `ticket` returns only that issue's visible jobs, newest first.
- [ ] Implement `resolve_job_ref`/`resolve_job_refs` (one statement, `LIMIT 11`, `job_visible_sql`)
      and `JobFilter.ticket` (append the predicate to `list_jobs`'s `WHERE`, binding after the
      existing parameters).
- [ ] Write failing tests for leases: `list_leases` sets `job_ticket_ref` from the lease's job,
      `None` for a lease without a job, and `hide_unseen_jobs` blanks it with `job_id`. In
      `tests/isolation.rs` add `rls_scopes_the_lease_job_join`, which creates `job-1` in both orgs
      with different `ticket_ref`s and a lease in org A naming `job-1`. Pinned to org A as `otto_app`,
      a deliberately **unscoped** join `repo_leases l LEFT JOIN jobs j ON j.id = l.job_id` (no `org_id`
      predicate) returns only A's `ticket_ref`. Also assert the real `list_leases` gives A's.
- [ ] Implement `Lease.job_ticket_ref` (`#[sqlx(default)]`, serialized `jobTicketRef`, doc
      comment), the join in `list_leases` (`LEASE_COLS` qualified `l.`; join on `j.org_id = l.org_id
      AND j.id = l.job_id`), and the blanking.
- [ ] Run `cargo test -p of-core` and `cargo test -p of-web` (error mapping), then clippy. Both pass.
- [ ] Format and commit: `cargo fmt --all` then
      `git commit -m "feat(of-core): resolve jobs by their issue reference"`.

## Task 2 — The MCP surface ⬜

**Files:** `crates/of-mcp/src/tools/{mod,jobs,coord}.rs`, `crates/of-mcp/tests/tools.rs`.
**Interfaces:** consumes Task 1. Produces `tools::job_ref`/`job_refs` and `ListJobsArgs.ticket`.

- [ ] Write failing tests in `tools.rs`: `get_job("#13")` returns the job linked to `acme/api#13`,
      and so does `get_job("acme/api#13")`. Two jobs with the same issue refuse with
      `ambiguous_ticket_ref`, and the message contains both ids. `set_dependencies(job="#13",
      add=["#14"])` returns `job` = the resolved id and the dependency. `claim_jobs(["#13"])`,
      `renew_claim`, `complete_job("#13")` and `fail_job` round-trip. `update_job("#13")`.
      `add_job(dependsOn=["#13"])`. `acquire_lease(job="#13")` stores the resolved id, and
      `list_leases` shows `jobTicketRef`. `list_jobs(ticket="#13")` gives only those jobs, and
      `list_jobs(ticket="job-1")` is `invalid_argument`. `send_message(job="#13")` resolves. A
      malformed `get_job("#x")` is `invalid_argument`. Set `ticketRef` with `add_job` or `link_ticket`,
      as the existing tests do.
- [ ] Run `cargo test -p of-mcp --test tools`. The new tests fail.
- [ ] Add `job_ref(tx, team, raw)` / `job_refs` in `tools/mod.rs` (thin `.mcp()` wrappers over
      Task 1), and replace every `JobId::from(args.job…)` / `ids(...)` on caller input in `jobs.rs`
      and `coord.rs` with them, resolved inside the transaction **before** the existing
      `ensure_job_visible`/`visible_job`. In `add_job` resolve `depends_on` before building
      `NewJob` in both branches (replay then fingerprints resolved ids, per spec §1). In
      `set_dependencies` resolve `job`, `add` and `remove`. `sync_ticket` resolves its `job` too.
      Delete `fn ids` if it becomes unused.
- [ ] Add `ListJobsArgs.ticket` and wire `JobFilter.ticket` (blank means absent; `JobRef::Id` →
      `Error::Invalid` with spec §2's message), parsed before the transaction opens, like `status`.
- [ ] Update argument docs (`JobArgs.job`, `ClaimJobsArgs.jobs`, `SetDependenciesArgs.*`,
      `AddJobArgs.depends_on`, the `job` fields in `UpdateJobArgs`/`CompleteJobArgs`/`FailJobArgs`/
      `RenewClaimArgs`/`RequestCancelArgs`/`CancelJobArgs`/`LinkTicketArgs`, coord's `job` fields) to
      spec §1's sentence, and `list_jobs`'s description to mention `ticket`.
- [ ] Run `cargo test -p of-mcp` (including `tools.rs`'s tool-list and description assertions) and
      clippy. Both pass.
- [ ] Format and commit: `git commit -m "feat(of-mcp): accept an issue reference wherever a job id is accepted"`.

## Task 3 — The drift marker, and the webhook signal ⬜

**Files:** `0009_job_ticket_drift.sql` (create), `crates/of-core/src/jobs.rs`,
`crates/of-trackers/src/{webhook,sync}.rs`, `crates/of-web/src/routes/webhooks.rs`,
`crates/of-core/tests/jobs.rs`, `crates/of-web/tests/webhooks_http.rs`.
**Interfaces:** produces `Job.ticket_changed_at`, `Job.ticket_stale`, `JobsExt::note_ticket_changed`,
`InboundDecision::Changed { remote_revision }` and `CommentSnapshot.author_is_bot`.

- [ ] Write the migration (spec §5, two nullable `timestamptz` columns, with a comment on each).
- [ ] Write failing tests in `crates/of-core/tests/jobs.rs`. A new job has `ticket_stale = false` and
      `ticket_changed_at = None`. `note_ticket_changed(now + 1 min)` gives stale. A later
      `update_job(description = …)` clears it, and so does `update_job(title = …)`. Changing only
      `metadata` does not clear it. `note_ticket_changed` with a time before `created_at` stays
      clean. Two notes keep the later time (GREATEST). **Cross-org negative:** `note_ticket_changed`
      pinned to org B naming org A's `job-1` id changes nothing in org A (B has its own `job-1` or none).
- [ ] Implement: `JOB_COLS` gains `ticket_changed_at` and the computed `ticket_stale` (spec §5). `Job`
      gains both fields with LLM-facing doc comments. `note_ticket_changed`. `update_job` sets
      `described_at = now()` when title or description is passed. `update_from_ticket` sets it
      always. Check every `RETURNING {JOB_COLS}` / `SELECT {JOB_COLS}` still compiles and runs.
      The computed column is unqualified, like the rest.
- [ ] Write failing unit tests in `of-trackers`. `parse_github` on `issue_comment` reads
      `comment.user.type == "Bot"` into `author_is_bot`, and it is false when `user` is absent. A
      human comment on an existing non-terminal job gives `Changed`. A bot comment gives `NoOp`.
      An echoed comment (`updated_at <= remote_revision`) gives `Drop`. A comment on a terminal job
      gives `NoOp`. An `issues` `edited` event still gives `Update`. A JIRA comment event is
      unchanged (`Update`).
- [ ] Implement `author_is_bot` and the `Changed` branch in `inbound_decision` (GitHub only; spec
      §5 signal 1).
- [ ] Write a failing test in `webhooks_http.rs`: a signed human `issue_comment` webhook for a bound
      repo's existing job flips `ticketStale` (read back through `of-core`), and a bot one does not.
      Reuse that file's existing signed-webhook helpers.
- [ ] Apply `Changed` in `routes/webhooks.rs`: `note_ticket_changed` with the parsed
      `remote_revision` plus `set_remote_revision`, in the webhook's transaction.
- [ ] Run `cargo test -p of-core -p of-trackers -p of-web -p of-mcp` and clippy. They pass.
      (`of-mcp` too, because `Job`'s JSON gained fields, and any snapshot-style assertions there
      must be updated.)
- [ ] Format and commit: `git commit -m "feat(of-core): mark a job stale when its issue changes after it was written"`.

## Task 4 — `sync_ticket` reads "Blocked by" and drift ⬜

**Files:** `crates/of-trackers/src/{sync,github}.rs`, `crates/of-core/src/jobs.rs`,
`crates/of-mcp/src/{server,lib}.rs`, `crates/of-mcp/src/tools/{jobs,out}.rs`,
`crates/of-mcp/tests/tools.rs`, `crates/of-core/tests/jobs.rs`, `crates/of-mcp/Cargo.toml` (dev-deps only if needed).
**Interfaces:** consumes Tasks 1 and 3. Produces `sync::blocked_by`, `GithubAppClient::get_issue`,
`JobsExt::add_ticket_dependencies`, `BlockerOutcome`, `out::SyncTicketOut` and
`TrackerSyncConfig.github_api_base`.

- [ ] Write failing unit tests for `blocked_by`: `"Blocked by #925"` gives `["#925"]`. `"**Blocked by:** #1, #2 and acme/x#3"`
      gives all three. Case-insensitive. A reference before the phrase on the same line is ignored.
      Lines inside ```` ``` ```` and `~~~` fences are ignored. Duplicates collapse. More than 20 are
      capped at 20. `"#abc"` and `"a/b/c#1"` are skipped. Several "Blocked by" lines combine.
- [ ] Implement `blocked_by` (pure, no regex dependency; reuse `ticket_ref` token rules by calling
      `of_core::ticket_ref::JobRef::parse` on candidate tokens).
- [ ] Write failing `of-core` tests for `add_ticket_dependencies(job, &[TicketQuery], scope)`:
      `added` (the dependency row exists), `already`, `no_job` (nothing written), `ambiguous`
      (candidate ids, nothing written), and `cycle` (a candidate that depends on the job, and the
      job's own issue). After a `cycle`, a later `added` in the same call still lands and the
      transaction commits. **Cross-org negative:** an org B job carrying the referenced issue is
      `no_job` for org A.
- [ ] Implement `add_ticket_dependencies` and `BlockerOutcome { ticket_ref, outcome, jobs }`
      (`Serialize`, `JsonSchema`, camelCase, `outcome` a snake_case enum). The cycle check comes
      before the insert, using the same recursive reach as `set_dependencies`, starting from the
      candidate.
- [ ] Add `GithubAppClient::get_issue` (and make `with_api_base` `pub` with a doc saying it is for
      tests pointing at a mock). Add a unit test against the existing `TestServer`, as
      `get_issue_updated_at`'s test does.
- [ ] Add `TrackerSyncConfig.github_api_base` (set to `None` in `of-mcp/src/lib.rs`, with a comment
      that no env var sets it) and apply it wherever `GithubAppClient::new` is called in `jobs.rs`.
- [ ] Write failing tests in `tools.rs` against a mock GitHub (an axum server on a loopback port
      answering `POST /app/installations/{id}/access_tokens`, `GET /repos/acme/api/issues/{n}`,
      `POST …/comments`, built with `Factory::new_with_tracker_sync` and the RSA test key that
      `of-trackers`' tests use). A **pending** job linked to `acme/api#13` whose issue body says
      "Blocked by #14", with another job linked to `acme/api#14`, gets `sync_ticket` →
      `blockers[0].outcome == "added"`. The dependency shows through `set_dependencies`/`blocked`,
      and no comment was posted. "Blocked by #99" with no job gives `no_job`, and the job's
      dependencies are unchanged. An issue `updated_at` later than the job's creation, with no
      `remote_revision`, gives `ticketStale: true` in the result and in `ready`. A non-pending job
      still posts its comment (existing behaviour) and returns `blockers`. Update
      `sync_ticket_refuses_a_still_pending_job`: a pending job with no binding still gets the
      not-configured error. Rename the test to say what it now asserts.
- [ ] Implement in `sync_ticket` per spec §4 steps 1–6: fetch with `get_issue` (failure →
      `tracker_sync_failed`, retriable), then one transaction with `note_ticket_changed` when
      `updated_at > remote_revision` (or none) and `add_ticket_dependencies` (short refs qualified
      with the job's `owner/repo`), then return for pending, else the existing write-back. Return
      `SyncTicketOut`. Update the tool description (spec §4 Output).
- [ ] Run `cargo test --workspace` and clippy. Both pass.
- [ ] Format and commit: `git commit -m "feat(of-mcp): turn Blocked by lines into dependencies in sync_ticket"`.

## Out-of-band checks (Phase 5)

- Migration: `0009_job_ticket_drift.sql` is new and schema-only. A fresh cluster applies cleanly
  (`podman compose down -v && podman compose up -d && cargo test -p of-core`).
- No `Dockerfile`, `fly.toml`, `web/`, worker, CI or `OF_*` change (`github_api_base` is not an
  env var).
- `docs/clients/matrix.md`: no client-visible break, so no entry is needed.
