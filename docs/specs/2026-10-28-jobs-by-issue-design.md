# Jobs by issue design

> **Status:** IMPLEMENTED (PR #260, 2026-10-28; merged as 5449924). Agents can name a job by its issue wherever
> they can name it by id: `#928` or `owner/repo#928` on reads, `owner/repo#928` on tools that change
> a job. `list_jobs` gains a `ticket` filter, `list_leases` shows each lease's `jobTicketRef`,
> `sync_ticket(importBlockers: true)` turns "Blocked by #N" into a dependency, and a job whose
> issue has drifted carries `ticketStale`. Additive only: no tool, field or route is renamed or
> removed. Closes savvagent/otto-factory#257 and savvagent/otto-factory#258.

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
   actually lost is **comments**: `inbound_decision` maps an `issue_comment` with action `created`
   to a `NoOp`, and (found during implementation) the webhook route skipped every comment event
   before calling it at all, so no comment ever reached a job. This change lets GitHub comment
   events through, and a comment on an issue with no job is a `NoOp`, never a `Create`. JIRA
   comment events are still skipped. The job
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

- §1 Issue references accepted in place of a job id by every MCP tool argument that takes one:
  both forms on reads, the full form on writes (short `#N` only where it is unambiguous by
  construction, see §1 "Where it applies").
- §2 A `ticket` filter on `list_jobs`.
- §3 `jobTicketRef` on each lease `list_leases` returns.
- §4 `sync_ticket` reads the GitHub issue and, only when called with `importBlockers: true`,
  adds a dependency for each "Blocked by" reference in the issue's own repository that resolves to
  exactly one job. It now also accepts a pending GitHub job (read only, no write-back). A pending
  JIRA job is still refused. Every GitHub request for a job is limited to the repository its
  repo's binding names.
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
- Any console UI (`web/`) change. The console REST API does gain fields, additively: every `Job`
  it returns carries `ticketChangedAt` and `ticketStale`, and every lease carries `jobTicketRef`
  (they are the same `of-core` types the MCP tools return). The console renders jobs from that JSON
  and ignores fields it does not know, so the UI is unchanged.

**Constraint check.** (1) Repo anchoring: references resolve to jobs, and jobs still belong to
repos. Nothing creates a job or lease without one, and every GitHub call is confined to the
repository a repo's binding names. (2) Substrate: the server records that an issue changed. It
never decides what to do about it, and never rewrites a description. The one interpretation of
issue text, "Blocked by", is opt-in (`importBlockers: true`), and that is what keeps it on the
substrate side of the line, for three reasons together. The issue asks for it by name. It maps
onto a primitive the server already owns (`job_dependencies`) and adds no state of its own, so
what it writes is exactly what `set_dependencies(job="#928", add=["#925"])` would. And it never
runs unless the caller asks, so a skill that wants different rules (other phrases, JIRA links,
removing stale dependencies) reads the issue itself and calls `set_dependencies`; the server's
import is a convenience next to that path, never a policy in front of it. (3) Agent-agnostic:
plain tool arguments and JSON fields, nothing client-specific.

## Goal & success criteria

An agent that thinks in issues can do everything it does with job ids without listing the
queue, and can see when the issue has moved on since the job was written.

- Every tool argument that takes a job id also takes `owner/repo#N`, and the read tools also take
  `#N` (test per tool family, and a refusal test of `#N` on every write tool).
- An ambiguous reference never resolves. The refusal lists only job ids the caller may see.
- A reference never resolves to another org's job, or to a job of a team the caller is not on
  (cross-org and team negative tests).
- A "Blocked by #N" line in a bound GitHub issue becomes a dependency on
  `sync_ticket(importBlockers: true)`, and an unresolvable one is reported, not guessed.
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

The short form matches that number in every repo, so on a tool that changes a job it can land
on the wrong one: `complete_job("#12")` would complete, and bill, `acme/web#12` whenever
`acme/api#12` has no job yet. Reads keep it, because a wrong guess there costs nothing and an
ambiguous one says so. Writes take a job id or the full form (revised in review).

In `of-mcp`, a helper replaces `JobId::from(args.job)` at every site, before the existing
`ensure_job_visible`/`visible_job` call:

- **`tools::job_ref`** (both forms, `JobsExt::resolve_job_ref`): `get_job`, and
  `set_dependencies`' own `job`. The latter keeps `#N` because the acceptance criterion
  `set_dependencies(job="#928", add=["#925"])` names it, and resolution is unique-or-refused, so it
  never guesses between repos.
- **`tools::job_ref_for_write`** / `job_refs_for_write` (`JobsExt::resolve_job_ref_for_write`):
  `update_job`, `delete_job`, `claim_jobs` (`jobs`), `renew_claim`, `complete_job`, `fail_job`,
  `request_cancel`, `cancel_job`, `activate_job`, `repend_job`, `link_ticket`, `sync_ticket`, and in
  `coord.rs` `send_message` (`job`) and `acquire_lease` (`job`). A short `#N` is `invalid_argument`
  before any lookup: *"#N" can match an issue in any repo; on a tool that changes a job, name it by
  id or as owner/repo#N (get_job("#N") or list_jobs(ticket="#N") shows which job it is)*.
- **`tools::dependency_refs`** (`JobsExt::resolve_dependency_refs`): `set_dependencies`' `add` and
  `remove`, and `add_job`'s `dependsOn`. A short `#N` here is read relative to the dependent job's
  own repository, as GitHub reads `#N` inside that repository's issues, so it cannot pick up
  another repo's `#N`. For `set_dependencies` the home repository is the `owner/repo` of the
  resolved job's `ticketRef` (`ticket_ref::split_full`); for `add_job` it is the `external_ref` of
  the target repo's GitHub binding (`trackers::resolve_binding`), used rather than the repo's
  remotes because a repo can have several remotes but only one binding per provider. With no home
  (no full `ticketRef`, no GitHub binding) a short dependency is refused, naming why and asking for
  the full form. Job ids and full references resolve exactly as elsewhere.

The resolved id is what each tool passes on and echoes (`DependenciesOut.job`, `DeletedOut.deleted`,
the lease's `jobId`), so a caller who sent `acme/api#928` learns `job-52`. The write arguments'
schema descriptions say *A job id like "job-42", or the job's issue as "owner/repo#928"*; the
read arguments' and dependency arguments' say which short form they take and what it means.

`add_job`'s idempotency fingerprint hashes the **resolved** `depends_on` ids. A replay that spells
a dependency as `acme/api#925` the first time and `job-49` the second still replays, because both
resolve to the same id. Resolution happens before `find_replayed_job`, and both branches of `add_job`
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

`sync_ticket` takes its own `SyncTicketArgs { job, importBlockers: bool }` (`importBlockers`
defaults to false; the other job tools keep `JobArgs`). If `importBlockers` is true and the token
lacks `jobs:write`, the call is refused with the same `insufficient_scope` error
`require_scope(jobs:write)` gives, before anything is read: an explicit ask fails closed rather
than being quietly skipped. For a job with `tracker = github`:

1. Resolve the binding as today. With no binding there is nothing to read with, and the existing
   `NotConfigured`/`Broken` errors stand.
2. **Check the binding.** The `ticketRef`'s `owner/repo` must equal the binding's `external_ref`,
   compared case-insensitively. Every request is sent with the binding's installation token, which
   reaches every repository the App is installed on, so a `ticketRef` typed into `link_ticket` (or
   left behind by a rename) must not choose where that token is used. A mismatch is
   `invalid_argument`, before any token is minted: *job X's ticketRef names owner/repo, but this
   repo's GitHub binding is A/B; relink it with link_ticket using the bound repo's name*. The
   automatic write-back after a transition (`sync_job_after_transition`) applies the same check
   and skips a mismatch with a `warn` log naming only the job id.
3. Fetch the issue: a new `GithubAppClient::get_issue(installation, owner, repo, n) -> IssueView
   { body: Option<String>, updated_at: Option<String> }`, the same `GET /repos/{o}/{r}/issues/{n}`
   that `get_issue_updated_at` already makes. A fetch failure is `tracker_sync_failed`, classified
   as in "Retriability" below.
4. In one transaction: lock the job's row (`JobsExt::lock_job_visible`, `FOR NO KEY UPDATE`, so two syncs of one job queue rather than deadlock, plus the repo
   hold) and compare its `tracker`/`ticketRef` with the ones the issue was fetched for. The first
   read committed before the fetch, so `link_ticket` may have relinked the job in between; a
   relinked job is refused, retriable, rather than given another issue's blockers and drift time
   (added in review). Then record drift (§5) against the `remote_revision` read under the lock,
   whether or not blockers were asked for. Then, only with `importBlockers: true`, for each
   `blocked_by` reference qualify a short one with the job's own `owner/repo` (from its
   `ticketRef`), and call
   `JobsExt::add_ticket_dependencies(job, home, &[TicketQuery], scope) -> Vec<BlockerOutcome>`,
   which first takes the org's dependency lock (see Risks) and then per reference:
   - a reference whose `owner/repo` is not `home` (the job's own issue's repository, compared
     case-insensitively) → `other_repo`, nothing looked up or written (added in review). The body
     is written by whoever can edit the issue, and a line naming any repository would let them
     make this job wait on any job in the org;
   - holds the job's repo while it checks the job is visible (`get_job_visible_held`), because this
     is a write and `sync_ticket`'s first read has already committed;
   - resolves with exactly §1's visible-matching rules (the job itself included, so naming its own
     issue is reported as `cycle`, not hidden), and re-checks a
     unique candidate with `get_job_visible_held` (a failure is `no_job`);
   - `none` → outcome `no_job` (job left alone, as the AC asks);
   - 2+ → `ambiguous` with the visible ids;
   - already a dependency → `already`;
   - would close a cycle (the job is reachable from the candidate through `job_dependencies`, or
     the candidate *is* the job) → `cycle`, nothing written. This is checked **before** the insert,
     so one bad line never aborts the transaction or the other lines;
   - otherwise insert → `added`.
