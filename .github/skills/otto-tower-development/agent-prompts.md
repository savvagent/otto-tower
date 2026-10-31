# otto-tower-development — Agent Dispatch Prompt Templates

Verbatim prompt bodies for every `Agent`-tool dispatch in `SKILL.md`. The skill spine owns the
*decision* logic (when to dispatch, which `model:` / `subagent_type`, status handling, fix-loop caps,
convergence rules); this file owns the prompt *text* you paste.

**Use the template exactly. Do not improvise a dispatch prompt body.** Fill the `<...>` placeholders
from your working memory (spec / plan / task text / report verbatim, plus the relevant Load-Bearing
Invariants and Repository Conventions from `SKILL.md`). The `subagent_type` is named in each
template's heading — honor it. If the runtime exposes only generic subagent types, paste the prompt
body into a `general` subagent unchanged; the reviewer's identity lives in the prompt, not in the
type string.

`<ref>` below is the source reference: a GitHub issue (`savvagent/otto-factory#123`) or — on the
ticketless path — the captured task brief.

---

## Spec Critique — Phase 1 Step 4 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Review spec document"
  prompt: |
    You are a spec document reviewer. Verify this spec is complete and ready for planning.

    Spec to review (full text inline; it is committed at docs/specs/<file>):
    <PASTE FULL SPEC TEXT>

    Repo profile — the invariants and conventions this spec must respect:
    <PASTE the "Load-Bearing Invariants" section and the relevant Repository Conventions rows from SKILL.md>

    Check:
    - Completeness: TODOs, placeholders, "TBD", incomplete sections
    - Consistency: internal contradictions, conflicting requirements
    - Clarity: requirements ambiguous enough to cause someone to build the wrong thing
    - Scope: focused enough for a single plan, with explicit non-goals
    - The three constraints in CLAUDE.md: coordination is anchored on repos; the server is a
      substrate and ships no workflow opinion; every coding agent is equally first-class. A
      capability that could live in a customer's own skill belongs in the skill, not the server
    - Tenant isolation: if a tenant table is added or touched, does the spec name the org_id column,
      the 0001_baseline.sql registration, the <table>_tenant_isolation policy, and the cross-org negative test?
    - Metering: if an MCP tool is added, does the spec name its tower-billing::classify classification?
    - Public-interface changes: is any non-additive change to the MCP tool surface, the console API,
      the OAuth/discovery endpoints, the config surface, or the schema named explicitly? (An applied
      migration must never be edited.)
    - YAGNI: unrequested features, over-engineering
    - Alignment with the source AC (cite ref <ref>)

    Only flag issues that would cause real problems during planning. Approve unless there are serious gaps.

    Output:
    ## Spec Review
    Status: Approved | Issues Found
    Issues: - [Section X]: [issue] - [why it matters]
    Recommendations (advisory): - [...]
```

---

## Plan Critique — Phase 2 Step 6 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Review plan document"
  prompt: |
    You are a plan document reviewer. Verify this plan is complete and ready for implementation.

    Plan to review (full text inline; it is committed at docs/plans/<file>):
    <PASTE FULL PLAN TEXT>

    Spec for reference (full text inline):
    <PASTE FULL SPEC TEXT>

    Repo profile — invariants, conventions, and commands:
    <PASTE the "Load-Bearing Invariants" section, the Repository Conventions table, and the test/lint commands from SKILL.md>

    Check completeness, spec alignment, task decomposition, buildability, and whether each task
    respects the repo-specific requirements above. In particular:
    - Does every task use the repo's real commands (cargo test -p of-<crate> --test <suite>,
      cargo clippy --all-targets -- -D warnings, cargo fmt --all, and for web/ npm run check /
      npm run lint / npm test)?
    - Does a task touching a tenant table or tenant-scoped function include a cross-org negative test step?
    - Does a task adding an MCP tool include a tower-billing::classify step and a tool description
      written for an LLM that has never read the docs?
    - Does any task put SQL outside tower-core?
    - Does a task adding a migration add a NEW file (never editing an applied one), keeping
      0001_baseline.sql last?
    - Are the out-of-band artifacts the task touches (container image, console bundle, Cloudflare
      Worker, migrations, .env.example) called out?

    Only flag issues that would cause an implementer to build the wrong thing or get stuck.

    Output:
    ## Plan Review
    Status: Approved | Issues Found
    Issues: - [Task X, Step Y]: [issue] - [why it matters]
    Recommendations (advisory): - [...]
```

