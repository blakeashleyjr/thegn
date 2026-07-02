# Design

## Detection (core, pure-ish)

A detector enumerates known external agents by probing PATH (`which claude` /
`codex` / `cursor`) and their known config locations (`~/.claude/mcp_servers.json`,
`~/.codex/mcp.json`, `~/.cursor/mcp.json`). The known-agent table (id → binary,
config path, registration style) is declarative + unit-tested; the actual PATH/FS
probe is the I/O seam (exercised by smoke, per the coverage split).

## Registration (idempotent, non-clobbering)

For each detected agent, `szhost agent register` (extending the `cmd/agent.rs`
setup surface):

1. Prefer the agent's own CLI (`claude mcp add superzej -- szhost mcp serve`).
2. Fall back to a **merge** of the agent's config file: parse it, insert/update
   only superzej's server entry, write it back — never touching other entries.
   Re-running is a no-op (idempotent).

`szhost agent unregister`/`disable` removes only superzej's entry via the same
merge. The MCP surface served is the existing `mcp/router.rs` house tools.

## Error markers (bouncer / proxy)

Stable marker strings (e.g. `SUPERZEJ_APPROVAL_REQUIRED`, `SUPERZEJ_QUOTA_EXHAUSTED`,
`SUPERZEJ_TOOL_DENIED`) are emitted in the agent-facing error text at the bouncer
approval seam and the proxy quota/route-failure seam, each paired with a
machine-readable next-step. The vocabulary is a small enum, unit-tested (each
marker renders its stable string + guidance; unknown conditions fall back to a
generic marker).

## Invariants

- **Event loop**: registration is a CLI/synchronous action (not in the compositor
  loop); when invoked in-session it runs off-loop. No polling timer.
- **Render**: none (registration is a CLI verb); markers are text in existing
  agent/error surfaces.
- **State**: `user_version` bump only if the registered set is tracked for removal;
  otherwise the agents' config files are the record.
- **Additivity**: entirely AI layer; detection/registration no-ops when no external
  agents are present, and the shell never depends on them.

## Alternatives considered

- **Overwriting agent configs** — rejected; the idempotent merge is the whole point
  (codag-cli's non-clobbering pattern) so users keep their own MCP servers.
- **Only registering the first-party agent** — insufficient; the value is being
  reachable from whatever agent the user already runs.
- **Opaque errors** — rejected; agent-addressed markers turn failures into
  actionable next-steps the LLM can follow.