5. If the job is **pending**: commit and return, with no outbound write-back (there is still
   nothing to post). For a GitHub job this replaces today's `invalid_argument` refusal. The
   refusal now runs after the binding lookup, so a pending job with no binding gets
   `NotConfigured` first. A pending **JIRA** job keeps the `invalid_argument` refusal, because
   there is no read path for JIRA here and nothing else to do.
6. Otherwise: the existing write-back, with its errors now classified like the read's.
7. Record the call (`charge`) as today: in its own short `charge_tx`, opened after everything
   else has committed (`tools/jobs.rs:1600-1602`), on **every** success path, the new pending
   path included. It is never the first statement of the dependency transaction. Charging there
   would roll a recorded call back with a failed tracker call that came after it, and that is the
   reason the existing write-back paths already charge last.

The dependency write happens before the outbound call. A tracker failure after it leaves the
dependencies in place, and a retry is idempotent (`already`). The blocker ids returned are
re-checked against the caller's scope in the same final transaction that re-reads the job, so a
blocker whose repo moved out of the caller's teams during the tracker round trip is no longer named
(its outcome stays; its id is dropped from `jobs`).

### Retriability

Every GitHub failure in `sync_ticket`, the read and the write-back alike, goes through one
classification (`github_retriable`); `sync_github_job` returns `of_trackers::Error` rather than a
string so the HTTP status survives to it:

