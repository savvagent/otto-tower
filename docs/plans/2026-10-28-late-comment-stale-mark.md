# Late comment stale mark — guard a comment webhook by `ticket_changed_at`

Goal: a human GitHub comment marks its issue's live job `ticketStale` even when its webhook arrives
after a newer issue revision was stored. `inbound_decision` stops running the shared
`remote_revision` echo check for GitHub comment events and guards them by the job's
`ticket_changed_at` instead. Closes savvagent/otto-factory#259.

**Spec:** `docs/specs/2026-10-28-late-comment-stale-mark-design.md` — read it first. This plan
implements it exactly.

## Status — 2026-10-28

⬜ Not started.

## Global Constraints

- No AI self-attribution anywhere (commits, code comments, docs, PR body).
- Run `cargo fmt --all` before every Rust commit.
- Every SQL statement lives in `of-core`. This plan adds none; `note_ticket_changed_if_linked`
  is reused as is.
- No `unwrap()` outside tests.
- Tests need `podman compose up -d` (Postgres on 15433) and `.env` (`cp .env.example .env`).
- Additive behaviour fix: no tool, field, route, env var or migration changes. PR title
  `fix(of-trackers): …`. Nothing out of band (no image, console, Worker, migration or config
  change).

## File Structure

| File | Responsibility |
|---|---|
| `crates/of-trackers/src/sync.rs` | **Modify.** GitHub comment events on an existing job take their own decision path (terminal → bot → time → `ticket_changed_at` guard → `Changed`), before the shared echo check. |
| `crates/of-trackers/tests/sync.rs` | **Modify.** Replace the echo-drop comment test; add late-comment and redelivery cases. |
| `crates/of-web/src/routes/webhooks.rs` | **Modify.** Comment beside the `Changed` arm only: where a redelivery is now guarded. |
| `crates/of-web/tests/webhooks_http.rs` | **Modify.** End-to-end: a comment delivered after a later issue edit marks the job; its redelivery changes nothing. |

## Task Order & Rationale

One task: the decision change and both test layers are one behaviour, and the webhook route
needs no code change.

## Task 1 — Guard comment webhooks by `ticket_changed_at` ⬜

**Files:** the four above.
**Interfaces:** consumes `Job::ticket_changed_at` (already on `Job`); produces no new interface.
`InboundDecision` is unchanged.

- [ ] In `crates/of-trackers/tests/sync.rs`, replace `inbound_github_echoed_comment_is_dropped`
  with `inbound_github_comment_already_counted_is_dropped`: a job with `remote_revision`
  `2026-09-03T10:00:00Z` (older than both comments, so today's echo check passes them and the test
  fails first) and `ticket_changed_at = 2026-09-03T12:00:00Z`, set on the `Job` the `job()` helper
  returns; a human comment at `12:00:00Z` and at `11:00:00Z` → `Drop`.
- [ ] Add `inbound_github_comment_older_than_the_stored_revision_is_changed`: job
  `remote_revision = 2026-09-03T12:00:00Z`, comment at `12:00:00Z` (same second) and at
  `11:00:00Z`, with `ticket_changed_at` `None` and with `10:00:00Z` → `Changed { at }`.
- [ ] Run `cargo test -p of-trackers --test sync`. Expected: both new tests fail (`Drop` returned
  for the late comment; `Changed` returned for the already-counted one).
- [ ] In `crates/of-web/tests/webhooks_http.rs`, add
  `a_comment_delivered_after_a_later_issue_edit_still_marks_the_job`: compute
  `let (t1, t2) = (hours_from_now(1), hours_from_now(2));` once; open issue, post an issue edit at
  `t2`, then the same human comment body (label `&["trackers"]`) at `t1` twice. Assert `200` each
  time, `ticket_stale`, `ticket_changed_at == t1`, description is the edit's body,
  `remote_revision == t2`. (The redelivery assertion holds through `GREATEST` alone; the unit test
  is what pins the `Drop` guard.)
- [ ] Run `cargo test -p of-web --test webhooks_http a_comment_delivered_after`. Expected: fails
  (`ticket_stale` false).
- [ ] In `crates/of-trackers/src/sync.rs`, at the top of the `if let Some(job) = existing` block,
  return `github_comment_decision(event, job)` for a GitHub `IssueComment` event, before
  `is_stale_or_echoed`. Move the comment arm's body into that function, add the
  `ticket_changed_at` guard (`at <= seen` → `Drop`), and remove the now unreachable comment arm
  from the `match existing`. Keep the bot-filter comment; replace the arm's ordering comment with
  one on the new function saying why the echo check does not apply to comments (spec §1).
- [ ] Update the `InboundDecision::Changed` doc comment if it describes the old guard, and the
  comment beside the `Changed` arm in `crates/of-web/src/routes/webhooks.rs` (spec §2).
- [ ] Run `cargo test -p of-trackers --test sync` and `cargo test -p of-web --test webhooks_http`.
  Expected: all pass, including the existing comment, bot, terminal, JIRA and issue-edit tests.
- [ ] Run `cargo test --workspace` and `cargo clippy --all-targets -- -D warnings`. Expected: pass.
- [ ] Format and commit: `cargo fmt --all` then
  `git commit -m "fix(of-trackers): guard comment webhooks by ticket_changed_at"`.
