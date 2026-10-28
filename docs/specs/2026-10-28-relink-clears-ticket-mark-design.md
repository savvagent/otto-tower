# Relink clears the ticket mark design

> **Status:** IMPLEMENTED (PR #267, 2026-10-28; merged as dbf9a38). `link_ticket` clears a job's
> `ticket_changed_at` when it points the job at a different ticket, so the old ticket's drift mark
> neither keeps the job `ticketStale` nor makes the comment guard drop the new ticket's comments.
> No tool, field, route, migration or env var changes. Closes savvagent/otto-factory#264.
> Review added two `of-core` tests beyond the three listed under Testing:
> `relinking_to_another_tracker_with_the_same_ref_clears_the_mark` and `a_refused_relink_keeps_the_mark`.
>
> **Follows:** `docs/specs/2026-10-28-late-comment-stale-mark-design.md` (Risks: "A relinked job
> keeps the old ticket's `ticket_changed_at`") and `docs/specs/2026-10-28-jobs-by-issue-design.md`
> §5 (the drift marker).

## Brief

From savvagent/otto-factory#264, verbatim:

> `link_ticket` (`crates/of-core/src/jobs.rs`, around line 1107) resets a job's `remote_revision`
> to NULL when it is relinked to another ticket, so the old ticket's state cannot hold back the new
> ticket's webhooks. It does not reset `ticket_changed_at` (from #257). So after a relink, the old
> ticket's newest comment still sets `ticketStale` on the job, which now describes a different
> ticket. Since #263, the old mark also makes the comment guard drop any comment on the new ticket
> that is no later than it. The visible `ticketStale` is unchanged by that, because the guard only
> drops writes that `GREATEST` would have ignored.
>
> When `link_ticket` changes the ticket a job points at, clear `ticket_changed_at`, as it already
> clears `remote_revision`. Decide whether `described_at` should be reset too.
>
> - [ ] After relinking a job to a different ticket, `ticketStale` is false until a change on the
>   new ticket is seen.
> - [ ] A comment on the new ticket marks the job, whatever the old ticket's mark was.

## Goal & Success Criteria

A job's drift mark always describes the ticket the job is linked to now.

- After `link_ticket` moves a job to a different `(tracker, ticket_ref)`, the returned job and any
  later read have `ticket_changed_at = NULL` and `ticketStale = false`.
- A human GitHub comment on the new ticket whose time is earlier than the old ticket's mark marks
  the job stale (end to end through `/webhooks/github`).
- Relinking a job to the ticket it is already linked to leaves `ticket_changed_at` (and therefore
  `ticketStale`) as it was.
- The existing `link_ticket`, drift-marker and comment-guard tests pass unchanged.

## Premise corrections

None. `link_ticket` (`crates/of-core/src/jobs.rs:1086`) sets `remote_revision = NULL` and leaves
`ticket_changed_at` alone; `inbound_decision`'s comment guard
(`crates/of-trackers/src/sync.rs:193`) drops a comment at or before `job.ticket_changed_at`;
`ticket_stale` is computed in `JOB_COLS` as `ticket_changed_at > COALESCE(described_at,
created_at)`.

## Scope

**In:** the `UPDATE` in `link_ticket`; its why-comment; the `Job::ticket_changed_at` and `Job::ticket_stale` doc comments (they render into the MCP output schema, and `ticket_stale`'s said only `update_job` clears it); tests in `crates/of-core/tests/jobs.rs`
and `crates/of-web/tests/webhooks_http.rs`.

**Out:**

- `described_at` (decided below, §2: left alone).
- `remote_revision` on a same-ticket relink. It is still reset unconditionally, as today; changing
  that is a separate behaviour with its own tests and not what the issue asks.
- `create_from_ticket`, `update_from_ticket`, `close_from_ticket`: they never change which ticket a
  job points at.
- A migration. The column comment in `0009_job_ticket_drift.sql` ("Newest change to the linked
  issue …") stays true, and an applied migration is never edited.

Constraints: repo-anchored (unchanged; `link_ticket` still resolves the job by `org_id` and id);
substrate, not workflow (the server still interprets no ticket text); agent agnostic (no client is
involved).

## §1 Clearing the mark

The `UPDATE` in `link_ticket` gains:

```sql
ticket_changed_at = CASE WHEN tracker IS NOT DISTINCT FROM $3
                          AND ticket_ref IS NOT DISTINCT FROM $4
                         THEN ticket_changed_at END
```

In a Postgres `UPDATE`, column references on the right-hand side read the row's values before the
update, so the comparison is against the link the job had. A job moving from no ticket, or from a
different tracker or ticket, gets `NULL`; a job relinked to the same ticket keeps its mark.

Why conditional, when `remote_revision` is cleared unconditionally: clearing `remote_revision` on
a same-ticket relink costs at most one redundant inbound write (the echo guard lets the next event
through). Clearing `ticket_changed_at` on a same-ticket relink would silently drop a real, unread
change to the very ticket the job still describes — a false "up to date", which is the failure
the marker exists to prevent. `ticket_ref` comparison is exact text, the same comparison
`note_ticket_changed_if_linked` and `get_job_by_ticket_for_repo` use, so "same ticket" here means
what it means to the webhook that would mark it.

Concurrency: `link_ticket` runs one `UPDATE`, which takes the row lock; a concurrent
`note_ticket_changed_if_linked` for the old ticket either commits first (and its mark is then
cleared by the relink) or runs after and matches no row (its `ticket_ref` predicate fails). A
concurrent `sync_ticket` already refuses to record drift for a job relinked under it
(`record_issue_signals`).

Tenant isolation: no new statement; the existing `UPDATE` keeps `org_id = $1`. The existing
cross-org `link_ticket` test in `crates/of-core/tests/isolation.rs` covers the statement.

## §2 `described_at` is left alone

`described_at` records when a caller last wrote the job's title or description through
`update_job`. A relink does not rewrite either, so it has nothing new to record:

- Setting it to `now()` would hide every change on the new ticket made between the job's last
  description and the relink — the job would read as up to date with a ticket its text was never
  written from.
- Setting it to `NULL` (read as `created_at`) would discard a real fact (an `update_job` that did
  happen) for no gain in accuracy.

Left alone, the rule stays one sentence: the job is stale when the newest change seen on its
current ticket is later than its text. A caller who relinks and then rewrites the description from
the new ticket clears the mark the usual way, through `update_job`.

## §3 Effect on the comment guard

No code change in `of-trackers` or `of-web`. With the mark cleared, `inbound_decision` reads
`ticket_changed_at = None` for the relinked job and lets the new ticket's first human comment
through as `Changed { at }`; `note_ticket_changed_if_linked` stores `at`; `ticketStale` is
`at > COALESCE(described_at, created_at)`.

## Assumptions

1. **"Changes the ticket" means a different `(tracker, ticket_ref)` pair, compared exactly.** The
   rest of the drift marker compares `ticket_ref` exactly, so a case-only difference is a different
   link everywhere, consistently.
2. **The cleared mark is not recoverable.** Relinking back to the old ticket does not restore it;
   the next change seen on that ticket (a webhook or `sync_ticket`) sets it again.
3. **No audit event.** `link_ticket` writes none today; the mark is derived state, not a record.

## Error Handling & Edge Cases

- Job with no ticket linked yet: `tracker`/`ticket_ref` are NULL, `IS NOT DISTINCT FROM` the new
  non-null values is false → `NULL` (it was already `NULL`; a job with no ticket is never marked).
- Same tracker, different ticket: cleared.
- Different tracker, same `ticket_ref` text: cleared.
- Same ticket: kept.
- Unique-violation (another live job holds the ticket): the savepoint rolls the `UPDATE` back, so
  the mark is untouched along with everything else, as today.
- Unknown job: `job_not_found` before the `UPDATE`, as today.

## Testing

- `crates/of-core/tests/jobs.rs`:
  - `relinking_to_another_ticket_clears_the_old_tickets_mark`: link to `#1`, mark at
    `now + 10 min` (stale), relink to `#2` → returned and re-read job have `ticket_changed_at`
    `None` and `ticket_stale` false; then `note_ticket_changed_if_linked(#2, now + 1 min)` (earlier
    than the old mark) → stale with `ticket_changed_at` equal to that time.
  - `relinking_to_the_same_ticket_keeps_its_mark`: link to `#1`, mark, link to `#1` again → mark
    and `ticket_stale` unchanged.
- `crates/of-web/tests/webhooks_http.rs`:
  `a_comment_on_a_relinked_jobs_new_issue_marks_it_whatever_the_old_mark`: open issue 17, comment
  on it at `t2 = now + 2 h` (stale), relink the job to `acme/api#18` with `link_ticket`, assert
  not stale; post a human comment on issue 18 at `t1 = now + 1 h < t2` → `200`, stale,
  `ticket_changed_at == t1`.

## Risks & Open Questions

- **Same-ticket relink still clears `remote_revision`.** Pre-existing; out of scope (see Scope).
  Harmless for the marker, which no longer depends on it.
- **Case-variant `ticket_ref`.** `acme/api#1` vs `Acme/API#1` counts as a different ticket and
  clears the mark. Consistent with the exact comparisons elsewhere; whether ticket refs should be
  normalised is a separate question.
- **A renamed repository's relink.** `sync_ticket` tells an agent whose `ticket_ref` names a
  renamed repository to relink it under the bound repository's name. That is the same issue under
  another spelling, so the exact comparison clears the mark. Accepted: the `Job` doc comments and
  the `link_ticket` description tell the caller to run `sync_ticket` afterwards, which re-reads the
  issue (with `remote_revision` NULL, a foreign change since the job's text was written marks it
  again). Recognising a rename needs knowledge of the rename, which the database does not have.
- **A relink round-trip clears the mark without an audit event.** Relinking A → B → A drops A's
  mark with nothing in the audit trail saying why. Accepted: `ticketStale` is advisory, and
  `update_job` can already clear it.