- **Retriable:** a transport failure, a garbled response, a 5xx, a 429, and a 403 whose body says
  it is a rate limit (case-insensitive "rate limit"; GitHub answers both its primary and secondary
  limits with a 403 as often as a 429).
- **Not retriable:** any other 4xx (401, a plain 403, 404, 422: the App cannot see the issue, or it
  does not exist), and anything refused before a request was made: missing App credentials, an
  unusable installation id, a malformed `ticketRef`.

For JIRA, a missing client id, client secret or encryption key is now `retriable: false` too, like
the GitHub App's: it is server configuration and fails the same way until an operator fixes it.
Other JIRA write-back failures stay retriable, as before. The automatic write-back after a
transition still only logs its failures.

### Behaviour changes

Additive in shape, but four existing behaviours change: a GitHub write-back failure in
`sync_ticket` that GitHub will answer the same way again (a plain 403, a 404, a 422) is now
`retriable: false` where it was `true`; a missing JIRA client id, secret or encryption key is now
`retriable: false` where it was `true`; a `sync_ticket` or automatic write-back on a job whose
`ticketRef` names a repository other than the bound one is now refused (or skipped) where it was
sent; and a short `#N` on a tool that changes a job (new in this change) is refused.

### Output

`sync_ticket` returns `SyncTicketOut { job: Job, blockers: Vec<BlockerOutcome>, blockersImported: bool }`
(`blockersImported` is true only when the import ran: false without `importBlockers: true` and
for a JIRA job, so an empty `blockers` is never mistaken for "no Blocked by lines"; added in
review), where
`BlockerOutcome { ticketRef: String, outcome: "added"|"already"|"no_job"|"ambiguous"|"cycle"|"other_repo", jobs: Vec<JobId> }`
(`jobs`: the one id for added/already/cycle, the candidates for ambiguous, empty for no_job and
other_repo, and in every case only ids the caller can still see).
Adding a field to the result object is additive: `out::DependenciesOut` already has two.
`blockers` is empty for a JIRA job.

