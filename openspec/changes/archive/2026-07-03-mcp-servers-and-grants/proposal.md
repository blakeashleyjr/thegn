## Why

thegn ships one agent (the managed pi) with hard-coded house tools, but users
can't extend the agent with their own MCP servers, and thegn has no
permission model for the external tools it runs. Zed solves both with one idea:
extensions **declare** MCP servers, and every side-effecting host call is gated
by a **capability declared in the manifest** (process:exec / download_file /
npm:install, glob-scoped). This change brings that pattern to thegn.

## What Changes

- Add a pure **capability-grant model** in `thegn-core` (`grants.rs`): a
  `Grant { kind, scope }` (kinds `process:exec`, `download_file`, `npm:install`,
  `cargo:install`) with glob-scoped matching, a `Grants` set, and
  `Grants::allows(action)` — the least-privilege check applied when acquiring or
  launching a _user-declared_ tool. Includes a small `*`/`**` glob matcher.
- Add a **`[mcp_servers.<name>]` config** + model (`mcp::config`): a launch spec
  (`command`, `args`, `env`), an optional managed-tool `source` (npm / cargo /
  github-release) to acquire the server binary, and the server's `grants`. A pure
  builder emits the standard **`mcpServers`** settings block the agent consumes.
- **Consume it in the agent layer**: `thegn agent setup` merges the
  `mcpServers` block into the managed pi's `settings.json` (additive; the
  de-facto key used by Claude/Cursor/pi-style agents), so declared servers ride
  alongside the `thegn-acp` house tools.
- Add a **`thegn mcp` CLI** (`list`, `emit`, `install <name>`) and surface
  declared servers + their grants in **`thegn doctor`**. Acquisition is
  grant-checked: a server whose `source`/launch isn't covered by a matching
  grant is refused with a clear message.

Non-goals (deferred): running MCP servers as long-lived thegn-supervised
panes/daemons, and applying grants to the _first-party_ pi/bs tools (they are
implicitly trusted; grants gate _user-declared_ servers).

## Capabilities

### New Capabilities

- `capability-grants`: the glob-scoped grant model (kinds, scopes, matcher,
  `allows`) and its enforcement at the acquire/launch boundary for user-declared
  tools.
- `mcp-servers`: user-declared MCP servers — the `[mcp_servers.<name>]` config,
  the launch/acquire model, the `mcpServers` settings-block builder, agent-setup
  injection, the `thegn mcp` CLI, and doctor reporting.

## Impact

- **Code:** new `crates/thegn-core/src/grants.rs` + `crates/thegn-core/src/mcp/config.rs`
  (+ `mcp/mod.rs` `pub mod config;`, `lib.rs` `pub mod grants;`); `config.rs`
  gains a `mcp_servers` map field; new `crates/thegn-host/src/cmd/mcp.rs`
  (+ `main.rs`/`cmd/mod.rs` wiring); `cmd/agent.rs` merges the `mcpServers`
  block into pi `settings.json`; `cmd/doctor.rs` reports servers + grants.
- **Dependencies:** none new (`cargo`/`npm` installers already used; the resolver
  handles acquisition).
- **Invariants:** core stays pure + 95%-coverage-gated (grant matching, the
  settings-block builder, and server resolution are fully unit-tested); the
  settings-json merge + installs are `cov_ignore`/smoke seams; no event-loop or
  render-plan surface, and no ratcheted god-file grows (config.rs stays under its
  ceiling; new logic lives in new modules).
- **Roadmap (`tasks.md`):** advances **AL** (MCP server, 455–466) and **AJ**
  (security/opsec — the capability manifest); consumes the Phase-1 managed-tool
  resolver.
