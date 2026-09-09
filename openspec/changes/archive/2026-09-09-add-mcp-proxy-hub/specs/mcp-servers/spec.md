# MCP Servers

## ADDED Requirements

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