The description gains: it reads a GitHub issue first and records drift; with
`importBlockers: true` (and `jobs:write`) it also reads "Blocked by #N" lines from the body and adds
a dependency on #N's job when exactly one job has that issue in the same repository. It reports
each line's outcome in `blockers`, removes nothing, refuses a `ticketRef` outside the bound
repository, says which failures are worth retrying, and works on a pending GitHub job (read only).
A pending JIRA job is still refused.

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
- `update_from_ticket` (webhook body edit) does **not** touch `described_at` (revised in
  review). The body edit is absorbed into the description, but a comment noted before it is not,
  and clearing on the edit would hide an unread comment behind something as small as a label
  change. Only a caller saying it has re-read the issue (`update_job` with a title or description)
  clears the mark.

### Signals

1. **Webhook comments (GitHub).** `CommentSnapshot` gains `author_is_bot: bool` (GitHub
   `comment.user.type == "Bot"`, defaulting to false when absent). `inbound_decision`, for an
   existing non-terminal job and `kind == IssueComment` on GitHub, returns a new
   `InboundDecision::Changed { remote_revision }` when the author is not a bot, or `NoOp` when it
   is. This replaces the `Update` that a comment `edited` action produced and the `NoOp` for
   `created`/`deleted`. The comment event's issue snapshot carries the same body, so the `Update`
   rewrote nothing new. The existing echo check (`is_stale_or_echoed`) runs first, as now. The
   webhook route applies `Changed` with `note_ticket_changed(existing.id, at)` and does **not**
   advance `remote_revision` (revised in review). Advancing it would make `is_stale_or_echoed` drop
   an earlier issue edit that GitHub delivers after the comment, losing that edit, and a
   redelivered comment needs no echo guard because `note_ticket_changed` is idempotent
   (`GREATEST`). `Changed` carries the parsed `DateTime<Utc>`, so the route does no re-parse. The
   bot filter is what keeps otto-tower's own write-back comment from
   flagging its own job: the webhook for that comment can arrive before the write-back has stored
   its `remote_revision`.
2. **`sync_ticket` pull (GitHub).** The fetched `updated_at`, when it is later than the job's
   stored `remote_revision` (or there is none), is a change otto-tower did not make, so
   `note_ticket_changed(updated_at)`. The SQL comparison with `described_at`/`created_at` decides
   whether it counts as stale. A job created after the issue's last change stays clean.

