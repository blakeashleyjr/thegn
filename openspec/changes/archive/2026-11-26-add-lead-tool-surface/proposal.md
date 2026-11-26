# Add the Lead tool surface

Linear: THE-2171, THE-2173. Roadmap: tasks.md group BA (item 761).

## Why

A meta-agent that supervises the workspace needs a small, scoped, audited tool
surface. Today the only way to give an agent control is the same-uid daemon
socket, which grants implicit admin, so scope limits are meaningless.

## What Changes

Add eleven catalog capabilities (HTTP, CLI and MCP; `approvals.resolve` HTTP and
CLI only), an opt-in `Notice` event frame, and Lead-authored automation rules
that are weaker than config rules.

## Decisions stated loudly

- One schema bump (110 to 111), owned by the automations lane. Approvals are
  `agent_attention` notification rows; the Lead group is a `terminals` row.
- `THEGN_CALLER` is audit-only, self-asserted, and never used for authorization.
- Rules with `origin = lead` may only use `notify.push` and `sessions.open`.
- `approvals.resolve` is Admin scope and is not exposed over MCP.
- Nothing runs when `[lead]` is absent; `merge.drain` spawns only on request.

## Impact

Specs: capability-catalog, mcp-servers, notifications.
Roadmap: tasks.md group BA, item 761.
