# GitHub webhook delivery dedupe design

> **Status:** IMPLEMENTED — shipped in PR #268 (squash bb7c0b3). Records every verified GitHub
> tracker-webhook event in a new tenant table, keyed by the SHA-256 of its signed body, in the same
> transaction as its effects, and acknowledges a repeat with `200` before `inbound_decision` runs.
> A body whose shape does not match its unsigned `X-GitHub-Event` is rejected before it is recorded.
> Swept after 30 days. Closes savvagent/otto-factory#265. Deferred review findings: the replay past
> the retention window and installation moves are in savvagent/otto-factory#271; the pinned-only
> insert policy, a positive event-shape check, JIRA replay and logging the repeat branch at `warn` are in
> savvagent/otto-factory#273.
>
> **Follows:** `docs/specs/2026-10-28-late-comment-stale-mark-design.md` (PR #263), whose security
> review found the gap (#259).

## Brief

From savvagent/otto-factory#265, verbatim:

> ## Problem
> GitHub's `X-Hub-Signature-256` signs a webhook body but carries no timestamp, and the tracker
> webhook route does not record `X-GitHub-Delivery` ids. So a captured signed delivery can be
> replayed. For issue events, the `remote_revision` echo check limits the damage. Since #263, a
> comment event is guarded only by `ticket_changed_at`, so a replayed comment can set a job's
> `ticketStale` mark where it used to be dropped. That can happen only once per job, because the
> column only moves forward.
>
> ## Proposed change
> Record GitHub delivery ids, with a retention window, in a tenant table (per the CLAUDE.md
> tenant-table rules: `org_id NOT NULL`, a `<table>_tenant_isolation` policy, a cross-org negative
> test). Drop a repeated delivery before `inbound_decision` runs, the same way `platform_events`
> deduplicates platform webhooks.
>
> ## Acceptance criteria
> - [ ] A redelivered GitHub webhook with a delivery id already seen is acknowledged with `200`
>   and has no effect.
> - [ ] Delivery ids are scoped per org and swept after the retention window.

## Goal & Success Criteria

A signed GitHub webhook event takes effect at most once per org within the retention window,
whether it comes back as GitHub's own redelivery or as an attacker's replay of a captured request.

- GitHub's redelivery (the same `X-GitHub-Delivery` id and the same body) answers `200
  {"ok":true}` and changes nothing (no job created, rewritten, marked, or closed; no completion row).
- A second delivery of a byte-identical signed body under a **different** (or no) delivery id is
  treated the same way.
- A different body under an already-seen delivery id is a new delivery and applies: the unsigned id
  never decides a repeat.
- A signed body sent under the wrong (unsigned) `X-GitHub-Event` is rejected with the uniform `404`
  and recorded nowhere, so it cannot pre-empt the genuine delivery.
- A delivery whose processing fails (a `5xx`) records nothing, so a manual redelivery (GitHub does
  not retry on its own) still applies it.
- The same delivery id or body in another org is not a repeat (per-org keys), and a pinned
  transaction cannot read, delete, or forge another org's rows (RLS test as `otto_app`).
- Rows older than 30 days are deleted by the existing hourly sweeper, including where RLS applies
  to the connecting role.

## Premise corrections

1. **`X-GitHub-Delivery` is not signed.** `X-Hub-Signature-256` is an HMAC over the body only
   (`crates/of-trackers/src/webhook.rs`); every header, including the delivery id, is
   attacker-controlled on a replay. Deduplicating by delivery id alone stops GitHub's own
   redeliveries (which reuse the id) but not the replay the issue describes: the attacker sends the
   captured body and signature with a fresh GUID. And as a unique key it would let any request
   holding some signed body mark a not-yet-used id as seen. So the dedupe key is the SHA-256 of the
   raw body alone, which the signature does bind: a replay must send the identical bytes. GitHub's
   redelivery resends the same body, so the acceptance criterion as written (a seen delivery is
   acknowledged without effect) holds through the body key; the id is stored for diagnosis only.
   `X-GitHub-Event` is unsigned too, and an `issue_comment` body carries everything an `issues`
   payload needs, so the parser rejects an `issues` payload carrying a `comment` object (the reverse
   already fails: `issue_comment` requires one). Each signed body then parses under one event only,
   so the event name need not be part of the key.
2. **Byte-identical bodies are the same event.** Every GitHub `issues` / `issue_comment` payload
   carries the issue's `updated_at` and, for comments, the comment id and author, so two distinct
   events with identical bytes do not occur in practice. Were they to, the second is the same input
   to `inbound_decision` as the first, so dropping it changes nothing an echo/`ticket_changed_at`
   guard would not have dropped anyway.

## Scope

**In:**

- Migration `0010_tracker_deliveries.sql`: the table, its two-branch RLS policy, grants.
- `of_core::tracker_deliveries`: `record` (insert-if-new in the caller's pinned `Tx`) and `sweep`
  (org-less retention delete).
- `of_trackers::webhook::github_delivery`: the body digest, and the validated delivery-id header.
- `of_trackers::webhook::parse_github`: reject an `issues` payload carrying a `comment` object.
- `of-web`'s `/webhooks/{provider}` route: record a verified GitHub `Event` before `apply`, in each
  pass's transaction; acknowledge a repeat with `200`.
- `of-server`'s `sweep_loop`: also sweep `tracker_deliveries`.
- `platform_events::org_deleted`: purge the org's rows.

**Out:**

- **JIRA.** Its webhooks authenticate with a shared secret in the URL, not a signature over the
  body; anyone holding the secret can forge any event, so replay is not the threat a dedupe table
  addresses there. Not changed.
- **`ParsedWebhook::Ignored` deliveries** (pings, events otto-tower does not read). They have no
  effect to repeat, so they are not recorded (no rows for traffic that cannot matter).
- **A replay older than the retention window.** It is applied again, guarded as today by
  `remote_revision` (issues) and `ticket_changed_at` (comments). The window is long next to
  GitHub's own redelivery limit (3 days) and the rows are small.
- No MCP tool, console route, env var, or result shape changes (no metering classification needed).

## §1 Table

```sql
CREATE TABLE tracker_deliveries (
  org_id      uuid             NOT NULL,
  provider    tracker_provider NOT NULL,
  digest      bytea            NOT NULL CHECK (octet_length(digest) = 32),
  delivery_id text             CHECK (delivery_id IS NULL OR length(delivery_id) BETWEEN 1 AND 128),
  received_at timestamptz      NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, provider, digest)
);
CREATE INDEX tracker_deliveries_received_idx ON tracker_deliveries (received_at);
```

`provider` is kept (rather than a GitHub-only table) so the key is honest about what it scopes and
a future provider needs no schema change; today only `github` rows are written. No foreign key to
anything (orgs are the platform's).

**Tenant isolation.** `org_id NOT NULL`; `ENABLE` + `FORCE ROW LEVEL SECURITY`; policy
`tracker_deliveries_tenant_isolation` with the two branches `platform_events` has since
`0002_org_less_retention.sql` (`org_id = current_org() OR current_org() IS NULL`, for `USING` and
`WITH CHECK`), because the sweeper runs on the pool with no org pinned and a DELETE's `WHERE` is
subject to the SELECT policy. Only background code is unpinned. Grant `SELECT, INSERT, DELETE` to
`otto_app` when it exists (the `0003_console_sessions.sql` pattern). The table is created in its
own migration rather than added to `0001_baseline.sql`'s `tenant_tables` array, which an applied
migration cannot gain; `Db::verify_tenant_isolation` discovers it by the policy name.

The migration is schema-only: it reads and rewrites no existing row, so the per-org data-migration
loop in `CLAUDE.md` does not apply.

## §2 `of_core::tracker_deliveries`

```rust
/// Record a verified delivery in `tx`; `false` if this org already recorded
/// the same digest. `delivery_id` is stored for diagnosis only.
pub async fn record(tx: &mut Tx<'_>, provider: Provider, delivery_id: Option<&str>,
                    digest: &[u8; 32]) -> Result<bool>;
/// Delete rows older than `keep_days`, for every org. Background only.
pub async fn sweep(db: &Db, keep_days: i32) -> Result<u64>;
```

`record` is `INSERT ... VALUES (tx.org(), ...) ON CONFLICT (org_id, provider, digest) DO NOTHING`
and returns `rows_affected() == 1`. The explicit
`org_id` is `tx.org()` (guard 1); the pinned policy is guard 2. Under concurrency the second insert
of the same key waits for the first transaction: if it commits, the second sees the conflict and is
a repeat; if it rolls back (a failed delivery), the second inserts and applies. Exactly once either
way.

`sweep` is `DELETE FROM tracker_deliveries WHERE received_at < now() - make_interval(days => $1)`
on the pool, like `platform_events::sweep`.

## §3 Delivery key

`of_trackers::webhook::github_delivery(headers, raw_body) -> Result<GithubDelivery>` owns what makes
a GitHub delivery unique, so `of-web` only passes it on. `GithubDelivery { id, digest }`:

- `id` is `X-GitHub-Delivery`: absent → `None` (the digest still dedupes); present → must be 1–128
  bytes of visible ASCII (GitHub sends a GUID), else an error the route answers with the same uniform
  `404` as any other rejected webhook.
- `id` is never a key; it is stored beside the digest for diagnosis.
- `digest` is SHA-256 of the raw signed body alone. No unsigned header is part of it: a replay under
  any other headers is the same delivery. The event header, which decides how the body is parsed,
  is instead checked against the body's shape by `verify_and_parse` (an `issues` payload with a
  `comment` object is rejected), so a body sent under the wrong event is refused before it is
  recorded and cannot use up the genuine delivery.

It is computed only after the signature verifies, so an unsigned request never reaches it.

## §4 Route

In `receive`, for `Provider::Github` with `ParsedWebhook::Event`, the route computes
`github_delivery` once and calls `tracker_deliveries::record` at the start of **each**
pass, before `apply`:

- pass 1, in the transaction `begin_live` opened. `false` → log at info (`provider`, `org_id`,
  delivery id) "repeated webhook delivery; acknowledged without effect", drop the transaction,
  answer `200 {"ok":true}`.
- pass 2 (after `Applied::NeedsStatus`): pass 1's transaction, and its row, were rolled back, so the
  record is made again in the new transaction. `false` here means a concurrent delivery of the same
  event committed during the standing lookup; it is acknowledged the same way.

The record therefore commits only with the delivery's effects, as `platform_events` records its
events (`first_delivery`): a delivery that fails rolls its record back, and a manual redelivery
applies.

## §5 Retention and purge

`of-server`'s hourly `sweep_loop` also calls `tracker_deliveries::sweep(&db, 30)`
(`TRACKER_DELIVERY_RETENTION_DAYS`), logging rows swept or a warning on failure, independently of
the platform-event sweep (one failing does not skip the other). `platform_events::org_deleted` adds
`tracker_deliveries` to the tables it purges (the org's connections go with it, so nothing can
match a recorded delivery again).

## §6 Testing

- `of-core` (`tests/tracker_deliveries.rs`): `record` true then false for the same body; true for
  a new body with a seen delivery id; true for the same body in another org (per-org); cross-org
  negative test — a pinned transaction as `otto_app` with deliberately unscoped SQL sees, deletes,
  and can forge none of another org's rows; `sweep` deletes only rows past the window, for two orgs,
  on a pool running as `otto_app` (the `housekeeping_deletes_where_row_level_security_applies`
  shape). The `org_deleted` purge is covered in `tests/platform_events.rs`
  (`org_deleted_purges_that_org_and_only_that_org`).
- `of-trackers`: `github_delivery_id` absent / valid / oversized / non-ASCII; `github_delivery`'s
  digest is the body's SHA-256, unchanged by the event or delivery-id headers; a comment body under
  `issues` and an issue body under `issue_comment` are both rejected.
- `of-web` (`tests/webhooks_http.rs`). The existing guards (`remote_revision`, `ticket_changed_at`)
  would also drop most repeats, so each test first clears the job's `ticket_changed_at` with direct
  SQL (the state #264's relink can leave) so that only the dedupe can be what drops the repeat:
  (a) a comment under delivery id X marks the job; after the reset, the same body under the same X
  (GitHub's redelivery) is `200` and leaves `ticket_changed_at` NULL; a *different* body under the
  same X then applies (the id is not a key). (b) a comment marks the job; after the reset, the byte-identical body and signature
  under a new delivery id is `200` and leaves it NULL; a different body under a new id then applies
  (control). (c) a malformed delivery id is `404`. (d) a comment body sent first under `issues` is
  `404`, records nothing, and does not stop the genuine `issue_comment` delivery from marking the
  job. (e) a billed close (the
  two-pass path) leaves its delivery recorded. Existing tests that post an identical body twice
  forget the delivery records before the repeat, so they still exercise the job's own guards.
- `of-server`: `the_whole_router_assembles` unchanged; the sweeper change is covered by the of-core
  sweep test.

## Error Handling & Edge Cases

- **Database error in `record`** → `ApiError::from` → `5xx`, transaction rolled back, GitHub
  retries. Never read as "not a repeat".
- **Delivery recorded but binding missing, or the event dropped** (an event for a repo not yet
  bound, or an action `inbound_decision` drops): recorded anyway, so a manual GitHub redelivery after
  binding the repo within 30 days is acknowledged as a repeat. Accepted: an event is judged once;
  `sync_ticket` or a later change on the ticket brings the job in. Recording it also means a body
  captured before the repo was bound cannot be replayed into it afterwards, within the window. The
  route's comment says so.
- **Connection changed between passes** (the existing `still_verified` drop): returns before pass
  2's `record`, so nothing is recorded and a retry is not mistaken for a repeat.
- **Missing delivery header**: body digest only.

## Assumptions

- Retention 30 days, matching `PLATFORM_EVENT_RETENTION_DAYS`: long next to GitHub's 3-day manual
  redelivery window, and the cost is one small row per event.
- Dedupe on the body digest alone (premise correction 1); the AC's delivery id alone would leave the
  replay open, and as a second key it would let an unsigned value suppress a signed body.
- A malformed delivery header is rejected, not ignored: GitHub never sends one, and silently
  treating it as absent is the kind of fallback `CLAUDE.md` rules out.
- Migration number `0010`; re-checked against `origin/master` before the PR opens.

## Risks & Open Questions

- **Another PR adds `0010` first.** Renumber at PR time.
- **Retention past 30 days**: a replay then is applied again. Against an existing job it is bounded
  by the monotonic guards (`remote_revision` for issues; `ticket_changed_at` for comments, once per
  job); if the job is gone, a replayed `opened` or `labeled` event creates it again. Closing this
  needs an acceptance window on the payload's own timestamp no longer than the retention window;
  left to savvagent/otto-factory#271.
- **Installation moved to another org**: the GitHub App's signing secret is app-wide, but the keys
  are per org, so bodies recorded under the old org are unseen by the new one. Accepted: moving an
  installation is a rare admin action, and a cross-org unique key would let one tenant's rows decide
  another's writes. Recorded in savvagent/otto-factory#271.
- **Two genuinely different events with byte-identical bodies** (for example label, unlabel and
  relabel within one second, since GitHub timestamps have one-second resolution) are one delivery:
  the second is dropped. Accepted as rare; a later change on the ticket corrects it.