An `issues` `edited` event keeps its existing `Update` path, which rewrites the description and
leaves both `ticket_changed_at` and `described_at` alone, so an earlier comment's mark survives it.

GitHub's timestamps and the database's `now()` come from different clocks. A comment made a
second or so after an `update_job` can land on the wrong side of the comparison. That is
accepted: the marker is a hint to re-read the issue, not a guarantee.

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
- A short `#N` on a tool that changes a job: `invalid_argument`, before any lookup, naming
  `owner/repo#N` and the read tools that show which job `#N` is.
- A short `#N` dependency with no home repository (a job with no full `ticketRef`, an `add_job`
  repo with no GitHub binding): `invalid_argument`, asking for the full form.
- A `ticketRef` outside the bound repository: `sync_ticket` refuses (`invalid_argument`, nothing
  sent); the automatic write-back skips it.
- `claim_jobs(["acme/api#928", "job-52"])` naming the same job twice: resolves to a duplicate id, which
  `claim_jobs` already sorts and dedupes (`crates/of-core/src/jobs.rs:1176-1178`). One claim results.
- `set_dependencies(job="#928", add=["#928"])`: both resolve to one id, giving the existing
  self-dependency `DependencyCycle`.
- A reopened issue with a completed and a live job: a short or full reference is ambiguous and the
  refusal lists both. "Blocked by" reports `ambiguous`. The caller picks by id.
- "Blocked by" naming the job's own issue: `cycle`.
- "Blocked by" naming an issue in another repository: `other_repo`, never looked up.
- `importBlockers: true` without `jobs:write`: `insufficient_scope`, nothing read.
- The job relinked while `sync_ticket` was fetching its issue: refused, retriable, nothing
  recorded.
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
- `of-trackers` unit: `blocked_by` (fences, including a shorter fence inside a longer one and a
  fence line with an info string, separators, dedupe, cap, case, full refs),
  `inbound_decision` → `Changed` for a human comment, `NoOp` for a bot, `Drop` for an echo, and
  `parse_github` reading `user.type`.
- `of-core` `tests/jobs.rs` (review): `other_repo`, the short-on-write refusal, short dependencies
  read in the home repo, `lock_job_visible` making a relink wait, and two connections proving the
  dependency lock: one adds A→B and holds, the other's B→A waits, then reports a cycle.
