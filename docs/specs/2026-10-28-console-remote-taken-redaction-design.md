# Console `remote_taken` redaction design

> **Status:** IMPLEMENTED (PR #269, 2026-10-28; merged as bd2d222). The console's repo writes
> withhold the owning repo's slug from a team-limited caller, through the same redaction the MCP
> tools use, moved into `of-core`. Review added `update_repo` resolving its target through the
> caller's scope, held for update (`resolve_repo_visible_held(.., Hold::ForUpdate)`), and pinned the
> member arm of `a_taken_remote_never_names_a_hidden_repo` to exactly `403`. No follow-up issues.

> **Implements:** savvagent/otto-factory#251 (follow-up from PR #249, security review note I1).

## Premise corrections

The issue says a member not on team X who registers, through `POST /api/orgs/{org}/repos`, a
remote that team X's repo already owns learns that repo's slug. As the code stands today, that
caller never reaches the error:

- `routes::repos::register_repo` and `routes::repos::update_repo` (`crates/of-web/src/routes/repos.rs`)
  both call `ctx.require_admin()` first, so a member is refused `403` before any SQL runs.
- `TeamScope::for_member` (`crates/of-core/src/teams.rs`) answers `TeamScope::All` for every role
  that `can_administer()`, and `require_admin` tests the same `OrgCtx::role`. Every caller that
  reaches `Error::RemoteTaken` on the console is therefore unrestricted, and the MCP rule would
  hand them the error unchanged.

So there is no live leak. What is missing is the rule itself: the console's safety rests on an
admin gate several lines away, chosen for a different reason (who may register repos), while the
MCP surface admits any `repos:write` caller and redacts. If the console's gate is ever brought in
line with MCP's, the slug leaks with no test to say so. This change puts the redaction on the
console's write paths as well, so the invariant "a hidden repo is never named" holds structurally
on both surfaces rather than by coincidence on one.

## Scope

**In:**

- Move `redact_foreign_ids` from `of-mcp` (`crates/of-mcp/src/tools/mod.rs`) into `of-core` as
  `TeamScope::redact_foreign_ids(&self, e: Error) -> Error`, unchanged in behaviour and wording.
  `of-web` cannot depend on `of-mcp` (they are siblings over `of-core`), so a shared rule has to
  live below both.
- `of-mcp`'s six call sites (four in `tools/jobs.rs`, two in `tools/repos.rs`) call the `of-core` method; the `of-mcp` copy is deleted.
- `of-web`'s `register_repo` and `update_repo` build the caller's `TeamScope` (free for admins: no
  platform call) and pass any `of-core` error through it.
- `of-web`'s `update_repo` resolves its target through the same scope, held for update
  (`resolve_repo_visible_held(.., Hold::ForUpdate)`, what MCP's `update_repo` does through
  `repo_for_write`), instead of the unscoped `resolve_repo`. Added in review: without it the
  handler's own target lookup would still name, and write to, a hidden repo once the admin gate
  is relaxed, so the redaction alone would only half-apply the rule.
- Tests (§3).

**Out:**

- Relaxing the console's `require_admin` on repo writes. That is a product decision, not this fix.
- Any change to which errors are redacted, or their messages, or their codes.
- Any new route, tool, field, env var, or migration. No SQL changes.

The three constraints are untouched: no repo anchoring change, no workflow opinion, nothing
client-specific.

## §1 The shared rule

`TeamScope::redact_foreign_ids` keeps `redact_foreign_ids`'s exact contract: for `TeamScope::All`
the error is returned unchanged; otherwise `TicketAlreadyLinked`, `RemoteTaken`, and
`DependencyCycle` become `Error::Redacted { code: e.code(), message }` with the same fixed
messages as today, and every other error is returned unchanged. It is a pure function over an
error value — no SQL, no I/O — so it sits beside the type it consults, in `teams.rs`.

## §2 The console write paths

`register_repo` and `update_repo` call `ctx.team_scope(&state.platform)` after the existing
authorization checks and before the transaction opens (the same position `list_repos` uses), and
map the `register_repo` / `update_repo` result with `|e| scope.redact_foreign_ids(e)` before `?`.
`ApiError::from(of_core::Error)` already maps `Redacted { code: "remote_taken", .. }` to `409`
(`crates/of-web/src/error.rs`), and the body keeps `code: "remote_taken"`, so a console client
branching on the code sees no difference.

`update_repo` resolves its target with `resolve_repo_visible_held(.., &scope, Hold::ForUpdate)`:
a repo the scope cannot see is `404 repo_unresolved` like a missing one, its slug kept out of the
error's list of registered repos, and the row is held so a team reassignment cannot commit between
the visibility check and the write. For admins (`TeamScope::All`) the result is the same repo
`resolve_repo` returned, now locked for the rest of the transaction, which `update_repo` writes
anyway.

## §3 Testing

- `of-core` unit tests in `teams.rs`: `All` passes `RemoteTaken` (and the others) through intact;
  `Teams(..)` turns each of the three into `Redacted` with the original `code()` and no slug or job
  id in the message; an unrelated error passes through unchanged.
- `of-web` unit test in `error.rs`: a `Redacted { code: "remote_taken", .. }` is `409` with code
  `remote_taken` and the redacted message.
- `of-web` integration test beside `a_hidden_repo_is_not_found_and_not_named_anywhere`
  (`crates/of-web/tests/team_scoping.rs`): after the owner adds a remote to `platform-repo`, a
  member on another team posting that remote (register) is refused without `platform-repo`
  appearing anywhere in the body; the owner posting it gets `409 remote_taken` naming
  `platform-repo` (redaction does not over-apply to an unrestricted caller). The same pair for
  `PATCH .../repos/shared` with `addRemotes`.
- Existing `of-mcp` suites cover the moved function's MCP behaviour unchanged.

## Assumptions

- **Defence in depth is the intended fix.** The issue asks for redaction "the same way"; with the
  admin gate in place the redaction cannot fire today, and the reasonable reading is to install it
  anyway rather than close the issue as not reproducible.
- **The method lives on `TeamScope`.** The decision is "what may this scope be told", and both
  callers already hold a `TeamScope`; a free function in `error.rs` would need to import `teams`
  the other way round.
- **The `of-web` integration test cannot drive the redacted branch over HTTP** while the admin gate
  stands. It pins the member's answer to exactly `403` (so relaxing the gate fails it on purpose
  and forces a decision about what a team-limited caller sees), and the owner's `409` naming the
  repo; the unit tests pin the redaction itself. Deleting a route's `map_err` is therefore not
  caught by a test today. Extracting the handler body to test it with a hand-built `TeamScope` was
  judged heavier than this defence-in-depth change warrants; the gate's tripwire is the guard.

## Goal & success criteria

The console's repo writes apply the same hidden-repo redaction as MCP's, from one definition.

- `redact_foreign_ids` exists once, in `of-core`; `git grep 'fn redact_foreign_ids'` finds one hit.
- Both console repo write handlers map their `of-core` error through it.
- The new tests pass; `cargo test --workspace`, clippy `-D warnings`, and fmt are clean.
- No public interface changes (wire codes, statuses, and messages for existing callers identical).

## Error handling & edge cases

- `team_scope` for an admin returns `All` without a platform call, so the console write path gains
  no new failure mode for the callers who can reach it today. For a non-admin it would be one
  `member_teams` lookup that fails closed (`503`), but `require_admin` refuses them first.
- A `Platform` error from `team_scope` is the existing `503 platform_unavailable` mapping.

## Risks & open questions

- Low risk: a pure function moved between crates plus two `map_err`s. The main risk is wording
  drift between surfaces, removed by having one definition.
