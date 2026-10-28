# Console `remote_taken` redaction — one hidden-repo redaction rule for both surfaces

Goal: move `redact_foreign_ids` from `of-mcp` into `of-core` as `TeamScope::redact_foreign_ids`,
and apply it on the console's repo write paths (`POST /api/orgs/{org}/repos`,
`PATCH /api/orgs/{org}/repos/{repo}`), so a team-limited caller is never told which hidden repo
owns a remote, on either surface. Closes savvagent/otto-factory#251.

**Spec:** `docs/specs/2026-10-28-console-remote-taken-redaction-design.md` — read it first. This
plan implements it exactly. Note its premise correction: both console write routes are admin-only
today, so this is defence in depth, not a live leak.

## Status — 2026-10-28

⬜ Not started.

## Global Constraints

- No AI self-attribution anywhere (commits, code comments, docs, PR body).
- Run `cargo fmt --all` before every Rust commit.
- Every SQL statement lives in `of-core`. This plan adds none; the moved function is pure.
- No `unwrap()` outside tests.
- Tests need `podman compose up -d` (Postgres on 15433) and `.env` (`cp .env.example .env`).
- No public interface change: codes, statuses and messages are identical for every caller that can
  reach them today. PR title `fix(of-web): …`. Nothing out of band (no image, console bundle,
  Worker, migration or config change).

## File Structure

| File | Responsibility |
|---|---|
| `crates/of-core/src/teams.rs` | **Modify.** Add `TeamScope::redact_foreign_ids` (moved verbatim) and its unit tests. |
| `crates/of-mcp/src/tools/mod.rs` | **Modify.** Delete `redact_foreign_ids`. |
| `crates/of-mcp/src/tools/repos.rs`, `crates/of-mcp/src/tools/jobs.rs` | **Modify.** Call `team.redact_foreign_ids(e)`. |
| `crates/of-web/src/routes/repos.rs` | **Modify.** `register_repo` / `update_repo` build the caller's `TeamScope` and map errors through it. |
| `crates/of-web/src/error.rs` | **Modify.** Unit test: a redacted `remote_taken` is `409 remote_taken` with the redacted message. |
| `crates/of-web/tests/team_scoping.rs` | **Modify.** Integration test beside `a_hidden_repo_is_not_found_and_not_named_anywhere`. |

## Task Order & Rationale

One task: the move and the new call sites must land together (the of-mcp copy is deleted in the
same commit its replacement appears), and the tests are of the same behaviour.

## Task 1 — Shared redaction, applied on the console ⬜

**Files:** as above. **Interfaces:** produces `of_core::teams::TeamScope::redact_foreign_ids(&self,
e: of_core::Error) -> of_core::Error`; consumes `OrgCtx::team_scope`.

- [ ] Write the `of-core` unit tests in `teams.rs` (`All` passes the three errors through; `Teams`
      redacts each to `Redacted` with the original code and no slug/job id; an unrelated error
      passes through). Run `cargo test -p of-core --lib teams` — expect a compile failure (method
      missing).
- [ ] Move the function into `teams.rs` as a `TeamScope` method, delete the `of-mcp` copy, update
      the five `of-mcp` call sites. Run `cargo test -p of-core --lib teams` — pass.
- [ ] Write the `of-web` unit test in `error.rs` and the integration test
      `a_taken_remote_never_names_a_hidden_repo` in `tests/team_scoping.rs` (owner adds a remote to
      `platform-repo`; member `bob` registering it, or adding it to `shared`, is refused without
      `platform-repo` in the body; owner gets `409 remote_taken` naming `platform-repo`).
- [ ] Apply the redaction in `routes/repos.rs` (`ctx.team_scope` before `ctx.begin`, `map_err`
      on the `of-core` write). Run `cargo test -p of-web --test team_scoping` and
      `cargo test -p of-web --lib error` — pass.
- [ ] Run `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`.
- [ ] Format and commit: `cargo fmt --all` then
      `git commit -m "of-web: redact remote_taken for team-limited callers on repo writes"`.
