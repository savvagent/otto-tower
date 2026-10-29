# Tracker webhook dedupe hardening design

> **Status:** DRAFT — splits `tracker_deliveries`' policy so an unpinned statement can no longer
> insert, adds a positive per-event action check to `parse_github`, logs a repeated delivery at
> `warn` with the original row's `received_at`, and records the decision not to dedupe JIRA
> webhooks. Closes savvagent/otto-factory#273.
>
> **Follows:** `docs/specs/2026-10-28-github-delivery-dedupe-design.md` (PR #268, #265), whose
> security reviews found these gaps.

## Brief

From savvagent/otto-factory#273, condensed (the issue is the contract):

1. `tracker_deliveries_tenant_isolation`'s `WITH CHECK (org_id = current_org() OR current_org() IS
   NULL)` lets an unpinned statement insert rows for any org. Only the org-less sweep needs the
   `IS NULL` branch, and it only reads and deletes. Split into an `INSERT` policy that is pinned-only
   and `SELECT`/`DELETE` policies that keep both branches, in a **new** migration, with an
   `rls_scopes_*` test run as `otto_app`.
2. `parse_github` separates `issues` from `issue_comment` bodies by one negative check (an `issues`
   body carrying `comment` is refused). Add a second, positive check.
3. JIRA has no replay protection. Implement dedupe by issue `updated` + event type with a test, or
   record the decision not to in a spec.
4. Log the repeat branch at `warn` with the original row's `received_at`.

Acceptance criteria (verbatim):

- An unpinned `INSERT` into `tracker_deliveries` as `otto_app` is refused, through separate
  `INSERT` and `SELECT`/`DELETE` policies, and an `rls_scopes_*` test run as `otto_app` pins it.
  The org-less sweep still works.
- An `issues` / `issue_comment` body cannot be applied under the other event name, and a test pins
  the positive check.
- JIRA replay handling is either implemented with a test, or the decision not to implement it is
  recorded in a spec.
- A repeated GitHub delivery is logged at `warn` with the original row's `received_at`.

## Goal & Success Criteria

Each backstop the #268 reviews named is either in place or deliberately declined, in writing.

- As `otto_app` with no org pinned, `INSERT INTO tracker_deliveries` fails with an RLS violation;
  `sweep` still deletes every org's expired rows; a pinned transaction still records, sees and
  deletes only its own org's rows.
- `Db::verify_tenant_isolation` still reports `tracker_deliveries` as a tenant table.
- An `issue_comment` body sent as `issues` is rejected whatever its action, and so is an `issues`
  body sent as `issue_comment`; an action that only the other event documents is rejected on its
  own, independently of the `comment` check.
- A repeat is logged once at `warn`, naming the provider, org, delivery id and the original
  `received_at`.

## Premise corrections

- The issue offers "validate `action` against each event's documented action set" as the positive
  check. The two sets overlap: `edited` and `deleted` are documented for both `issues` and
  `issue_comment`. So the action check alone cannot separate an `edited` comment body from an
  `edited` issue body. It is a second check for every other action, and the structural checks (an
  `issues` body must not carry `comment`; an `issue_comment` body must) still decide the overlap.
  This is recorded rather than papered over (§2).
- The alternative the issue offers, binding the event name into the digest, would not add a check:
  it moves which key a body is recorded under, but whether the body is *applied* under the other
  event still rests on the same parse rejection. It is not done.

## Scope

**In:**

- Migration `0011_tracker_deliveries_pinned_insert.sql` (§1).
- `parse_github`'s per-event action check (§2).
- `of_core::tracker_deliveries::record` returning the original `received_at` on a repeat, and the
  route logging it at `warn` (§3).
- The JIRA decision (§4), recorded here.

**Out:**

- JIRA replay protection itself (§4 says why).
- Replay after the retention window, and installation moves: savvagent/otto-factory#271.
- Any change to what an applied event does (`inbound_decision` is untouched).
- No MCP tool, console route, config key or crate edge changes. No public interface changes.

The three constraints: repo anchoring, substrate-not-workflow and agent agnosticism are untouched;
this is the server hardening its own ingest.

## §1 Pinned-only insert policy

New migration `crates/of-core/migrations/0011_tracker_deliveries_pinned_insert.sql`
(`0010_tracker_deliveries.sql` is applied and is never edited):

```sql
DROP POLICY tracker_deliveries_tenant_isolation ON tracker_deliveries;

CREATE POLICY tracker_deliveries_tenant_isolation ON tracker_deliveries
  FOR SELECT USING (org_id = current_org() OR current_org() IS NULL);
CREATE POLICY tracker_deliveries_insert ON tracker_deliveries
  FOR INSERT WITH CHECK (org_id = current_org());
CREATE POLICY tracker_deliveries_sweep ON tracker_deliveries
  FOR DELETE USING (org_id = current_org() OR current_org() IS NULL);
