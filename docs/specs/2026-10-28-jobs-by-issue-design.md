# Jobs by issue design

> **Status:** DRAFT. Agents can name a job by its issue (`#928`, `owner/repo#928`) wherever they
> can name it by id. `list_jobs` gains a `ticket` filter, `list_leases` shows each lease's job's
> `ticketRef`, `sync_ticket` turns "Blocked by #N" into a dependency, and a job whose issue has
> drifted carries `ticketStale`. Additive only: no tool, field or route is renamed or removed.
> Closes savvagent/otto-factory#257.

## Brief

From savvagent/otto-factory#257, verbatim acceptance criteria:

- `get_job(job="#928")` in a repo returns the job whose `ticketRef` is that repo's #928; a full
  `owner/repo#928` works across repos.
- An issue reference that matches two or more jobs is refused, and the refusal lists the matching
  job ids.
- `set_dependencies(job="#928", add=["#925"])` resolves both references.
- `list_jobs(ticket="#928")` returns only that issue's jobs.
- `ready`, `list_leases` and `claim_jobs` results include each job's `ticketRef` when it has one.
- `sync_ticket` adds a dependency for a "Blocked by #N" line when #N has a job, and leaves the job
  alone when it doesn't.
- A job whose issue changed after the job was last updated carries a staleness marker in `get_job`
  and `ready`.

Non-goals (from the issue): changing the job id format, and rewriting a job's description from
the issue automatically.

## Premise corrections

What the code already does, checked against `master` at `a609b44`:

1. **`ready` and `claim_jobs` already return `ticketRef`.** Both return full `Job` objects
   (`tools::out::JobsOut`), and `Job` serializes `ticket_ref` as `ticketRef`
   (`crates/of-core/src/jobs.rs:132`). Only `list_leases` lacks it: a `Lease` carries a bare
   `jobId` (`crates/of-core/src/leases.rs:49`). That AC needs only the lease change.
2. **Body edits already reach bound jobs.** For a repo with an active GitHub binding, an `issues`
   webhook with action `edited` becomes `InboundDecision::Update`, which rewrites the job's title
   and description from the issue (`crates/of-trackers/src/sync.rs:92-105`). The issue's
   non-goal is already broken for bound repos, and this change leaves that alone. What is
   actually lost is **comments**: an `issue_comment` with action `created` is a `NoOp`. The job
   in the motivating session (job-52) had no tracker binding at all, so it got no signal of
   either kind.
3. **`sync_ticket` is outbound-only and refuses pending jobs**
   (`crates/of-mcp/src/tools/jobs.rs:1495-1502`). A dependency matters most on a pending job, so
   reading "Blocked by" through `sync_ticket` means it has to accept a pending job.
