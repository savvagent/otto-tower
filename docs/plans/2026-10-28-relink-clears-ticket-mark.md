# Relink clears the ticket mark — `link_ticket` resets `ticket_changed_at` on a new ticket

Goal: when `link_ticket` points a job at a different `(tracker, ticket_ref)`, it clears the job's
`ticket_changed_at`, so the old ticket's drift mark neither keeps the job `ticketStale` nor makes
the comment guard drop the new ticket's comments. A relink to the same ticket keeps the mark;
`described_at` is left alone. Closes savvagent/otto-factory#264.

**Spec:** `docs/specs/2026-10-28-relink-clears-ticket-mark-design.md` — read it first. This plan
implements it exactly.

## Status — 2026-10-28

✅ Shipped in PR #267 (squash dbf9a38). Review added two tests
(`relinking_to_another_tracker_with_the_same_ref_clears_the_mark`, `a_refused_relink_keeps_the_mark`)
and doc wording saying the `ticket_ref` is compared exactly; no follow-up issues.

## Global Constraints

- No AI self-attribution anywhere (commits, code comments, docs, PR body).
- Run `cargo fmt --all` before every Rust commit.
- Every SQL statement lives in `of-core`. This plan changes one existing statement there and adds
  none; it keeps its explicit `org_id = $1`.
- No `unwrap()` outside tests.
- Tests need `podman compose up -d` (Postgres on 15433) and `.env` (`cp .env.example .env`).
- Behaviour fix, not a public-interface change: no tool, field, route, env var or migration
  changes. PR title `fix(of-core): …`. Nothing out of band (no image, console, Worker, migration or
  config change).

## File Structure

| File | Responsibility |
|---|---|
| `crates/of-core/src/jobs.rs` | **Modify.** `link_ticket`'s `UPDATE` clears `ticket_changed_at` when the link changes; its why-comment says why conditionally and why `described_at` is untouched. The `Job::ticket_changed_at` / `ticket_stale` doc comments (rendered into the MCP output schema) name the relink. |
| `crates/of-core/tests/jobs.rs` | **Modify.** Relink to another ticket clears the mark and a new-ticket change earlier than the old mark marks the job; relink to the same ticket keeps it. |
| `crates/of-web/tests/webhooks_http.rs` | **Modify.** End to end: after a relink, a human comment on the new issue earlier than the old mark marks the job. |

## Task Order & Rationale

One task: the SQL change and both test layers are one behaviour. The `of-web` test needs no
production change in `of-web` or `of-trackers`.

## Task 1 — Clear `ticket_changed_at` when `link_ticket` changes the ticket ✅

**Files:** the three above.
**Interfaces:** consumes `Tx::link_ticket`, `Tx::note_ticket_changed_if_linked`; produces no new
interface.

- [x] In `crates/of-core/tests/jobs.rs` (drift-marker section, after
  `a_conditional_mark_skips_a_job_relinked_in_between`), add
  `relinking_to_another_ticket_clears_the_old_tickets_mark`: `fresh` job, link to `acme/api#1`,
  `note_ticket_changed_if_linked(#1, now + 10 min)`; assert stale. Relink to `acme/api#2`; assert
  the returned job and a re-read have `ticket_changed_at == None` and `!ticket_stale`. Then
  `note_ticket_changed_if_linked(#2, now + 1 min)` → `true`; re-read is stale with
  `ticket_changed_at` equal to that time (micros).
- [x] Add `relinking_to_the_same_ticket_keeps_its_mark`: link to `#1`, mark at `now + 10 min`,
  link to `#1` again; assert `ticket_changed_at` unchanged and still stale.
- [x] Run `cargo test -p of-core --test jobs relinking_`. Expected: the first fails
  (`ticket_changed_at` still `Some`), the second passes.
- [x] In `crates/of-web/tests/webhooks_http.rs`, after `a_human_comment_marks_the_issues_job_stale`,
  add `a_comment_on_a_relinked_jobs_new_issue_marks_it_whatever_the_old_mark`: bind `acme/api`,
  `open_issue` 17, post a human comment on 17 at `t2 = hours_from_now(2)` → stale; relink the job
  to `acme/api#18` in a `h.db.begin(org)` tx; assert not stale; post a human comment on 18 at
  `t1 = hours_from_now(1)` → `200`; `job_for(…, 18)` is stale with `ticket_changed_at == t1`.
- [x] Run `cargo test -p of-web --test webhooks_http a_comment_on_a_relinked`. Expected: fails
  (not stale after relink is false).
- [x] In `crates/of-core/src/jobs.rs` `link_ticket`, add to the `UPDATE`:
  `ticket_changed_at = CASE WHEN tracker IS NOT DISTINCT FROM $3 AND ticket_ref IS NOT DISTINCT
  FROM $4 THEN ticket_changed_at END`, and extend the why-comment (spec §1, §2).
- [x] Run `cargo test -p of-core --test jobs` and `cargo test -p of-web --test webhooks_http`.
  Expected: pass, including the existing `link_ticket_*` and drift-marker tests.
- [x] Run `cargo test --workspace` and `cargo clippy --all-targets -- -D warnings`. Expected: pass.
- [x] Format and commit: `cargo fmt --all` then
  `git commit -m "fix(of-core): clear ticket_changed_at when link_ticket changes the ticket"`.