---

## Implementer Dispatch — Phase 3 Step A — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Implement Task N: <name>"
  prompt: |
    You are implementing Task N: <name>

    ## Task Description
    <FULL TEXT of the task pasted inline — do not reference the plan file>

    ## Context
    <2-4 sentences: where this fits, dependencies on prior tasks, architectural notes, plus the
    repo-specific reminders for this task — the applicable Load-Bearing Invariants, the test/lint
    commands, and any out-of-band artifact the task touches>

    Tests need a real Postgres: `podman compose up -d` (Postgres 16 on host port 15433) and a `.env`
    with DATABASE_URL (`cp .env.example .env`). Every SQL statement lives in tower-core. Run
    `cargo fmt --all` before every Rust commit. Never add AI self-attribution to anything.

    ## AUTONOMOUS MODE — IMPORTANT

    You are running inside an autonomous pipeline. Do NOT ask clarifying questions.
    There is no developer available to answer mid-run.

    Instead:
    - When the task is ambiguous, pick the most reasonable interpretation given the
      surrounding code and the spec. Document the assumption in your report.
    - If the assumption is high-risk (could plausibly be wrong in a way the developer
      would care about), report DONE_WITH_CONCERNS and list the assumption explicitly.
    - Only return BLOCKED if you genuinely cannot proceed without information that
      cannot be reasonably inferred (e.g., a missing credential, an undocumented external
      contract). Do NOT return BLOCKED for stylistic ambiguity.

    ## Your Job
    1. Follow the task's TDD steps in order: failing test → run → implement → run → commit.
    2. Use exact file paths and commands from the task. Do not invent your own.
    3. Self-review before reporting (completeness, quality, YAGNI, testing).
    4. Commit per the task's step-by-step instructions, using the repo's `<scope>: <subject>` format.

    Work from: <worktree absolute path>

    ## Report Format
    - Status: DONE | DONE_WITH_CONCERNS | BLOCKED | NEEDS_CONTEXT
    - Files changed (with commit SHAs)
    - Test results (command + outcome)
    - Assumptions made (with one-line rationale each)
    - Concerns or blockers (if any)
```

---

## Spec Compliance Review — Phase 3 Step C — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Spec compliance: Task N"
  prompt: |
    You are reviewing whether an implementation matches its specification.

    ## What Was Requested
    <FULL TEXT of the task — same as the implementer received>

    ## What Implementer Claims They Built
    <implementer's report verbatim>

    ## CRITICAL: Do Not Trust The Report
    Read the actual code at the commit SHAs they listed. Verify line-by-line.

    Check:
    - Missing requirements (claimed implemented but actually skipped)
    - Extra work (built features not requested)
    - Misinterpretations (right feature, wrong way)
    - Repo-specific gotchas: SQL outside tower-core, a tenant table without an org_id / RLS policy /
      cross-org negative test, an MCP tool without a tower-billing::classify entry, an edited migration,
      a 403 where the product answers 404, a credential spent on a GET, an unwrap() outside tests,
      a test that spawns a Watcher without calling shutdown().

    Report:
    - ✅ Spec compliant
    - ❌ Issues found: [list with file:line refs]
```

---

## Code Quality Review — Phase 3 Step E — `subagent_type: code-reviewer`

Capture commit boundaries first (SKILL.md step E owns this):
`BASE_SHA = git rev-parse HEAD~<N>` (N = commits this task produced), `HEAD_SHA = git rev-parse HEAD`.

