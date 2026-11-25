# MCP proxy — agent wiring delta

## MODIFIED Requirements

### Requirement: Agent CLIs are wired by explicit, reversible command

`thegn mcp wire [--agent <kind>] [--remove]` SHALL write thegn's entries into
supported agent CLIs' MCP settings via per-vendor adapters (vendor-specific
paths and formats confined to their implementation files): `thegn`, the proxy
hub, and `thegn-serve`, thegn's own docs and live-state server with its scopes
resolved from config at launch. Both entries MUST be argv only (no `env`).
Wiring MUST be explicit (never a side effect of launch; the welcome tour may
offer it, but writes only after the user presses its one accept key),
idempotent,
marker-tagged so thegn-managed entries are distinguishable, and MUST refuse to
modify or remove entries it did not mark. `--remove` MUST restore the settings
to their pre-wire state with respect to thegn's entries, and MUST NOT create a
settings file or container that did not exist. A settings file that does not
parse MUST be refused, never rewritten. JSON settings carry the marker as an
`x-thegn-managed` key; TOML settings (Codex `config.toml`, relocated by an
absolute `CODEX_HOME`) carry it as a comment above the server table and keep
the rest of the document, comments included, unchanged. `thegn mcp wire` also writes lifecycle hooks under the same rules: for Claude a
marked `Stop` and `Notification` command group in `settings.json` (relocated
by an absolute `CLAUDE_CONFIG_DIR`), for Codex a top-level `notify` argv. Both
call `thegn agent hook`, which reports to the daemon over the control socket
and never writes to the terminal. A Codex `notify` the user wrote is never
overwritten or removed. A bare `thegn mcp
wire` targets the configured `[[agents]]` plus every supported agent whose home
directory exists for the user. Hermes settings (`~/.hermes/config.yaml`,
relocated by an absolute `HERMES_HOME`) are edited as text: the marker is a
comment above each entry under the top-level `mcp_servers:` mapping, and a
flow, sequence, CRLF or duplicate container is refused rather than rewritten.

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

#### Scenario: Hooks are reversible and user hooks survive

- **WHEN** Claude's `settings.json` holds a user `Stop` hook, `thegn mcp wire`
  runs and then `thegn mcp wire --remove` runs
- **THEN** the user's hook is untouched throughout and the file returns to its
  pre-wire content

#### Scenario: The tour wires only on accept

- **WHEN** the welcome tour detects an unwired Claude and the user presses
  `w` on the agents step and finishes the tour
- **THEN** the agent's MCP entries and hooks are written; without the key
  press nothing is written

#### Scenario: Hermes is wired in place

- **WHEN** `thegn mcp wire --agent hermes` runs against a `config.yaml` holding
  user comments and a user `mcp_servers.git` entry
- **THEN** `thegn` and `thegn-serve` are added under thegn's marker comment,
  the user's text is unchanged, and `--remove` restores the file exactly

### Requirement: Remote environments are wired with the in-sandbox command

After the provisioning steps that install coding agents and sync their logins,
a Sprites or Vercel environment SHALL have each installed agent's MCP settings
wired with thegn's two entries, its lifecycle hooks (Claude `Stop`/`Notification`,
Codex `notify`) and the `thegn` skill seeded in the agent's
user-level skill folder. The entry command MUST NOT be the host's `thegn` path:
it is an argv-only resolver that runs `thegn` from `PATH`, else the resident
bridge's `thegn`, else exits with a clear error. The pass MUST be idempotent
across re-provision and snapshot restore, MUST rewrite only thegn-marked
entries (a host path carried up in synced settings is replaced), MUST NOT touch
a foreign entry, an unparseable file or a user-edited skill, and MUST NOT fail
the provisioning step.

#### Scenario: Existing user is offered the wiring once

- **WHEN** the first-run tour is already done, an installed agent is not fully
  wired, and thegn starts
- **THEN** one confirm menu offers to wire it (nothing is written until
  confirmed), and it is never offered again whatever the answer

#### Scenario: Re-provision changes nothing

- **WHEN** a provisioned environment's agent step runs again
- **THEN** no settings file or skill is rewritten

#### Scenario: Host path is replaced

- **WHEN** the synced `~/.claude.json` carries a thegn-marked entry naming the
  host's `thegn` path
- **THEN** that entry becomes the in-sandbox resolver and no host path remains