- `of-mcp` `tests/tools.rs`: `get_job("#N")`, an ambiguous refusal listing ids, `#N` refused on
  every write tool, `set_dependencies` with references (short dependencies in the job's own repo),
  `add_job` short dependencies against a binding and refused without one, a `ticketRef` outside the
  bound repo refused with no request sent and skipped by the automatic write-back, `other_repo`,
  `importBlockers` off by default and refused without `jobs:write`, rate-limited 403s retriable on
  the read and the write-back, `list_jobs(ticket)`, `list_leases` `jobTicketRef`, `sync_ticket` on a pending
  job with a "Blocked by" line against a mocked GitHub (`GithubAppClient` with a test
  `api_base`, as `of-trackers`' own tests do; see Risks), and `ticketStale` in `ready`.
- `of-web` `tests/webhooks_http.rs`: a signed human `issue_comment` webhook flips `ticketStale`, and
  a bot one does not, and neither does a human one on an issue without the trigger label (§5 limits).

## Assumptions

- **Org-wide matching for `#N`, on reads only.** The issue says "within the repo", but no job tool
  takes a repo. On a read (`get_job`, `list_jobs.ticket`) an agent with the short form gets its job
  when it is unique, and otherwise a list of ids to pick from. On a write a unique match is not
  enough, because it may be the wrong repo's job that happens to be the only one, so writes take an
  id or `owner/repo#N` (revised in review). The exceptions are the ones where "the repo" is
  defined: `set_dependencies`' own `job` keeps the org-wide unique match the acceptance criterion
  names, and a short dependency (`add`/`remove`, `dependsOn`) is read in the dependent job's own
  repository. Adding `repo`/`remote` to fifteen tools is a bigger interface change than the gain.
- **`job_not_found` is reused for "no job for that issue"** (a new variant, same code), so callers'
  existing branches hold. Ambiguity gets a new code, because the right next step differs.
- **Comment staleness only from non-bot authors.** CI bots and otto-tower's own App comment
  constantly, and treating those as scope changes would make the marker noise.
- **Edits keep their current behaviour** (the description is rewritten for bound repos). The
  marker covers what that path misses: comments by webhook on labelled issues, and any change
  `sync_ticket` sees on a bound repo (labelled or not). Unbound repos stay uncovered (§5).
- **`sync_ticket` on a pending GitHub job is now allowed** (read only; a pending JIRA job is
  still refused). Before, it was an error with
  nothing to retry, so no correct caller depends on it.
- **The full form matches the stored spelling, not the repo's current GitHub name.** A renamed
  repo's old `ticketRef`s still match the short form on reads.
- **Stored `ticketRef`s are matched as stored, apart from case.** Leading zeros are dropped from
  the reference a caller passes, not from what is stored, so a `ticketRef` typed into `add_job` or
  `link_ticket` as `acme/api#0928` is not found by `#928`. Nothing that generates references
  writes one (GitHub numbers have no leading zeros, and webhook-created jobs store GitHub's own
  `full_name#number`), and normalizing stored data would need a per-org data migration for a
  spelling no producer emits.

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
- **A late comment webhook can miss the mark.** The existing echo check (`is_stale_or_echoed`)
  runs before `Changed`. So a comment whose event arrives after a newer revision was stored (an
  issue edit, or otto-tower's own write-back) is dropped, and so is one in the same second
  (`<=`). This is the weaker failure, a missed hint rather than a lost edit, and the next
  `sync_ticket` pull still catches any change after the stored revision. Closed by
  savvagent/otto-factory#259 (PR #263): `Changed` now has a guard of its own against
  `ticket_changed_at`; see `docs/specs/2026-10-28-late-comment-stale-mark-design.md`.
- **"Blocked by" lines are untrusted input.** They come from the issue body, which anyone who can
  edit the issue controls, not only the caller. What they can do is bounded: nothing happens unless
  the caller asks (`importBlockers: true`, which needs `jobs:write` on top of the `trackers` that
  `sync_ticket` needs); a line can only make this job wait on a job the caller can see in the
  same repository as the issue (`other_repo` otherwise); and nothing is ever removed. An unwanted
  dependency is undone with `set_dependencies`.
- **A renamed repository's old `ticketRef`s need relinking before GitHub is called for them.** The
  binding check compares a job's `ticketRef` with the binding's `external_ref`, so after a rename
  (this org's `savvagent/otto-factory#…` refs against a binding for `savvagent/otto-tower`)
  `sync_ticket` refuses with the relink instruction, and the automatic write-back skips those jobs
  until `link_ticket` gives them the bound name. That is the price of never letting a stored
  reference choose which repository the installation token is used on.
- **Concurrent dependency writes could close a loop; closed here (savvagent/otto-factory#258).**
  The cycle checks run under READ COMMITTED, so without serialization two concurrent writes could
  each see no path and each insert half of a loop. `set_dependencies` and
  `add_ticket_dependencies` now both start with
  `pg_advisory_xact_lock(hashtextextended('job_dependencies:' || org_id, 0))`, so dependency writes
  in one org run one at a time and the second sees the first's edge. Per org, so orgs never wait on
  each other. `add_job` reaches it through `set_dependencies` when it has dependencies.

## Follow-ups

- A guard of its own for `Changed` (comparing a late comment against `ticket_changed_at` rather
  than `remote_revision`), so a comment webhook delivered after a newer revision still marks the
  job (see Risks). Done: savvagent/otto-factory#259, PR #263.
- savvagent/otto-factory#258 (concurrent dependency writes) is fixed in this change, not a
  follow-up.