```
Agent tool:
  subagent_type: code-reviewer
  description: "Quality: Task N"
  prompt: |
    Review the code changes between <BASE_SHA> and <HEAD_SHA>.

    Plan/requirements: Task N (full text inline):
    <FULL TEXT of task>

    Check standard code-quality concerns plus:
    - One clear responsibility per file?
    - Units decomposed for independent testing?
    - Following the file structure from the plan?
    - Did this change create or grow files significantly beyond what the task required?
    - Repo conventions from CLAUDE.md: all SQL in tower-core; errors written for an LLM caller with a
      stable Error::code() and an honest retriable(); no unwrap() outside tests; no silent fallback
      on a resolution failure; comments that explain WHY, not what the next line does.

    Report: Strengths, Issues (Critical / Important / Minor), Assessment.
```

---

## Final Code Review — Phase 3 Step H — `subagent_type: code-reviewer`

```
Agent tool:
  subagent_type: code-reviewer
  description: "Final review: <slug>"
  prompt: |
    Final review of the complete implementation.

    Plan (committed at docs/plans/<file>, full text inline):
    <PASTE FULL PLAN TEXT>
    Spec (committed at docs/specs/<file>, full text inline):
    <PASTE FULL SPEC TEXT>
    Branch: <branch-name>
    Diff range: <merge-base-with-master>..HEAD

    Verify:
    - All plan tasks are implemented end-to-end
    - The implementation actually achieves the spec's success criteria
    - No dead code, leftover debug, or skipped tests
    - Test coverage is reasonable for what was built, and cross-org negative tests exist for every
      tenant-scoped function touched
    - Repo convention compliance (CLAUDE.md + the SKILL.md conventions table): out-of-band artifacts
      handled, commit/branch conventions followed, no AI self-attribution anywhere in the diff.

    Report: Strengths, Issues, Overall assessment (Ready to merge / Needs work).
```

---

## Mandatory Review Trio — Phase 4 Step 8

All three are dispatched in the same parallel batch on every PR, with no size-based carve-out
(Non-Negotiable Rules 4–5).

### Rust expert — `subagent_type: rust-pro`

```
Agent tool:
  subagent_type: rust-pro
  description: "Rust review: PR #<N>"
  prompt: |
    Review the Rust in PR #<N> of savvagent/otto-factory (`gh pr diff <N>`), for ref <ref>.

    Judge idiomatic Rust against this repo's conventions:
    - Ownership, lifetimes, and borrow discipline; no needless clones or Arc<Mutex<...>> where a
      value would do
    - Error handling: this workspace uses typed errors with a stable Error::code() and a retriable()
      hint, written for an LLM caller that has never read the docs. No unwrap()/expect() outside
      tests, no panics on untrusted input, no silent fallback on a resolution failure
    - Async: Send + Sync seams, no blocking work in an async task, no transaction held across a long
      poll or an outbound request
    - sqlx: statements confined to tower-core, explicit org_id predicates, correct use of FOR UPDATE,
      enum round-tripping, and #[sqlx::test] integration tests against a real Postgres (there are no
      database mocks, on purpose)
    - Tests: a tenant-scoped function needs a cross-org negative test; an RLS policy test must
      SET LOCAL ROLE otto_app explicitly or it passes against no policy at all; a test that spawns a
      Watcher must call shutdown() or it hangs at teardown

    Report: Strengths, Issues (Critical / Important / Minor), Assessment.
```

### Architect — `subagent_type: architect-reviewer`

```
Agent tool:
  subagent_type: architect-reviewer
  description: "Architecture review: PR #<N>"
  prompt: |
    Review PR #<N> of savvagent/otto-factory (`gh pr diff <N>`) for architectural consistency, for
    ref <ref>. Read CLAUDE.md at the repo root first — it is the conventions document of record.

    Judge:
    - The three constraints: coordination anchored on repos (repo_id NOT NULL, repo-scoped leases,
      no silent fallback on an unresolvable repo); substrate not workflow (a capability that could
      live in a customer's skill belongs in the skill; jobs carry opaque metadata the server never
      interprets); coding-agent agnostic (no client-specific hook, plugin, skill, or tool annotation;
      agentType is never validated against a list)
    - Crate boundaries: tower-core owns the domain and ALL SQL with no HTTP and no auth; of-auth,
      tower-billing, tower-mcp, tower-web, tower-trackers layer on it; tower-server only assembles
    - Tenant isolation's two guards: the Tx/OrgId API shape and the RLS policies, plus the
      startup check that vouches for guard 2
    - The console API staying read-only over the queue; the router and OpenAPI document built from
      one catalog list
    - Whether a public-interface change is additive, and if not, whether it is named in the spec,
      the plan, and the PR body
    - Alignment with the committed spec and plan

    Report: Strengths, Issues (Critical / Important / Minor), Assessment.
```

