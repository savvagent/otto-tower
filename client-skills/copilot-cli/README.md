# Copilot CLI — session-start marker (contribution wanted)

**Target client:** GitHub Copilot CLI (`id: 'copilot-cli'` in the console's
`web/src/lib/clients.ts`, config at `~/.copilot/mcp-config.json`).

**Status: stub.** No working hook code ships here yet — see `../README.md`'s "Launch
scope" note: this repo's own conformance work has only ever driven Claude Code and
Copilot CLI live, and shipping unverified hook code for an automation surface nobody has
actually run against a real Savvagent Tower server would read as working when it isn't.

**Starting hypothesis (unverified):** Copilot CLI's own session/task lifecycle hooks, if
and when it exposes one equivalent to Claude Code's `SessionStart`. Check Copilot CLI's
own current documentation for whatever it calls its extensibility or hook surface before
assuming this shape still applies.

**Does Copilot CLI's automation surface hold a credential itself?** Unknown — this is
exactly the first thing a contributor needs to establish. If it can reach a valid
Savvagent Tower MCP credential directly, prefer a single deterministic script implementing
`../README.md`'s job-lifecycle contract outright (no model hand-off needed). If it can't
— the same limitation Claude Code's own `SessionStart` hook has — the two-actor split
documented in `../claude-code/README.md` is a worked example to adapt, not a mandate; a
different mechanism is fine if neither shape fits.

## Contributing this template

Follow `../README.md`'s contribution checklist exactly. In short, a PR adding this
template should be able to tick every box there: name the Copilot CLI version tested,
name the exact mechanism used (with a link to Copilot CLI's own docs for it), list every
Savvagent Tower MCP tool called and with what arguments, state plainly whether it was
verified against a real running `tower-server` or is an untested hypothesis, and follow the
silent-failure / no-auto-registration / claim-only-what-you-created rules from the
contract.
