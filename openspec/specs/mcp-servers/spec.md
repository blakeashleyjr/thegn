# mcp-servers Specification

## Purpose

Declaring, acquiring (grant-checked), and inspecting user-configured MCP
servers; thegn emits them as a standard `mcpServers` settings block that a
user's coding agents can consume.

## Requirements

### Requirement: Users declare MCP servers agents consume

thegn SHALL let users declare MCP servers in config (`[mcp_servers.<name>]`)
with a launch command, arguments, and environment, and MUST emit them as a
standard `mcpServers` settings block (`thegn mcp emit`) that a coding agent's
settings can consume.

#### Scenario: A declared server appears in the settings block

- **WHEN** a `[mcp_servers.<name>]` is configured and the settings block is built
- **THEN** the block contains an `<name>` entry with its command, args, and env

### Requirement: MCP server binaries are acquired via the resolver, grant-checked

thegn SHALL acquire a declared server's binary through the shared managed-tool
resolver only when the server's declared grants permit that acquisition, and MUST
otherwise refuse with a clear message naming the missing grant. This applies when
a server specifies an acquisition `source` (npm / cargo / github-release).

#### Scenario: Granted acquisition proceeds

- **WHEN** `thegn mcp install <name>` runs for a server whose grants cover its
  source acquisition
- **THEN** the binary is acquired via the resolver and pinned

#### Scenario: Ungranted acquisition is refused

- **WHEN** a server's source acquisition is not covered by any declared grant
- **THEN** thegn refuses the install and names the missing capability

### Requirement: Declared servers and grants are inspectable

`thegn mcp list` SHALL list declared servers with their launch spec and grants,
`thegn mcp emit` SHALL print the `mcpServers` settings block, and `thegn
doctor` SHALL report declared servers and their grants.

#### Scenario: list and doctor surface declared servers

- **WHEN** MCP servers are declared and `thegn mcp list` (or `doctor`) runs
- **THEN** each server is shown with its command and its declared grants

### Requirement: A declared server can opt into the proxy

`[mcp_servers.<name>]` SHALL accept a `proxy` subtable — `tools` (glob list;
required for any exposure; `["*"]` is the explicit everything opt-in) and
`scope` (`global`|`workspace`|`worktree`, default `global`) — declaring how
the mcp-proxy capability exposes the server. Absence of the subtable (or of
`tools`) MUST leave the server fully out of the proxy while remaining
available for direct `mcp emit` consumption. Every key MUST be documented in
`config/config.toml.example`.

#### Scenario: Declaration without exposure

- **WHEN** a server is declared with no `proxy.tools`
- **THEN** `thegn mcp emit` still includes it, and the proxy excludes it

### Requirement: Server env and args support worktree-context placeholders

Declared server `env` values and `args` SHALL support `{workspace}`,
`{worktree}`, `{repo_root}`, and `{branch}` placeholders, expanded from the
consuming connection's worktree context when the server is proxy-scoped.
Expansion MUST be a pure core function; a placeholder that cannot be resolved
for a given context MUST cause the server to be withheld from that context
(never launched with a literal `{...}` or an empty expansion).

#### Scenario: Unresolvable placeholder withholds, never garbles

- **WHEN** a server's env references `{workspace}` and the connecting context
  has no workspace
- **THEN** the server is not launched for that context and the reason is
  inspectable

### Requirement: Curated presets ship as data

`thegn mcp preset list` SHALL enumerate presets embedded with the binary and
`thegn mcp preset show <name>` SHALL print a vetted `[mcp_servers.<name>]`
block — pinned acquisition `source`, least-privilege `grants`, a default
`proxy` exposure, and a comment noting external requirements (API keys,
container runtime). `--write` MUST append the printed block to the user config
only after printing it; presets MUST never modify config otherwise. The
curated set SHALL include memory-category presets, of which at least one MUST
be fully local (no API key, no network at runtime), and presets are references
— thegn MUST NOT bundle, vendor, or hard-depend on any preset's software.

#### Scenario: Preset is print-first

- **WHEN** `thegn mcp preset show <name>` runs without `--write`
- **THEN** the TOML block is printed and no file is modified

#### Scenario: A local-only memory preset exists

- **WHEN** the preset list is enumerated
- **THEN** at least one memory preset declares no API-key requirement and a
  source runnable offline once installed