### Independent security review — `subagent_type: security-auditor`

**Blind by construction.** This dispatch receives the diff and nothing else — no spec, no plan, no
task brief, no PR-body summary, no implementer report (Non-Negotiable Rule 5). Keep the "do not read"
instruction attached to the prompt so a fallback to a generic subagent type cannot silently drop it.

```
Agent tool:
  subagent_type: security-auditor
  description: "Security review: PR #<N>"
  prompt: |
    Perform an independent security review of PR #<N> in savvagent/otto-factory.

    Read ONLY the diff: `gh pr diff <N>`.
    Do NOT read the PR description, the issue, the spec, the plan, or any summary of intent. Your
    findings must come from the code as written, so they cannot be steered by the author's framing.

    This is a hosted, multi-tenant server. Weigh these hardest:
    - Multi-tenancy: can any statement reach another org's rows? Is every tenant table's access
      scoped by org_id AND covered by a row-level-security policy? Does any new SQL live outside the
      crate that pins the transaction's org?
    - AuthN/AuthZ: is authorization decided before the handler body runs? Is an org the caller does
      not belong to indistinguishable from an org that does not exist? Is any credential spent on a
      GET? Are session cookie attributes intact (HttpOnly, Secure, Path=/, SameSite=Lax, __Host-)?
    - Tokens and secrets: raw tokens never persisted (hashes only), token audience/org fixed at
      issuance, exact redirect-URI matching, no secret in a log line, an error message, or a response
    - Account enumeration: does any new branch reveal whether an account exists?
    - Injection, deserialization, SSRF, path traversal, and unsafe handling of untrusted input
    - Rate limiting and the client-IP source: is the IP taken from a header the proxy overwrites?
    - Denial of service: unbounded queries, transactions held across long polls or network calls
    - Anything that fails open where it should fail closed

    Report: Findings (Critical / High / Medium / Low) with file:line refs and a concrete fix each,
    then an overall assessment.
```

---

## Review-Response Subagent — Phase 4 Step 9 — `subagent_type: general-purpose`

```
Agent tool:
  subagent_type: general-purpose
  description: "Address PR review feedback"
  prompt: |
    You are addressing PR review feedback on PR #<N> of savvagent/otto-factory, for <ref>.
    Follow otto-tower-development requirements (Phase 4 step 9).

    Your job:
    - Read all unresolved review threads on the PR
    - For each comment: either fix-and-reply ("Fixed in <sha>") or explicitly dismiss with
      reasoning. NEVER silent dismissal.
    - Work in the existing worktree; run `cargo fmt --all` before every Rust commit, and
      `cargo test --workspace` + `cargo clippy --all-targets -- -D warnings` before pushing. If
      `web/` changed, also `npm run check && npm run lint && npm test`.
    - After each reply, resolve the conversation thread via GraphQL:
        gh api graphql -f query='mutation {
          resolveReviewThread(input: {threadId: "<thread_id>"}) {
            thread { isResolved }
          }
        }'
    - Always reply inline to each comment explaining how the feedback was addressed
      (keeps the review thread traceable).
    - For automated reviewers, a flagged false positive should be verified (e.g. with
      `cat -A` for whitespace/table-formatting flags) then dismissed with reasoning.
    - Never add AI self-attribution to a commit, a reply, or the PR body.
    - If the same thread remains unresolved across multiple subagent runs, escalate
      (do not silently retry).

    Return when all threads are resolved or escalation is needed. The main thread
    receives only the summary (what was fixed, what was dismissed, any escalations).
```
