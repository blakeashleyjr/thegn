## Context

The core `mcp` module is a house-tool _router_ (git/forge), not a declarative
server model; there is no `[mcp_servers]` config. The managed pi reads its config
from `settings.json` (written by `pi install packages/superzej-acp` during
`szhost agent setup`). `capabilities.rs` models sandbox _isolation_, not
permission grants — there is no grant model. `dns_filter.rs` has a small
allow/deny glob matcher worth mirroring. Config sub-configs follow the
`BTreeMap<String, ValueStruct>` + serde-default pattern (`Bundle`, `EnvConfig`).
`config.rs` is a ratcheted god-file with ~16 lines of headroom, so new structs
must live in sibling modules with only a small field added to `Config`.

Zed: extensions declare context (MCP) servers, and every side-effecting host call
is gated by a glob-scoped capability in the manifest.

## Goals / Non-Goals

**Goals:**

- A pure, tested capability-grant model (glob-scoped) in core.
- A `[mcp_servers.<name>]` config + pure model + `mcpServers` settings-block
  builder.
- Real consumption: merge the block into the pi settings during `agent setup`.
- `szhost mcp` CLI + doctor reporting; grant-checked acquisition.

**Non-Goals:**

- Supervising MCP servers as long-lived superzej daemons/panes.
- Grant-gating the first-party pi/bs tools (implicitly trusted).
- Changing the `superzej-acp.ts` extension (no TS rebuild).

## Decisions

### Grant model (`superzej-core/src/grants.rs`) — pure

```
pub enum GrantKind { Exec, Download, NpmInstall, CargoInstall }   // parsed from "process:exec" etc.
pub struct Grant { pub kind: GrantKind, pub scope: String }        // serde: {kind, scope}
pub struct Grants(Vec<Grant>);
pub enum Action<'a> { Exec(&'a str), Download(&'a str), Npm(&'a str), Cargo(&'a str) }
impl Grants { pub fn allows(&self, a: &Action) -> bool }           // some grant.kind==action && glob_match(scope, resource)
pub fn glob_match(pattern: &str, value: &str) -> bool              // `*` within-segment, `**` across, pure
```

- **Why a flat `{kind, scope}`**: serde-friendly for `[[mcp_servers.<name>.grants]]`, mirrors Zed's per-capability glob scoping, trivially testable. Kind strings (`process:exec`/`download_file`/`npm:install`/`cargo:install`) parse to `GrantKind`.
- **glob_match** is our own small matcher (segments split on `/`; `**` spans; `*` within a segment), unit-tested — richer than `dns_filter`'s suffix matcher, which is DNS-specific.

### MCP server model (`superzej-core/src/mcp/config.rs`)

```
pub struct McpServerConfig {
  pub command: Vec<String>,           // launch argv (e.g. ["npx","-y","@modelcontextprotocol/server-foo"])
  pub args: Vec<String>,              // extra args
  pub env: BTreeMap<String,String>,
  pub source: Option<managed_tool::Source>,   // optional acquisition
  pub grants: Vec<grants::Grant>,
}
pub fn settings_block(servers: &BTreeMap<String, McpServerConfig>) -> serde_json::Value
   // → { "<name>": { "command": <argv[0]>, "args": [argv[1..], ...args], "env": {...} }, ... }
```

- `settings_block` builds the de-facto `mcpServers` JSON (command/args/env), pure + tested.
- `config.rs` adds `pub mcp_servers: BTreeMap<String, mcp::config::McpServerConfig>` (field + default only — ~6 lines, within headroom).

### Consumption — merge into pi settings.json (`cmd/agent.rs`)

After `register()` (which runs `pi install`, writing `settings.json`), read that
JSON, set `["mcpServers"]` to `settings_block(&cfg.mcp_servers)` when non-empty,
write back. Additive (preserves `packages`), best-effort (a cache-like write; its
failure logs, never fails setup). Skipped entirely when no servers are declared.

- **Why merge, not own the file:** `pi install` owns `settings.json`; we only add
  a key the agent reads (the widely-adopted `mcpServers` convention).

### Enforcement + CLI (`cmd/mcp.rs`, `Command::Mcp`)

- `list`: print each server (command + grants).
- `emit`: print `settings_block` JSON.
- `install <name>`: if the server has a `source`, map it to a managed tool,
  **grant-check** the acquisition (`Grants::allows(Action::Npm/Cargo/Download(..))`)
  → refuse-with-reason if uncovered → else `managed_tool::install`.
- `doctor`: list declared servers + grants (text + `--json`).

## Risks / Trade-offs

- **[pi may ignore `mcpServers`]** If pi doesn't read that key, the merge is inert.
  → Additive + harmless; `mcpServers` is the cross-agent de-facto standard; `emit`
  lets users wire it manually; documented.
- **[settings.json clobber]** Merging could corrupt pi's file. → Read-modify-write
  the parsed JSON, set one key, preserve the rest; best-effort with a log on
  failure; skipped when no servers declared.
- **[grant bypass]** Enforcement is at superzej's acquire/launch, not the OS. →
  It's a declarative guardrail (like Zed's), not a sandbox; the sandbox layer
  remains the real boundary. Scope is user-declared servers only.

## Migration Plan

Pure addition: new core modules, one optional `Config` map field (defaults
empty ⇒ no behavior change, no settings merge), a new CLI verb, and an additive
settings merge gated on declared servers. No schema/persisted-state change.
Rollback = revert; an already-written `mcpServers` key is harmless.

## Open Questions

- Whether to also emit a discoverable `~/.superzej/pi/agent/mcp-servers.json`
  artifact in addition to the settings merge (deferred; the merge + `emit` cover
  it).