```

- **The `SELECT` policy keeps the `<table>_tenant_isolation` name**, because
  `verify_tenant_isolation` (`otto-tenant`, `isolation::gather`) discovers tenant tables by a policy
  of exactly that name, of any command. Dropping and recreating it inside the migration's one
  transaction leaves no window without it; RLS stays `ENABLE`/`FORCE` throughout (`0010`), and a
  table under RLS with no applicable policy denies, so the window would fail closed regardless.
- **`DELETE` needs the `SELECT` policy too**: a `DELETE`'s `WHERE` reads the rows it filters, so
  the table's `SELECT` policies apply to it (`0002_org_less_retention.sql`). Both keep the two
  branches, so the unpinned sweep reaches every org and the pinned `org.deleted` purge reaches its
  own.
- **No `UPDATE` policy**, as before in effect (`otto_app` was never granted `UPDATE`); now, under
  `FORCE`, the owner cannot update either. Nothing updates a row. Tests that age rows do so as the
  superuser, which bypasses RLS.
- `INSERT … ON CONFLICT DO NOTHING` (`record`) is checked against the `INSERT` policy; a conflict
  check does not need an `UPDATE` policy.
- Schema-only: no row is read or rewritten, so the per-org loop rule does not apply.

**Test** (`crates/of-core/tests/tracker_deliveries.rs`, beside
`rls_scopes_tracker_deliveries_to_the_pinned_org`):
`rls_scopes_an_unpinned_tracker_delivery_insert_to_nothing`, an unpinned transaction with
`SET LOCAL ROLE otto_app` and no `app.org_id`: an `INSERT` for some org fails, while in a second
such transaction `DELETE … WHERE received_at <= now()` removes the existing rows of two orgs. The
predicate matters: a `DELETE` with no `WHERE` reads no rows and so is not bound by the `SELECT`
policy, and would pass even if that policy lost its `IS NULL` branch (the
`the_sweep_reaches_every_org_where_rls_applies` test still covers `sweep` itself). The existing
pinned test keeps proving a pinned insert for another org fails. A catalog assertion pins the shape:
`pg_policy` holds exactly the three policies above with commands `r`, `a`, `d`.

## §2 Positive event-shape check

`crates/of-trackers/src/webhook.rs`, `parse_github`. Two constant sets, from GitHub's webhook
reference:

- `ISSUES_ACTIONS`: `opened`, `edited`, `deleted`, `transferred`, `pinned`, `unpinned`, `closed`,
  `reopened`, `assigned`, `unassigned`, `labeled`, `unlabeled`, `locked`, `unlocked`, `milestoned`,
  `demilestoned`, `typed`, `untyped`.
- `ISSUE_COMMENT_ACTIONS`: `created`, `edited`, `deleted`.

For a parsed `issues` or `issue_comment` payload, in order after deserialization and the existing
`comment` check:

1. The action is documented for this event: proceed as today.
2. Otherwise, the action is documented for the **other** event only (e.g. `created` under `issues`,
   `opened` under `issue_comment`): `Error::InvalidWebhook("… X-GitHub-Event header does not match
   its body")`, the same rejection as the `comment` check, logged and answered `404` by the route.
3. Otherwise (an action neither documents, such as one GitHub adds later): proceed as today. It is
   not a mismatch, and this check does not judge it.

Why (3) proceeds rather than being ignored: an unknown *issue* action already does nothing
(`inbound_decision` acts only on `opened`, `edited`, `labeled` and `closed`), but an unknown
*comment* action on an existing job reaches `github_comment_decision`, which never looks at the
action and can set the job's `ticketStale` mark. Ignoring it would silently drop that signal
(found in PR #276's review; an earlier draft of this section ignored it). Proceeding also keeps
GitHub from marking a genuine delivery with a newer action as failed. Such a body still passes
both structural checks, which is what separates the events for it.

Together with the structural checks, a body is applied under at most one event name:

| Body sent as … | action in both sets (`edited`, `deleted`) | action of the other event only | action of neither |
|---|---|---|---|
| comment body as `issues` | rejected (`comment` present) | rejected (`comment` present, and action) | rejected (`comment` present) |
| issue body as `issue_comment` | rejected (no `comment`, deserialization fails) | rejected (no `comment`, and action) | rejected (no `comment`) |

The `edited`/`deleted` column and the "neither" column are where the structural check is the only
one: the first is the overlap in the premise corrections, the second an action the lists do not
know yet. The doc comment on `parse_github` says so.

**Tests** (unit, in `webhook.rs`'s test module): an `issue_comment` fixture whose `comment` is
removed and action set to `created`, sent as `issues`, is rejected by the action check alone (it
would pass the `comment` check); an `issues` fixture sent as `issue_comment` with an added `comment`
object and action `opened` is rejected by the action check alone; a body of either event with
action `frobnicated` parses as an event under its own name, as before; every action in each set
parses as an event under its own name.

## §3 Repeat logged at `warn` with the original `received_at`

`of_core::tracker_deliveries::record` returns:

```rust
pub enum Recorded {
    /// First delivery: recorded in `tx`.
    First,
    /// Already recorded for this org. `received_at` is when the original was;
    /// `None` only if the row was swept between the insert and the read.
    Repeat { received_at: Option<DateTime<Utc>> },
}
```

On conflict, a second statement in the same transaction reads the original:
`SELECT received_at FROM tracker_deliveries WHERE org_id = $1 AND provider = $2 AND digest = $3`. A
separate statement, not a CTE: under `READ COMMITTED` the `INSERT` waits for a concurrent first
delivery's transaction, and only a new statement's snapshot sees the row it committed. Both
statements carry `org_id` and run pinned.

`of-web`'s `first_delivery` logs the repeat at `warn` with `provider`, `org_id`, `delivery_id` and
`original_received_at` (RFC 3339, or `-`), and answers `200` as before. No body, digest or secret is
logged. A GitHub redelivery and a replay look alike in what is signed; the original time is what an
operator compares against GitHub's delivery log.

**Tests:** `of-core` tests assert `Repeat { received_at: Some(t) }` with `t` the first row's time;
the existing bool-shaped tests move to `matches!`, and the `record` call in
`crates/of-core/tests/platform_events.rs` (`populate`) compiles against the new type unchanged (it
only `unwrap`s). The log line itself is not asserted (no log
capture harness in this repo); the `of-web` HTTP repeat test still asserts `200` and no effect.

## §4 JIRA replay: decided against, for now

JIRA Automation authenticates with a per-org shared secret in `X-OF-Webhook-Secret`, not a
signature over the body. Whoever holds the secret can send any body, so:

- **A dedupe key on `issue.fields.updated` + webhook event type does not stop a replay.** Anyone
  able to replay a captured request holds the secret it carried, and with it can forge a fresh body
  with any `updated` they like. Dedupe would only catch exact repeats by a party that already has
  full write power over the endpoint.
- **It would drop genuine events.** Two JIRA comments posted in the same second carry the same
  `updated` and the same action, and a rule that fires twice on one change sends identical keys.
  The digest dedupe for GitHub cannot do this, because two genuine GitHub events never share a body.
- **The damage of an exact replay is already bounded** the way GitHub's was before #265: an issue
  update, close or comment on an existing job is held back by the `remote_revision` echo check
  (`is_stale_or_echoed`); JIRA comments never take the `ticket_changed_at` path that #265 closed
  for GitHub (`inbound_decision` sends only GitHub comments to `github_comment_decision`).

What would actually close it is moving JIRA to a transport that signs the body (Jira Cloud's
admin-registered webhooks can carry an HMAC signature over the body), which would let the GitHub
digest dedupe apply unchanged. That is a provider-setup change with its own console and docs work, so it is not
done here. It is filed as savvagent/otto-factory#275.

## Assumptions

- **The action lists are GitHub's documented sets as of 2026-10.** A future action falls into
  "neither" and is ignored, which matches what `inbound_decision` does with it today.
- **Rejecting an other-event action is a `404` like the `comment` check**, not an ignore: the
  header and body disagree, which a genuine GitHub delivery never does.
- **Policy names `tracker_deliveries_insert` and `tracker_deliveries_sweep`** follow no enforced
  convention; only the `SELECT` one has to keep `_tenant_isolation`.
- **`warn`, not `error`:** a repeat is expected (manual redeliveries) and has no effect.
- **`Recorded::Repeat`'s `received_at` is optional** rather than an error when the row vanished in
  between: the repeat was still a repeat, and the log is diagnostic.

## Error Handling & Edge Cases

- A concurrent first delivery: the second `INSERT` waits, conflicts, and the follow-up `SELECT`
  (new snapshot) reads the committed row's time. If the first rolled back, the second inserts.
- The sweep deletes the row between the two statements: `Repeat { received_at: None }`, logged `-`.
- An unpinned caller other than the sweep trying to record: now an RLS error rather than a
  silent cross-org row. No such caller exists; `record` takes a `Tx`.
- Deployment shapes: where the connecting role bypasses RLS, the policies change nothing for the
  owner, as for every tenant table; guard 1 (`Tx` + `org_id = $1`) stays the guard there.

## Risks & Open Questions

- **GitHub adds an action to both events** (as `edited`/`deleted` are), or to one event only:
  until the lists are updated it parses as before under its own event, separated by the structural
  checks alone.
- **GitHub extends one event with an action already in the other's list** (say `pinned` for
  `issue_comment`): until the lists are updated, rule 2 rejects that genuine delivery with `404`
  and GitHub shows it as failed. Nothing applied today is lost (the structural checks still separate
  the bodies, and `inbound_decision` acts on no such comment action), so the cost is a noisy
  delivery log until the lists catch up; the rejection's `warn` line names the action.
- **A previously-applied comment action that is not in the list** still applies: an action neither
  list knows is never refused or ignored by the action check.
- **JIRA signed webhooks** are deferred to savvagent/otto-factory#275 (§4).
