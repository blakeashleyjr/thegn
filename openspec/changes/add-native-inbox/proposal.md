# Native inbox

## Why

The Lead needed its own MCP servers (and your credentials) to read mail and
tasks. THE-2178 and THE-2214 to THE-2216 move that into the daemon: it polls,
sanitizes and records items, so the Lead only reads notification rows.

## What Changes

- `[inbox]` and `[[inbox.accounts]]` (global config only; `inbox` is a
  security-relevant root). Kinds: `command`, `todoist`, `imap`; `gmail`, `jmap`,
  `outlook`, `slack` are reserved.
- Items are `inbox_item` notification rows (`inbox:<account>:<id>`); the seen
  state is a per-account set in `inbox_account_state` (schema v116, also widens
  the `producer_leases` CHECK to allow `inbox`).
- `inbox_item` automation event with `inbox_account|kind|label` selectors,
  capped at `review`, no `{message}` in spawn templates.
- `thegn inbox list|test|status` and the `producers.status` verb.
- Calendar stays a separate seam.