4. **Stored `ticketRef`s are not uniform.** A webhook-created job stores
   `"{binding_external_ref}#{number}"` (`sync::ticket_ref`, `sync.rs:220`; for GitHub that is the
   payload's `repository.full_name`), and `add_job`/`link_ticket` store
   whatever the caller typed. This org's own jobs say `savvagent/otto-factory#256` while the
   GitHub repo is now `savvagent/otto-tower`. Matching has to be lenient about case, and the
   short form `#N` has to match any owner/repo.
5. **A job id never contains `#`, and neither does a JIRA key.** Ids are `job-N`
   (`crates/of-core/src/ids.rs`), and JIRA keys are `PROJ-123`. So "contains `#`" separates an
   issue reference from a job id with no ambiguity.

## Scope

**In:**

- §1 Issue references accepted in place of a job id by every MCP tool argument that takes one.
- §2 A `ticket` filter on `list_jobs`.
- §3 `jobTicketRef` on each lease `list_leases` returns.
- §4 `sync_ticket` reads the GitHub issue body and adds a dependency for each "Blocked by"
  reference that resolves to exactly one job. It now also accepts pending jobs (read only, no
  write-back).
- §5 A drift marker (`ticketChangedAt`, `ticketStale`) on every `Job`, fed by non-bot GitHub
  issue comments arriving by webhook, and by `sync_ticket` noticing the issue changed since
  otto-tower last touched it. It is cleared by `update_job` changing the title or description.

**Out:**

- JIRA keys as job references (`PROJ-123` in place of a job id). The issue asks for GitHub forms,
  and a bare JIRA key cannot be told apart from a malformed job id by shape alone.
- JIRA "Blocked by" parsing and JIRA comment staleness. JIRA has native issue links, and its
  comment events take a different inbound path (`Update`) that this change does not alter.
- Removing a dependency when its "Blocked by" line goes away. A dependency may have been added by
  hand, and the server cannot tell which ones came from the issue.
- Reading GitHub's native issue-dependency ("blocked by") relationships through the API. Text
  lines are what the issue names, and they work with no extra App permission.
- Rewriting a job's description from a comment (non-goal). The marker only flags the drift.
- Any console (`web/`) change. The console renders jobs from the same JSON and ignores fields it
  does not know.

**Constraint check.** (1) Repo anchoring: references resolve to jobs, and jobs still belong to
repos. Nothing creates a job or lease without one. (2) Substrate: the server records that an
issue changed and which issues block which. It never decides what to do about either, and never
rewrites a description. The only interpretation of issue text is "Blocked by", which the issue
names explicitly and which maps onto a primitive the server already owns (`job_dependencies`).
(3) Agent-agnostic: plain tool arguments and JSON fields, nothing client-specific.

## Goal & success criteria

An agent that thinks in issues can do everything it does with job ids without listing the
queue, and can see when the issue has moved on since the job was written.

- Every tool argument that takes a job id also takes `#N` / `owner/repo#N` (test per tool family).
- An ambiguous reference never resolves. The refusal lists only job ids the caller may see.
- A reference never resolves to another org's job, or to a job of a team the caller is not on
  (cross-org and team negative tests).
- A "Blocked by #N" line in a bound GitHub issue becomes a dependency on `sync_ticket`, and an
  unresolvable one is reported, not guessed.
- A human comment on a bound issue flips `ticketStale` on its live job, and the job's own
  write-back comment does not.

## §1 Issue references as job ids

### Grammar

A **job reference** argument is either:

- a job id: any string without `#`, passed through unchanged (today's behaviour and errors), or
- an issue reference: `#N` (short) or `OWNER/REPO#N` (full), where `N` is 1–18 ASCII digits
  (leading zeros dropped) and `OWNER`/`REPO` are each 1–100 of `[A-Za-z0-9._-]`. Leading and
  trailing whitespace is trimmed.

Anything else containing `#` is `invalid_argument`. The message gives both accepted forms and
quotes the input `{:?}`, capped at 200 characters.

New module `crates/of-core/src/ticket_ref.rs`:

```rust
pub enum JobRef { Id(JobId), Ticket(TicketQuery) }
pub struct TicketQuery { owner_repo: Option<String>, number: u64 }  // owner_repo lowercased
impl JobRef { pub fn parse(raw: &str) -> Result<JobRef> }
impl TicketQuery {
    /// The two bound parameters the shared SQL predicate takes.
    fn exact(&self) -> String;          // "#928" | "acme/api#928"
    fn suffix_like(&self) -> Option<String>; // Some("%/%#928") for the short form only
}
pub(crate) fn ticket_match_sql(exact: usize, like: usize) -> String
// "(lower(j.ticket_ref) = $exact OR ($like::text IS NOT NULL AND j.ticket_ref LIKE $like))"
```

The `LIKE` pattern is built only from validated digits, so it holds no caller wildcards. `%/%#928`
does not match `…#1928`, because the character before `928` has to be `#`.

### Matching

A short reference matches any job whose `ticketRef` ends in `/<anything>#N` or is exactly `#N`.
A full reference matches `ticketRef` case-insensitively and exactly. Matching is org-wide and
then narrowed by visibility (`job_visible_sql`). The tools take no repo argument, and adding one
to fifteen tools would be a wider interface change than the issue asks for. A short reference
that hits two repos is ambiguous, and the refusal tells the agent to pass the full form or an id
(see Assumptions).

### Resolution

`JobsExt::resolve_job_ref(&mut self, raw: &str, scope: &TeamScope) -> Result<JobId>`:

- `JobRef::Id` returns the id with no query, so existing not-found and visibility errors stay
  exactly where they are today (the `ensure_job_visible` / `visible_job` calls that follow).
- `JobRef::Ticket` runs one statement:
  `SELECT id FROM jobs j WHERE org_id = $1 AND <ticket_match_sql> AND <job_visible_sql> ORDER BY created_at DESC LIMIT 11`.
  - 0 rows: `Error::NoJobForTicket(String)`, code **`job_not_found`** (so an agent already branching
    on that code keeps working), with the message: *no job you can see has a ticketRef matching
    "#928"; list_jobs(ticket=…) shows what does*.
  - 1 row: that id.
  - 2+ rows: `Error::AmbiguousTicket { ticket, jobs: Vec<JobId> }`, new code
    **`ambiguous_ticket_ref`**, not retriable. The message lists up to 10 ids (the 11th row only
    tells us to say "and more"): *"#928" matches job-52, job-61; pass one of those ids, or the full
    owner/repo#928 form; list_jobs(ticket="#928") shows each one's status*. A common way to get
    here is an issue whose first job failed and was re-queued as a new job, and the hint is aimed
    at that case.

Ambiguity is decided after the visibility filter, so the listed ids are only ones the caller can
see. A hidden job never makes a reference ambiguous, and its existence never leaks.

`resolve_job_refs(&[String], scope) -> Result<Vec<JobId>>` resolves each in order and stops at the
first error.

### Where it applies

In `of-mcp`, a helper `tools::job_ref(tx, team, raw) -> Result<JobId, ErrorData>` (and
`job_refs`) replaces `JobId::from(args.job)` at every site, before the existing
`ensure_job_visible`/`visible_job` call:

- `jobs.rs`: `get_job`, `update_job`, `delete_job`, `claim_jobs` (`jobs`), `renew_claim`,
  `complete_job`, `fail_job`, `request_cancel`, `cancel_job`, `activate_job`, `repend_job`,
  `set_dependencies` (`job`, `add`, `remove`), `add_job` (`dependsOn`), `link_ticket`,
  `sync_ticket`.
- `coord.rs`: `send_message` (`job`) and `acquire_lease` (`job`).

The resolved id is what each tool passes on and echoes (`DependenciesOut.job`, `DeletedOut.deleted`,
the lease's `jobId`), so a caller who sent `#928` learns `job-52`. Each argument's schema
description gains: *A job id like "job-42", or the job's issue as "#928" or "owner/repo#928".*

`add_job`'s idempotency fingerprint hashes the **resolved** `depends_on` ids. A replay that spells
a dependency as `#925` the first time and `job-49` the second still replays, because both resolve
to the same id. Resolution happens before `find_replayed_job`, and both branches of `add_job`
already build `depends_on` there.

`complete_job` resolves inside the transaction after `charge_completion`, as the id does today.
Resolution is a read, so it changes nothing about billing.

## §2 `list_jobs(ticket=…)`

`ListJobsArgs.ticket: Option<String>` (blank means absent). It parses with `JobRef::parse`, and a
`JobRef::Id` (no `#`) is `invalid_argument`: *ticket takes an issue reference like "#928" or
"owner/repo#928"*. `JobFilter.ticket: Option<TicketQuery>` adds the same `ticket_match_sql`
predicate. It returns every matching visible job, newest first. This is the tool to call when a
reference was ambiguous.

## §3 `jobTicketRef` on leases

`Lease` gains `#[sqlx(default)] pub job_ticket_ref: Option<String>`, serialized `jobTicketRef`.
`list_leases` selects it with
`LEFT JOIN jobs j ON j.org_id = l.org_id AND j.id = l.job_id` (both tables tenant-scoped, the join
predicate carries `org_id`). Every other lease statement omits the column and gets `None`, which
`#[sqlx(default)]` allows. `hide_unseen_jobs` blanks `job_ticket_ref` together with `job_id`, so a
hidden job's issue is withheld exactly as its id is.

## §4 "Blocked by" in `sync_ticket`

### Parsing (pure, `of-trackers`)

`of_trackers::sync::blocked_by(body: &str) -> Vec<String>` returns issue references in order of
first appearance, deduplicated:

- It looks at lines outside fenced code blocks (```` ``` ```` / `~~~`), case-insensitively, for
  the phrase `blocked by`. Leading list markers, `>` and `*`/`_` emphasis need no special
  handling, because the phrase is searched for anywhere in the line.
- From the text after the phrase, to the end of the line, it takes every token matching the
  §1 grammar (`#N` or `OWNER/REPO#N`). Any separators are allowed ("#1, #2 and acme/x#3").
- At most 20 references are kept per body, to bound the work a hostile issue can cause.

### Resolution and write

`sync_ticket` for a job with `tracker = github`:

1. Resolve the binding as today. With no binding there is nothing to read with, and the existing
   `NotConfigured`/`Broken` errors stand.
2. Fetch the issue: a new `GithubAppClient::get_issue(installation, owner, repo, n) -> IssueView
   { body: Option<String>, updated_at: Option<String> }`, the same `GET /repos/{o}/{r}/issues/{n}`
   that `get_issue_updated_at` already makes. A fetch failure is `tracker_sync_failed`
   (retriable), as for the write-back.
3. In one transaction: record drift (§5), then for each `blocked_by` reference, qualify a short
   one with the job's own `owner/repo` (from its `ticketRef`). Then
   `JobsExt::add_ticket_dependencies(job, &[TicketQuery], scope) -> Vec<BlockerOutcome>`, which
   per reference:
   - holds the job's repo while it checks the job is visible (`get_job_visible_held`), because this
     is a write and `sync_ticket`'s first read has already committed;
   - resolves with exactly §1's visible-matching rules, excluding the job itself, and re-checks a
     unique candidate with `get_job_visible_held` (a failure is `no_job`);
   - `none` → outcome `no_job` (job left alone, as the AC asks);
   - 2+ → `ambiguous` with the visible ids;
   - already a dependency → `already`;
   - would close a cycle (the job is reachable from the candidate through `job_dependencies`, or
     the candidate *is* the job) → `cycle`, nothing written. This is checked **before** the insert,
     so one bad line never aborts the transaction or the other lines;
   - otherwise insert → `added`.
4. If the job is **pending**: commit and return, with no outbound write-back (there is still
   nothing to post). For a GitHub job this replaces today's `invalid_argument` refusal. The
   refusal now runs after the binding lookup, so a pending job with no binding gets
   `NotConfigured` first. A pending **JIRA** job keeps the `invalid_argument` refusal, because
   there is no read path for JIRA here and nothing else to do.
5. Otherwise: the existing write-back, unchanged.
6. Record the call (`charge`) as today: in its own short `charge_tx`, opened after everything
   else has committed (`tools/jobs.rs:1600-1602`), on **every** success path, the new pending
   path included. It is never the first statement of the dependency transaction. Charging there
   would roll a recorded call back with a failed tracker call that came after it, and that is the
   reason the existing write-back paths already charge last.

The dependency write happens before the outbound call. A tracker failure after it leaves the
dependencies in place, and a retry is idempotent (`already`).

### Output

`sync_ticket` returns `SyncTicketOut { job: Job, blockers: Vec<BlockerOutcome> }`, where
`BlockerOutcome { ticketRef: String, outcome: "added"|"already"|"no_job"|"ambiguous"|"cycle", jobs: Vec<JobId> }`
(`jobs`: the one id for added/already/cycle, the candidates for ambiguous, empty for no_job).
Adding a field to the result object is additive: `out::DependenciesOut` already has two.
`blockers` is empty for a JIRA job.

The description gains: it also reads "Blocked by #N" lines from a GitHub issue's body and adds a
dependency on #N's job when exactly one job has that issue. It reports each line's outcome in
`blockers`, removes nothing, and works on a pending job (read only).

## §5 Drift marker

### Schema

Migration `0009_job_ticket_drift.sql` (schema only, no data rewrite):

```sql
ALTER TABLE jobs
  ADD COLUMN ticket_changed_at timestamptz,  -- newest foreign change to the issue seen
  ADD COLUMN described_at      timestamptz;  -- title/description last written; NULL = created_at
```

`jobs` is already a tenant table with its policy. Adding columns needs no new registration.

### Read

`JOB_COLS` gains `ticket_changed_at` and the computed
`(ticket_changed_at IS NOT NULL AND ticket_changed_at > COALESCE(described_at, created_at)) AS ticket_stale`.
`Job` gains `ticket_changed_at: Option<DateTime<Utc>>` (`ticketChangedAt`) and `ticket_stale: bool`
(`ticketStale`), documented for the LLM reader: *true when the linked issue has had a comment or
change since this job's title or description were last written. Re-read the issue before relying
on the description, and call update_job to bring it up to date (which clears this).*
`described_at` is not exposed. Every tool returning a `Job` carries both fields (`get_job`,
`ready`, `list_jobs`, `claim_jobs`, …).

### Write

- `JobsExt::note_ticket_changed(id, at)`:
  `UPDATE jobs SET ticket_changed_at = GREATEST(COALESCE(ticket_changed_at, $3), $3) WHERE org_id = $1 AND id = $2`.
- `update_job` sets `described_at = now()` when `title` or `description` is passed. That clears
  `ticketStale` unless a newer change arrives.
- `update_from_ticket` (webhook body edit) sets `described_at = now()` as well, because the
  description now is the issue body.

### Signals

1. **Webhook comments (GitHub).** `CommentSnapshot` gains `author_is_bot: bool` (GitHub
   `comment.user.type == "Bot"`, defaulting to false when absent). `inbound_decision`, for an
   existing non-terminal job and `kind == IssueComment` on GitHub, returns a new
   `InboundDecision::Changed { remote_revision }` when the author is not a bot, or `NoOp` when it
   is. This replaces the `Update` that a comment `edited` action produced and the `NoOp` for
   `created`/`deleted`. The comment event's issue snapshot carries the same body, so the `Update`
   rewrote nothing new. The existing echo check (`is_stale_or_echoed`) runs first, as now. The
   webhook route applies `Changed` with
   `note_ticket_changed(existing.id, remote_revision parsed)` and
   `set_remote_revision`. The bot filter is what keeps otto-tower's own write-back comment from
   flagging its own job: the webhook for that comment can arrive before the write-back has stored
   its `remote_revision`.
2. **`sync_ticket` pull (GitHub).** The fetched `updated_at`, when it is later than the job's
   stored `remote_revision` (or there is none), is a change otto-tower did not make, so
   `note_ticket_changed(updated_at)`. The SQL comparison with `described_at`/`created_at` decides
   whether it counts as stale. A job created after the issue's last change stays clean.

An `issues` `edited` event keeps its existing `Update` path. That path sets `described_at = now()`
and never touches `ticket_changed_at`, so an earlier comment's mark is cleared only when it is
older than the rewrite.

### What the marker cannot see

Both limits are inherited from the existing tracker wiring. Neither is new here, and tests pin
both down:

- **A repo with no active GitHub binding gets no signal at all.** There is no webhook, and
  `sync_ticket` refuses with `NotConfigured` because it has no installation to read the issue
  with. The motivating job-52 was in that situation. It is covered once its repo is bound, and
  not before.
- **An issue without the binding's trigger label gets no webhook signal.** `inbound_decision`
  drops such events before anything else (`sync.rs:62`), so a job linked by `link_ticket` to an
  unlabelled issue is refreshed only by calling `sync_ticket`.

## Error handling & edge cases

- Malformed reference (`#`, `#abc`, `a#1`, `a/b/c#1`): `invalid_argument`, not retriable, both forms
  named.
- A reference to a job in another org: indistinguishable from none (`job_not_found`). Guard 1 is the
  `org_id = $1` predicate, and RLS is guard 2.
- A reference to a hidden team's job: none (`job_not_found`), and it is excluded from ambiguity lists.
- `claim_jobs(["#928", "job-52"])` naming the same job twice: resolves to a duplicate id, which
  `claim_jobs` already sorts and dedupes (`crates/of-core/src/jobs.rs:1176-1178`). One claim results.
- `set_dependencies(job="#928", add=["#928"])`: both resolve to one id, giving the existing
  self-dependency `DependencyCycle`.
- A reopened issue with a completed and a live job: a short or full reference is ambiguous and the
  refusal lists both. "Blocked by" reports `ambiguous`. The caller picks by id.
- "Blocked by" naming the job's own issue: `cycle`.
- An issue body with no "Blocked by": `blockers: []`, and the sync proceeds as before.
- An echoed or old comment webhook (`incoming <= remote_revision`): dropped as today, before any
  `Changed`.
- An unparseable `updated_at`: no drift recorded (there is no time to compare), and the sync
  proceeds.

## Testing

- `of-core` `tests/jobs.rs`: `JobRef::parse` table (valid/invalid), resolve short/full/case,
  0/1/2+, a cross-org negative (an org B job with the same `ticketRef` never resolves for org A
  and never appears in A's ambiguity list), a team-hidden negative, the `list_jobs` ticket filter,
  `note_ticket_changed` + `update_job` clearing staleness, change-before-creation not stale,
  `add_ticket_dependencies` outcomes including `cycle` leaving the transaction usable, and lease
  `job_ticket_ref` blanked by `hide_unseen_jobs`.
- `of-core` `tests/isolation.rs`: a `rls_scopes_*` test that the `list_leases` join cannot pair a
  lease with another org's job of the same id (`job-1` exists in both orgs).
- `of-trackers` unit: `blocked_by` (fences, separators, dedupe, cap, case, full refs),
  `inbound_decision` → `Changed` for a human comment, `NoOp` for a bot, `Drop` for an echo, and
  `parse_github` reading `user.type`.
- `of-mcp` `tests/tools.rs`: `get_job("#N")`, an ambiguous refusal listing ids, `set_dependencies`
  with references, `list_jobs(ticket)`, `list_leases` `jobTicketRef`, `sync_ticket` on a pending
  job with a "Blocked by" line against a mocked GitHub (`GithubAppClient` with a test
  `api_base`, as `of-trackers`' own tests do; see Risks), and `ticketStale` in `ready`.
- `of-web` `tests/webhooks_http.rs`: a signed human `issue_comment` webhook flips `ticketStale`, and
  a bot one does not, and neither does a human one on an issue without the trigger label (§5 limits).

## Assumptions

- **Org-wide matching for `#N`.** The issue says "within the repo", but no job tool takes a repo.
  An agent with the short form gets its job when it is unique, and otherwise a list of ids to pick
  from. Adding `repo`/`remote` to fifteen tools is a bigger interface change than the gain.
- **`job_not_found` is reused for "no job for that issue"** (a new variant, same code), so callers'
  existing branches hold. Ambiguity gets a new code, because the right next step differs.
- **Comment staleness only from non-bot authors.** CI bots and otto-tower's own App comment
  constantly, and treating those as scope changes would make the marker noise.
- **Edits keep their current behaviour** (the description is rewritten for bound repos). The
  marker covers what that path misses: comments by webhook on labelled issues, and any change
  `sync_ticket` sees on a bound repo (labelled or not). Unbound repos stay uncovered (§5).
- **`sync_ticket` on a pending job is now allowed** (read only). Before, it was an error with
  nothing to retry, so no correct caller depends on it.
- **The full form matches the stored spelling, not the repo's current GitHub name.** A renamed
  repo's old `ticketRef`s still match the short form.

## Risks & open questions

- **Mocking GitHub from `of-mcp` tests.** `GithubAppClient::with_api_base` is private, and
  `of-mcp` builds its client from `TrackerSyncConfig` (`crates/of-mcp/src/server.rs:93`). The plan
  makes `with_api_base` public and adds `TrackerSyncConfig.github_api_base: Option<String>`, which
  `of-server` always sets to `None` (no env var) and tests set to a mock server. This touches a
  struct `of-server` constructs, not the config surface. The JWT the client signs needs a real RSA
  key, and the tests generate one.
- **The short-form `LIKE` scans an org's ticketed jobs.** `jobs_org_ticket_idx` serves the exact
  branch. The suffix branch is a filtered scan over one org's rows with a non-null `ticket_ref`.
  That is fine at today's sizes, and a trigram or suffix index is a later concern.
- **`sync_ticket` pull can miss a change** that landed before otto-tower's own later write-back
  (that write-back's `remote_revision` covers it). The webhook path catches it for bound repos.
  Unbound jobs have no signal at all, which is already true.
