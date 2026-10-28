# Late comment stale mark design

> **Status:** IMPLEMENTED (PR #263, 2026-10-28; merged as 77dc932). A GitHub comment webhook is guarded by the job's `ticket_changed_at` instead of
> its `remote_revision`, so a human comment delivered after a newer issue revision was stored still
> marks the job `ticketStale`. No tool, field, route, migration or env var changes.
> Closes savvagent/otto-factory#259.
>
> **Follows:** `docs/specs/2026-10-28-jobs-by-issue-design.md` §5 (the drift marker) and its Risks
> entry "A late comment webhook can miss the mark".

## Brief

From savvagent/otto-factory#259, verbatim:

> `ticketStale` (from #257) is set by a human comment arriving on a GitHub webhook. The existing
> echo check (`is_stale_or_echoed`) runs before the new `InboundDecision::Changed`. A comment event
> can arrive after a newer revision has been stored, either from an issue edit or from
> otto-tower's own write-back. That event is dropped and never marks the job. A comment in the same
> second as the stored revision is dropped too, because the check is `<=`. The next `sync_ticket`
> call still catches any change after the stored revision. Until then the mark is missed.
>
> Give `Changed` a guard of its own: compare the comment event's time with the job's
> `ticket_changed_at` instead of `remote_revision`. Leave the shared echo check in place for
> `Update` and `Close`.
>
> - [ ] A human comment delivered after a later issue edit still marks the job stale.
> - [ ] A redelivered comment is still idempotent.

## Goal & Success Criteria

A person's comment on a bound GitHub issue marks the issue's live job stale whatever order GitHub
delivers it in relative to the issue's other events and otto-tower's own write-back.

- A human comment whose `issue.updated_at` is at or before the job's stored `remote_revision` and
  later than its `ticket_changed_at` (or with no `ticket_changed_at`) yields `Changed { at }`.
- A comment whose time is at or before `ticket_changed_at` (a redelivery, or an older comment than
  one already recorded) yields `Drop`, and the stored mark is unchanged.
- `Update` and `Close` decisions, JIRA comment events, and comments with no job keep their current
  behaviour exactly (the existing tests for them pass unchanged).
- The webhook HTTP test for the acceptance criterion passes against real Postgres.

## Premise corrections

None. The code matches the issue: `inbound_decision` (`crates/of-trackers/src/sync.rs:76`) runs
`is_stale_or_echoed(job.remote_revision, event.issue.updated_at)` for every event on an existing
job, comments included, before the comment arm builds `Changed`.

## Scope

**In:** `of_trackers::sync::inbound_decision`'s handling of a GitHub `IssueComment` event on an
existing job; the comment in `of-web`'s webhook route that describes why `Changed` needs no echo
guard; tests.

**Out:**

- The shared echo check for `issues` events and for JIRA. Unchanged.
- `sync_ticket`'s pull path. It already compares against `remote_revision` for a different reason
  (an issue read, not a comment) and its own known gap is recorded in the #257 spec.
- Clock skew between GitHub's `updated_at` and the database's `now()` in the `described_at`
  comparison. Accepted in the #257 spec; unchanged here.
- Any SQL change. `note_ticket_changed_if_linked` already keeps the later time (`GREATEST`).

Constraints: repo-anchored (unchanged, the job is still found through the binding's repo);
substrate, not workflow (the server still interprets no comment text, only who wrote it and when);
agent agnostic (no client is involved).

## §1 The comment guard

For an existing job and a GitHub `IssueComment` event, `inbound_decision` decides in this order,
and the shared `is_stale_or_echoed` check is never consulted:

1. Job terminal → `NoOp` (as today).
2. Author is a bot → `NoOp` (as today; this is what keeps otto-tower's own write-back comment from
   flagging its job).
3. `issue.updated_at` missing or unparseable → `NoOp` (as today).
4. `at <= job.ticket_changed_at` → `Drop`. The mark already covers this time: a redelivery, or a
   comment older than one already counted.
5. Otherwise → `Changed { at }`.

The trigger-label check still runs first for every event.

Why `ticket_changed_at` and not `remote_revision`: `remote_revision` is the issue revision whose
*body* the job holds, and it moves on issue edits and on otto-tower's write-back. A comment is not
part of the body, so whether it is newer than that revision says nothing about whether the job has
seen it. `ticket_changed_at` is exactly "the newest foreign change already counted", which is the
question a comment's guard asks.

Why it is safe to stop applying the echo check to comments: the echo check exists so that an
`issues` event otto-tower caused (or an older one delivered late) does not rewrite the job. A
`Changed` rewrites nothing; it only raises `ticket_changed_at`. The only echo a comment could carry
is otto-tower's own write-back comment, which step 2 filters by author. An older human comment
delivered late is a real change the job has not absorbed, and whether it makes the job stale is
decided by the existing SQL comparison against `described_at`: a comment older than the last
`update_job` that rewrote the description does not count.

`Drop` rather than `NoOp` in step 4, because it is the same kind of outcome as the echo check's:
an event already accounted for. The webhook route handles both identically (no write).

## §2 The webhook route

No behaviour change. The comment beside the `Changed` arm in `crates/of-web/src/routes/webhooks.rs`
is updated: a redelivered comment is now guarded in `inbound_decision` by `ticket_changed_at`, with
`GREATEST` in `note_ticket_changed_if_linked` as the second line (two deliveries racing past the
guard on separate connections still store the later time once). In `sync.rs`, the bot-filter
comment ("so the echo check above cannot catch it") and the `InboundDecision::Changed` doc comment
are updated to match: no echo check runs ahead of a comment any more.

## Assumptions

1. **`issue.updated_at` on an `issue_comment` event is the comment's time.** GitHub bumps the
   issue's `updated_at` when a comment is created, edited or deleted; #257 already relies on this.
2. **`Drop` for step 4**, not `NoOp`: both are no-ops in the route; `Drop` names it as an event
   already seen.
3. **Equal times are dropped** (`<=`). Two distinct comments in the same second record the same
   `at`, so the second has nothing to add; the job is already marked at that time.
4. **The job's `ticket_changed_at` is read unlocked**, as `existing` is today. A race between two
   deliveries can only let both through, and `GREATEST` makes that harmless.

## Error Handling & Edge Cases

- Comment delivered after a later issue edit: `remote_revision` is the edit's time, newer than the
  comment; `ticket_changed_at` is older or null → `Changed`. The edit does not move `described_at`,
  so the job reads stale.
- Comment delivered after otto-tower's own write-back stored a newer revision: same as above.
- Comment in the same second as the stored revision: no longer compared with it → `Changed`.
- Redelivery of an already-applied comment: `at == ticket_changed_at` → `Drop`.
- Comment older than the last `update_job` description rewrite: `Changed` is applied, but
  `ticket_changed_at` stays not after `described_at`, so `ticketStale` stays false. Correct: the
  agent re-read the issue after it.
- Comment on a terminal job, by a bot, without a usable time, or on an issue with no job:
  unchanged.

## Testing

- `of-trackers` `tests/sync.rs`:
  - Replace `inbound_github_echoed_comment_is_dropped` (which pinned the old behaviour) with a test
    that a comment at or before `ticket_changed_at` is `Drop`.
  - Add: a human comment at or before the stored `remote_revision`, with `ticket_changed_at` older
    or null, is `Changed`.
  - Existing tests for bots, terminal jobs, missing times, JIRA comments and issue edits pass
    unchanged.
- `of-web` `tests/webhooks_http.rs`: a comment at t1 delivered after an issue edit at t2 > t1 marks
  the job stale with `ticket_changed_at == t1`; delivering it again leaves `ticket_changed_at` at
  t1.

## Risks & Open Questions

- **An old human comment now counts where it used to be dropped.** One from before the job existed
  could arrive late (an `edited` or `deleted` action on an old comment carries the issue's current
  `updated_at`, so this is only a delayed `created`). The `described_at`/`created_at` comparison
  keeps it from marking a job created after it. No new false positive beyond what #257 accepted.
- **The bot filter is now the only guard against otto-tower's own comments** (added in review).
  Before, a write-back comment whose webhook arrived after its `remote_revision` was stored was
  also dropped by the echo check. `author_is_bot` is false when a payload has no `comment.user`
  (#257 chose that default, and `webhook.rs` tests pin it). GitHub sends `user.type` on every
  comment and App write-backs are `"Bot"`, so this is a hint-only risk: at worst a job is wrongly
  marked stale. Identifying write-backs by the comment ids otto-tower posts would be a positive
  check; not done here.
- **A replayed signed comment delivery gets further than before** (added in review). GitHub's
  signature carries no timestamp and deliveries are not deduplicated by `X-GitHub-Delivery`
  (pre-existing, for issue events too). A replay can now set the mark where the echo check used
  to drop it, but only once, since `ticket_changed_at` only moves forward. Follow-up: savvagent/otto-factory#265.
- **A relinked job keeps the old ticket's `ticket_changed_at`** (added in review, pre-existing
  from #257). `link_ticket` clears `remote_revision` but not `ticket_changed_at`, so the old
  ticket's mark keeps the job stale, and now also drops new-ticket comments no later than it.
  The visible `ticketStale` is the same as without the guard. Follow-up: savvagent/otto-factory#264.
