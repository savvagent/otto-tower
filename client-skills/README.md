# client-skills/

Per-client templates that register a session-start marker job on Savvagent Tower,
maintained by the community rather than by the server.

## Purpose

Savvagent Tower's server is coding-agent agnostic by design: it never depends on, or
special-cases, any one client's hook, plugin, or skill system (see the repo's own
constraint 3 in the root `CLAUDE.md`). But developers still want every coding-agent
session that starts work in a repo to become visible in Savvagent Tower automatically,
rather than relying on the developer (or their agent) to remember to call `add_job` by
hand.

Since that automation can never live in the server, it lives here instead: a directory
of small, per-client templates a customer installs into their own agent, distributed
from this open-source repo so the community — not the Savvagent Tower maintainers — carries
the burden of keeping each template working as that agent's own hook/skill format
evolves.

## The behavior contract

Every template in this directory — whatever shape a given client's automation surface
actually takes — follows the same job lifecycle:

1. **Resolve the repo** via `resolve_repo` (by git remote URL, the normal case). On any
   failure — the repo isn't registered, there's no Savvagent Tower MCP connection
   configured, the server is unreachable — stop immediately and take no further
   Savvagent Tower action. This directory never auto-registers a repo on a resolution
   failure. (What, if anything, may be said about the failure is point 5's job, not
   this one's.)
2. **Use the resolved canonical `slug` for everything downstream.** The `slug` `resolve_repo`
   returns is the one identifier used in the job's `repo` argument and in the idempotency
   key below — never a second, independently derived string (e.g. the raw remote URL).
3. **Create an idempotent marker job.** Call `add_job` with an `idempotencyKey` derived
   from repo slug + a 16-hex-character SHA-256 digest of the branch + UTC date
   (`session-<slug>-<branch_digest>-<yyyy-mm-dd>`), so repeated session starts on the
   same branch the same day collapse to one job rather than spamming the queue, and no
   attacker-influenceable branch string ever lands in a key or in model-facing text
   verbatim. A detached HEAD (no stable branch name) skips queuing entirely.
4. **Close the job out — never leave it claimable.** `add_job` alone never establishes a
   claim, so a marker left at `add_job` sits `pending` in the general `ready` list, which
   is exactly what "a marker job" must never look like — real work waiting to be picked
   up. Branch on the returned job's `status`: `completed` → nothing further to do;
   `pending` → `claim_jobs` on that exact returned job id, then (only on a successful
   claim) `complete_job`; anything else → stop without guessing.
5. **Fail silently, always — with one precisely-scoped exception.** Every failure at any
   step above — a resolution failure, an MCP call erroring, no Savvagent Tower connection
   configured at all — never blocks or delays the developer's session, never retries, and
   never produces a narration of what happened. The actor carrying out the calls (a
   deterministic hook/script, or — for a client whose automation surface can't itself
   reach a credential — the model following an injected instruction) may give **at most
   one short, non-dramatizing line** in a channel that does not interrupt the developer's
   own work: a hook's own stderr, or a single sentence in a model's reply. Nothing about
   an Savvagent Tower-side failure is ever the substance of a reply or a tool output, error
   detail is never surfaced, and a template is free to stay fully silent instead. This is
   one policy for both shapes — a "script prints to stderr" exception and a "model says
   one line" exception are the same rule applied to the actor that happens to exist.
6. **Treat every local value you interpolate as untrusted — and know what a character
   allowlist actually defends.** A git remote URL, a branch name, a file path — anything
   read from the developer's own repository state and then embedded in text a model is told
   to treat as instructions — is something an attacker, not the developer, chose, even
   though the value is "local" (anyone who controls a remote you add, or a branch you
   fetch, controls these bytes). A character allowlist over such a value defends the
   **shell** it passes through, nothing more: letters, digits, `.`, `-`, `_`, and `/` are
   already sufficient to write a fluent imperative sentence, so a value built only from
   "safe" characters can still read as an instruction override once it lands in front of a
   model. The control that actually matters for any value reaching model-facing text is
   that the value be **structurally incapable of reading as prose** — a digest (e.g. a
   short hex hash of a branch name, never the branch name itself), a value drawn from a
   fixed enumeration, or a narrowly-grammared form (e.g. an actual URL grammar — scheme,
   host, length-capped path segments — rather than a flat character class over the whole
   string, matched as one whole string with no per-line ambiguity for something like `grep`'s
   line-anchored `^`/`$` to exploit against a value that can carry an embedded newline).
   Present the result as clearly labeled, fenced *data* the instruction refers to by name,
   never interpolated directly into the imperative steps themselves — fencing is real
   **defense in depth**, but never the primary control, and never a substitute for the value
   itself being non-prose-capable. Additionally, **gate emission on something the developer
   explicitly opted into, scoped as narrowly as the opt-in can honestly be** (e.g. an
   allowlisted `host/owner` pair, not merely a host — a bare hostname's realistic content
   arms a hook in every repo on that host, which is rarely the intended population) *before*
   any untrusted byte is used to build model-facing text — the one thing the model can do
   that nothing upstream of it can, is check whether a repo is *actually* related to this
   Savvagent Tower org (`resolve_repo`), and by the time the model runs that check the
   untrusted bytes are already in its context. See `claude-code/session-start-hook.sh` and
   its README's Security section for a worked example: a hex branch digest instead of the
   raw branch name, a URL grammar (matched with the language's own whole-string regex
   construct, not a line-oriented tool) instead of a character class for the remote, an
   owner-scoped opt-in allowlist read from that same grammar match's own capture groups and
   checked before anything else is built, and how to keep a shell script's own heredoc
   construction from re-interpolating a captured value.

## What this directory is not

- **Not an auth setup.** Every template here assumes the client already has a working
  Savvagent Tower MCP connection (OAuth or a personal access token). Configuring that
  connection is `docs/clients/matrix.md`'s job, and the console's connect page's job —
  not this directory's.
- **Not a way to claim or work real jobs.** These templates only ever create, claim, and
  immediately complete a session-marker job. They never touch `ready`'s general pool of
  claimable work.
- **Not a server-side feature.** Nothing here changes `crates/*`. Savvagent Tower the server
  stays exactly as ignorant of whether, or how, any client automates queue registration
  as it was before this directory existed.

## Launch scope

This repo's own conformance work (`docs/clients/matrix.md`) has only ever driven Claude Code
and Copilot CLI live against a real Savvagent Tower server. Shipping a fabricated, never-run
template for a client nobody here has actually exercised would read as working when it
isn't — worse than an honest "not yet, here's the shape a PR should take." So launch ships
exactly one fully working reference template (Claude Code) and a stub `README.md` for every
other client this directory names, each stating the target client, a starting hypothesis for
its automation surface, and the contribution checklist below — no functioning hook code, no
fabricated verification claims. A stub graduates to a working template only once someone has
actually run it against a live `of-server` and can honestly check every contribution-checklist
box.

## Directory layout

```
client-skills/
  README.md          — this file
  claude-code/       — a fully working reference template
  copilot-cli/       — stub: contribution invitation
  cursor/            — stub: contribution invitation
  codex/             — stub: contribution invitation
  otto-cli/          — stub: contribution invitation
  generic/           — stub: contribution invitation
```

Subdirectory names match `web/src/lib/clients.ts::CLIENTS`'s own `id` values exactly (see
that file's own `generic` entry: "exists so that a client nobody here has heard of is still
a first-class citizen"), so a future console link can construct the directory URL from `id`
alone with no separate mapping table to keep in sync.

See `claude-code/README.md` for a worked example — the fullest description of the
contract above applied to one real client. Claude Code's own template needs a two-actor
split (a deterministic hook script handing an instruction to the model, which is the
party that actually holds an authenticated Savvagent Tower MCP connection) because a
`SessionStart` hook process has no access to the session's own MCP credentials. **This
split is specific to Claude Code's own limitation, not part of the contract itself.** A
client whose automation surface can itself reach a valid Savvagent Tower credential
directly should implement the whole sequence above as one deterministic script — no
model hand-off required. If neither shape fits a given client, propose a different
mechanism entirely in that client's own `README.md` and PR description; the job-lifecycle
contract above is the strong default, not a mandate on how it's carried out.

## Contributing a new client template

A new (or updated) client template's PR description should be able to tick every box
below:

- [ ] Names the target client and the minimum version tested against.
- [ ] Names the exact session-start (or equivalent) mechanism used, and links to that
      client's own documentation for it.
- [ ] Lists, in order, every Savvagent Tower MCP tool called and with what arguments.
- [ ] States plainly whether this was verified against a real running `of-server`, or is
      an unverified starting hypothesis (see `docs/clients/matrix.md`'s own register for
      this — "not run" is an acceptable, honest answer; a silent claim of "verified" when
      it wasn't is not).
- [ ] Follows the silent-failure contract above — never blocks or delays the developer's
      session on any Savvagent Tower-side failure, never retries, and surfaces nothing beyond
      (at most) one short, non-dramatizing line in the actor's least-intrusive channel
      (a hook's stderr, or one sentence in a model's reply).
- [ ] Does not attempt to auto-register an unregistered repo.
- [ ] Does not call `claim_jobs` against anything from the general `ready` pool — only
      against a job id this same template just created.
- [ ] Every local value that reaches a model-facing instruction (a remote URL, a branch
      name, a file path) is **structurally incapable of reading as prose** — a digest, an
      enumerated value, or a narrowly-grammared form — not merely restricted to a "safe"
      character set. A character allowlist alone is not sufficient and does not satisfy this
      box: letters, digits, `.`, `-`, `_`, `/` already spell fluent English. State plainly, in
      the PR description, what specifically makes each interpolated value non-prose-capable.
      Fencing/labeling the value as data the instruction refers to (rather than splicing it
      into an imperative step) is required in addition to this, as defense in depth — never
      as a substitute for it.
- [ ] If the template fires automatically in every repo the client opens (rather than only on
      an explicit, per-invocation action by the developer), emission is gated on something the
      developer opted into beforehand, scoped no wider than the population it is meant to
      cover (e.g. an allowlisted `host/owner` pair read from a file under the developer's own
      control — a bare hostname is not narrow enough, since its realistic content arms the
      gate in every repo on that host) — checked before any untrusted byte is used to build
      model-facing text, so the template is inert by default in a repo the developer never
      opted in.

There is no CI enforcement of this directory's contents — no lint job, no schema
validator. The checklist above is the only guard, enforced like any other open-source
contribution: at PR review, by a human.
