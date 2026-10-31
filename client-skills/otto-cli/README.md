# Otto CLI — session-start marker (contribution wanted)

**Target client:** Otto CLI (`id: 'otto-cli'` in the console's `web/src/lib/clients.ts`,
config at `~/.otto/config.toml`).

**Status: stub.** No working hook code ships here yet — see `../README.md`'s "Launch
scope" note: this repo's own conformance work has only ever driven Claude Code and
Copilot CLI live, so a fabricated, never-run template for Otto CLI would read as working
when nobody has verified it.

**Starting hypothesis (unverified):** Otto CLI's own session-lifecycle hook surface, if
it exposes one equivalent to Claude Code's `SessionStart`. Check Otto CLI's own current
documentation for whatever it currently calls this before assuming this hypothesis still
holds.

**Does Otto CLI's automation surface hold a credential itself?** Unknown — establish this
first. If Otto CLI's hook surface can reach a valid Savvagent Tower MCP credential directly,
prefer a single deterministic script implementing `../README.md`'s job-lifecycle contract
outright. If it can't — the same limitation Claude Code's own `SessionStart` hook has —
`../claude-code/README.md`'s two-actor split (a deterministic script handing an
instruction to the model) is a worked example to adapt, not a mandate; propose a
different mechanism entirely if neither shape fits.

## Contributing this template

Follow `../README.md`'s contribution checklist exactly. In short, a PR adding this
template should be able to tick every box there: name the Otto CLI version tested, name
the exact mechanism used (with a link to Otto CLI's own docs for it), list every
Savvagent Tower MCP tool called and with what arguments, state plainly whether it was
verified against a real running `tower-server` or is an untested hypothesis, and follow the
silent-failure / no-auto-registration / claim-only-what-you-created rules from the
contract.
