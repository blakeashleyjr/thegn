# MCP proxy — agent wiring delta

## MODIFIED Requirements

### Requirement: Agent CLIs are wired by explicit, reversible command

`thegn mcp wire [--agent <kind>] [--remove]` SHALL write thegn's entries into
supported agent CLIs' MCP settings via per-vendor adapters (vendor-specific
paths and formats confined to their implementation files): `thegn`, the proxy
hub, and `thegn-serve`, thegn's own docs and live-state server with its scopes
resolved from config at launch. Both entries MUST be argv only (no `env`).
Wiring MUST be explicit (never a side effect of launch), idempotent,
marker-tagged so thegn-managed entries are distinguishable, and MUST refuse to
modify or remove entries it did not mark. `--remove` MUST restore the settings
to their pre-wire state with respect to thegn's entries, and MUST NOT create a
settings file or container that did not exist. A settings file that does not
parse MUST be refused, never rewritten. JSON settings carry the marker as an
`x-thegn-managed` key; TOML settings (Codex `config.toml`, relocated by an
absolute `CODEX_HOME`) carry it as a comment above the server table and keep
the rest of the document, comments included, unchanged. A bare `thegn mcp
wire` targets the configured `[[agents]]` plus every supported agent whose home
directory exists for the user.

#### Scenario: Wire twice, one entry

- **WHEN** `thegn mcp wire` runs twice for the same agent
- **THEN** the agent's settings contain exactly one thegn-marked proxy entry
  and one thegn-marked `thegn-serve` entry, and the second run writes nothing

#### Scenario: User entries are untouchable

- **WHEN** an agent's settings contain a user-authored MCP entry and
  `thegn mcp wire --remove` runs
- **THEN** only thegn-marked entries are removed

#### Scenario: Codex is wired in place

- **WHEN** `thegn mcp wire --agent codex` runs against a `config.toml` holding
  user comments and a user `[mcp_servers.git]` table
- **THEN** `[mcp_servers.thegn]` and `[mcp_servers.thegn-serve]` are added
  under thegn's marker comment and the user's comments and table are unchanged

#### Scenario: A new user without agent config

- **WHEN** no `[[agents]]` is configured, `~/.claude` exists, and a bare
  `thegn mcp wire` runs
- **THEN** Claude is wired instead of the command refusing
