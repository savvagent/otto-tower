# Client conformance matrix

> **Note (platform split, savvagent/otto-factory#192).** The verification below was recorded when
> Savvagent Tower was its own authorization server. Registration, authorization, consent, and token
> exchange are now the Savvagent platform's (`/oauth/*` at its origin); Savvagent Tower serves only
> `/.well-known/oauth-protected-resource`, which names the platform. The `/oauth/*` paths and the
> `of_auth::oauth` references in the sections below are historical. Clients need only the MCP URL,
> which is `https://tower.savvagent.com/mcp` (the product was renamed from otto-factory; the old host keeps working, with a deprecation notice, until the published sunset), and should be re-verified against the platform after the cutover. Sessions recorded below ran under the previous name, so their server name and tool prefix read `otto-factory` / `mcp__otto-factory__*` there; a client added today with the commands in this file sees `Savvagent Tower` / `mcp__savvagent-tower__*`.

Milestone 1, task 12. Run on **2026-09-02** against `tower-server` built from `46c7896`
plus the two fixes this run produced (below), serving on one origin with a real
Postgres behind it.

The question this file answers is not "does the MCP surface work" — `tests/tools.rs`
answers that without a client in the room. It is **"can a coding agent a customer
actually runs reach it, authenticate, and coordinate with a different agent on the same
queue"**, which is a question only a real client can answer, because the failure modes
live in the client's own OAuth implementation and in the exact strings it sends.

| Client | Version | OAuth 2.1 | Personal access token | Tools called | Notes |
|---|---|---|---|---|---|
| Claude Code | 2.1.258 | ✅ end to end | ✅ | ✅ | Needs an interactive terminal to authorize; `claude mcp login` is the command. |
| Copilot CLI | 1.0.82 | ✅ server side, ⚠️ client step not scripted | ✅ | ✅ | Registers and opens the browser only in its interactive TUI; headless drops the server silently. |
| Cursor | — | not run | not run | — | Not installed on the conformance machine. Config shape is in `web/src/lib/clients.ts`. |
| Codex CLI | — | not run | not run | — | Same. |
| Otto CLI | — | not run | not run | — | Not installed on the conformance machine. Config shape (v0.29.1) confirmed against upstream source (`crates/otto/src/config_file.rs`, `crates/otto/src/mcp_config_writer.rs`) and its own round-trip tests, not a live run — see `savvagent/otto-factory#154` for the follow-up. |
| Any other MCP client | — | — | — | — | Streamable HTTP, `POST /mcp`. The `401` names the protected-resource document. |

Two agents were driven against **one queue in one org** and coordinated through it: the
run below is the milestone's "done means" criterion, executed rather than argued.

---

## What was verified, in order

1. **Sign-up through the console API**: signup → TOTP enrolment (secret, recovery codes)
   → confirm → create org `acme` → register repo `widget`.

   **This step is stale.** It predates both the removal of email and the move to passkeys.
   Account creation now happens in a browser (`navigator.credentials.create()`), so there
   is no scripted equivalent — re-running conformance needs a real browser or a CDP virtual
   authenticator for the first step. Everything below it (PATs, OAuth, the queue) is
   unaffected: those are layer 1 and never involved a human credential.
2. **PAT path** on both clients: mint in the console, paste into the client's config, call
   tools.
3. **OAuth path**: RFC 7591 dynamic registration, `/oauth/authorize` with PKCE S256 and an
   RFC 8707 `resource`, consent, code exchange, refresh rotation, and the negative cases.
4. **Two agents, one queue**: Claude Code (OAuth, owner) queued two jobs, claimed one, and
   took a lease on `widget@main`. Copilot CLI (PAT, a *second* invited user) called `ready`,
   saw the unclaimed job, claimed it, was refused the lease — `main of this repo is leased
   by agent-a until 2026-09-02 21:58:20 UTC` — messaged the first user by address, and
   completed its own job. Both agents' calls landed in one `usage` counter: 27 calls, 12
   billable.
5. **Protocol edges**: version negotiation, `GET`/`DELETE` on `/mcp`, a missing `Accept`
   header, a garbage bearer token, `resources/list` and `prompts/list`, and a 30-second
   `watch` long poll woken by another agent's write.

The client traffic was captured by putting a logging reverse proxy in front of the server
and pointing `OF_PUBLIC_URL` at it, so every quoted request below is a real one a client
sent, not a reconstruction.

---

## Claude Code 2.1.258

**OAuth — verified end to end.** Discovery, registration, consent, token exchange, and
tool calls with the resulting token.

```
POST /mcp                                    -> 401 + WWW-Authenticate: … resource_metadata="…"
GET  /.well-known/oauth-protected-resource   -> 200
GET  /.well-known/oauth-authorization-server -> 200
POST /oauth/register                         -> 201  redirect_uris ["http://localhost:3118/callback"]
GET  /oauth/authorize?…code_challenge_method=S256…&resource=…/mcp
POST /oauth/token                            -> 200  access_token + refresh_token
```

It probes with `server/discover` (the 2026-07-28 draft method) before `initialize`, takes
the `resource_metadata` pointer out of the `WWW-Authenticate` header rather than guessing a
well-known path, and sends `resource` on both the authorize and token requests.

**It will not authorize in a non-interactive session.** `claude -p` reports the server as
unauthenticated and stops; the flow needs `claude mcp login savvagent-tower` — or `/mcp` inside
an interactive session — because Claude Code asks the terminal to fall back to a pasted
redirect URL when the browser cannot reach it. Once a token is stored, `-p` sessions use it.

**Token path**: `claude mcp add --transport http savvagent-tower https://tower.savvagent.com/mcp --header "Authorization: Bearer sav_pat_…"`.
Verified by listing all 28 tools and calling `whoami`.

## Copilot CLI 1.0.82

**Token path — verified end to end**, in `~/.copilot/mcp-config.json` or a session's
`--additional-mcp-config`. It called `ready`, `claim_jobs`, `acquire_lease`,
`send_message`, `complete_job` and `stats` against the live queue.

**OAuth — the server half is verified; the client half is interactive only.** In its TUI
Copilot registers and opens the browser:

```
POST /oauth/register  -> 201  redirect_uris ["http://127.0.0.1:34467/"]   (ephemeral port, path "/")
GET  /oauth/authorize?…&resource=http://localhost:8081/mcp                 (browser opened)
```

Driving the last step from a script did not work — the code was issued to
`127.0.0.1:34467` and the client's own listener answered `404`, a TUI-state problem on the
client side, not a server response. To close the gap without hand-waving, the exchange was
then completed **using Copilot's own registered client**: its `client_id`, its ephemeral
loopback redirect URI, and the PKCE verifier it had written to
`~/.copilot/mcp-oauth-config/*.verifier`. That produced a working access token which called
`whoami` and came back `kind: oauth` with Copilot's `clientId`. Every server-side step
Copilot needs is therefore proven; what is unverified is Copilot's own handling after the
code reaches it.

**Non-interactive Copilot fails silently.** With an OAuth-only config, `copilot -p` reports
that no such tool exists rather than that a server needs authentication — it fetches the
protected-resource document and stops. Anyone scripting Copilot should use a PAT.

## Cursor, Codex CLI, and everything else

Not installed here, so not run. Their config shapes live in `web/src/lib/clients.ts` and are
rendered on the console's connect page; nothing in the server distinguishes them, and both
paths they would use — dynamic registration with a loopback redirect, or a bearer header —
are exercised above by clients that *are* installed. A client that speaks Streamable HTTP
and reads `WWW-Authenticate` needs nothing else.

---

## Fixed by this run

**`http://localhost:<port>` redirect URIs were refused, which meant Claude Code had no
OAuth path at all.** Registration answered
`redirect_uri must use https, except for http on 127.0.0.1 or [::1]`, and Claude Code
registers `http://localhost:3118/callback`. The old rule was deliberate — `localhost`
resolves through the host's resolver and the literal addresses do not — but the trade was
made on paper against a client that does not exist. `localhost` now sits in the RFC 8252
§7.3 carve-out with the literal addresses, port ignored, everything else exact; the reasoning
is in `of_auth::oauth::redirect_uri_matches` and the string Claude Code sends is now a test
case. Copilot, which registers `http://127.0.0.1:<port>/`, was unaffected either way.

**`watch` reported its timeout budget as the time it had waited.** A poll that was woken
after 22 seconds returned `waitedSeconds: 30`, which is wrong for the only reader that
matters — an agent pacing itself. It now reports the measured elapsed time, and the wake-up
test asserts the two differ.

## Found, not fixed — decisions for milestone 2

**`complete_job` and `fail_job` do not check who claimed the job.** A second user's agent
completed a job claimed by the first agent, with no error and nothing in the job row saying
a different party finished it. `finalize` checks only that the status is `in-progress`. The
console is read-only over the queue on the argument that "the agent doing the work is the
only party that can say when it is done" — that argument is not currently true of the MCP
surface itself. Whether the fix is a claimant check, an explicit `force`, or recording the
finisher separately is a spec decision, not a bug fix.

**A lease is held by a *user*, not by an agent.** Two agents authenticated as the same
person share lease identity: the second `acquire_lease` on a branch the first holds is
treated as a renewal and succeeds, because `holder_user_id` matches. The `agent` label is
descriptive only. Correct for two people, surprising for one developer running Claude Code
and Copilot side by side on one account — which is exactly what a trial user does first.

**Unknown tool arguments are dropped silently.** `add_job` with `body` instead of
`description` returns a created job with `description: null` and no error, because serde
ignores unknown fields. An LLM guessing a field name loses the field and is told nothing.

**The protected-resource document is served only at the root path.** RFC 9728 §3.1 builds
the well-known URL by inserting the path — for resource `…/mcp` that is
`/.well-known/oauth-protected-resource/mcp`, which answers `404` here. Neither client
noticed, because both follow the `resource_metadata` pointer in the `WWW-Authenticate`
header, which is what the MCP spec tells them to do. A client configured with the resource
URL and no `401` to consult would not find the document. Serving the same bytes at both
paths is a few lines and closes the case.

**The negotiated protocol version is always `2025-11-25`.** `rmcp` answers with its own
version regardless of what the client asked for — `2024-11-05`, `2025-06-18` and
`2026-07-28` all come back `2025-11-25`. Both clients accepted it. A strict client that
refuses an unrequested version would fail, and the fix belongs upstream in `rmcp`.

## Also observed, and correct

- `GET /mcp` and `DELETE /mcp` answer `405`. The transport is stateless on purpose: nothing
  is ever pushed, `watch` is a poll the agent initiates, and a server-held session would
  cost sticky routing across replicas for nothing.
- A request with no `Accept` header gets `406`, per the Streamable HTTP spec.
- A revoked or invented token gets `401` with the same `resource_metadata` pointer, so a
  client that has had its token revoked re-discovers instead of guessing.
- `resources/list` and `prompts/list` return empty lists rather than "method not found".
- Repo resolution failures name the registered slugs: `could not resolve a repo from
  (nothing supplied). Registered repos: "widget". Register this one with register_repo, or
  pass an explicit repo slug.`
- The consent screen leads with the redirect host, renders the client's self-chosen name as
  the untrusted string it is, and explains every scope in a sentence.
- Refusing consent redirects with `error=access_denied`; an unregistered `redirect_uri`
  renders an error page and redirects nowhere; `code_challenge_method=plain`, an unknown
  scope, and a foreign `resource` are each a `400`; a replayed authorization code revokes
  the tokens it issued; a reused refresh token revokes the family.

---

## Fixed since this run, not re-verified live

Recorded code-level fixes that landed after 2026-09-02 and were not re-exercised against a real
client as part of a conformance run — unlike everything above, which was.

**`server_info` now reports Savvagent Tower's own name/version at `initialize`.** Previously every
client saw `name: "rmcp"`, `version: "2.0.0"` regardless of what Savvagent Tower was running — the
MCP transport layer's own identity was leaked instead of the server's
(`docs/specs/2026-09-10-semver-release-design.md` §3).

**Breaking: the message tools take a channel.** `send_message`, `inbox`, `ack_messages` and
`unread_count` each require exactly one of `repo` (or `remote`) or `team` (a team slug), and
read or write that one channel. `send_message` no longer accepts `to`; the message args refuse
unknown fields, so a client still sending it gets a JSON-RPC `invalid_params` error (from
argument parsing) instead of a post. A missing or doubled channel, by contrast, is a tool error
whose `code` is `invalid_argument`. `ack_messages`' `upTo` must now be 0 or a message the caller
can read in that channel: a large number meaning "mark everything read" is `invalid_argument`
rather than clamped, because ids are global and cursors are per channel.
The `send_message` call in the Copilot CLI run above predates this and was not re-run
(`docs/specs/2026-10-24-message-channels-design.md`, savvagent/otto-factory#177).

---

## Re-running this

```bash
cargo run -p tower-server                      # .env, OF_PUBLIC_URL=http://localhost:8080
# console: sign up with a passkey, create an org, register a repo, mint a PAT
claude mcp add --transport http savvagent-tower http://localhost:8080/mcp
claude mcp login savvagent-tower                # interactive terminal required
copilot --additional-mcp-config @copilot.json --allow-all-tools -p "call whoami"
```

To capture what a client sends, point `OF_PUBLIC_URL` and `OF_RESOURCE_URI` at a logging
proxy in front of the bind address — the discovery documents are built from `OF_PUBLIC_URL`,
so the client follows the proxy of its own accord. A proxy used this way **must not follow
redirects**: the `303` off `/oauth/authorize` is addressed to the client's own loopback
listener, and a proxy that follows it eats the authorization code.
