# Add agent auto-registration + agent-addressed error markers

## Summary

Make superzej discoverable by the external agents already installed on a machine,
borrowing [`codag-cli`](https://github.com/codag-megalith/codag-cli)'s
battle-tested pattern. Two parts:

1. **Auto-registration** — detect installed agent CLIs (Claude Code, Codex,
   Cursor, …) on PATH and register superzej's MCP surface into each agent's config,
   preferring the agent's own `mcp add` CLI and falling back to an **idempotent,
   non-clobbering merge** of the agent's config file. A clean removal path
   (`disable`) un-registers.
2. **Agent-addressed error markers** — emit stable, machine-parseable error markers
   (e.g. at the bouncer / proxy quota seam) that tell the _agent_ what to do next
   (retry, request approval, run a command), instead of opaque failures.

## Impact

- **AL (MCP server)** — provides the client-side registration that makes
  superzej's house-tool MCP surface usable from external agents.
- **R (ACP)** — complements the first-party ACP agent by exposing the same surface
  to foreign harnesses via their MCP config.
- Relates to the managed-pi setup (`szhost agent setup`) — the same installer
  ergonomics, extended to _external_ agents.
- Extends the `agent` capability. **DB schema change: possible `user_version`
  bump** if the registered-agent set is tracked for clean removal (otherwise the
  agents' own config files are the record).

## Rationale

superzej already installs a managed pi agent and exposes a house-tool MCP router
(git*status/diff, pr_status, spawn_subtask, request_human). codag-cli shows the
clean way to make an MCP surface \_reachable* from whatever agents a user already
runs: detect them, register via their CLI, fall back to a careful config merge,
and be removable. Its second idea — error markers written _for the LLM_ (stable
strings that say what to do) — fits superzej's bouncer/proxy error seams, turning
"call failed" into an actionable instruction the agent can act on. Both are additive
and keep superzej a good citizen in a multi-agent machine.

## Non-goals

- **Bundling/managing external agents** — superzej registers into agents the user
  already installed; it does not install or version them (that is managed-pi's job
  for the first-party agent).
- **Clobbering user config** — registration is idempotent and merge-based; it never
  overwrites unrelated config, and `disable` cleanly removes only superzej's entry.
- **A new error taxonomy for the whole app** — markers target the agent-facing
  bouncer/proxy seams, not general UI errors.
- **AI-free-shell dependency** — this is entirely the AI layer; the shell does not
  depend on any external agent being present.
