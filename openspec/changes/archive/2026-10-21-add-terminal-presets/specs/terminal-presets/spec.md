# Terminal Presets

## ADDED Requirements

### Requirement: Presets are named launch configurations layered on the program registry

thegn SHALL let users declare `[[presets]]` entries — `name`, optional
`description`, `commands`, `mode` (`split`, the default, or `tabs`), a
worktree-relative `cwd`, an `env` overlay, and an optional named-`layout` ref
that takes precedence over `commands`. Each `commands` string MUST be resolved
first as an exact `[[agents]]`/`[[tools]]` name (the picker's resolution;
`"shell"` and the empty string are the login shell) and otherwise run via the
login shell — presets MUST NOT introduce a second program registry, and a
preset carries no provider/account/provisioning semantics of its own.
Validation MUST reject a preset with empty `commands` and no `layout`, MUST
warn on duplicate names and on a `layout` ref naming no saved layout (falling
back to `commands`), and every key MUST be documented in
`config/config.toml.example`.

#### Scenario: Commands resolve through the registry first

- **WHEN** a preset's `commands` are `["claude", "just dev"]` and `claude` is
  a configured `[[agents]]` entry
- **THEN** the first pane launches the `claude` agent entry (its command,
  sandbox wrap, and provider semantics) and the second runs `just dev` via the
  login shell

#### Scenario: An empty preset is rejected

- **WHEN** a `[[presets]]` entry has no `commands` and no `layout`
- **THEN** config validation fails naming the preset

### Requirement: The launch menu launches presets, agents, and tools into the active worktree

A bindable `launch-menu` action SHALL open a dedicated picker palette listing
every preset (name + description) followed by the agent picker's choices
(agents, then tools, then `shell`). Selecting an entry SHALL launch it into
the active worktree: agents/tools/shell as a new tab, presets per their
`mode`; every launch MUST compose through the same sandbox-wrapped launch-spec
pipeline as the new-worktree picker. Selecting an `[[agents]]` entry MUST
update the worktree's remembered agent; tools and presets MUST NOT. With no
active worktree the action SHALL surface a status message and do nothing. The
picker's rows are routed through a pending-selection gate so the main command
palette's rows remain actions only.

#### Scenario: The launch menu launches an agent at runtime

- **WHEN** the user invokes `launch-menu` in a worktree created with `shell`
  and picks the `aider` agent entry
- **THEN** a new tab runs the sandbox-wrapped `aider` command and the
  worktree's remembered agent becomes `aider`

#### Scenario: A preset row applies the whole shape

- **WHEN** the user picks the `dev` preset (`mode = "split"`, three commands)
- **THEN** one new tab opens in the active worktree with the three commands as
  an even split, each pane cwd'd to the worktree (plus the preset `cwd`) with
  the preset `env` applied

#### Scenario: No worktree, no launch

- **WHEN** `launch-menu` is invoked with no active worktree
- **THEN** a status message explains and nothing is spawned

### Requirement: Preset application never blocks the event loop

Applying a preset SHALL resolve its launch specs off the event loop (blocking
task, results delivered over a channel with a `TerminalWaker` pulse; the loop
drain spawns the panes), and the spawned panes SHALL join the same shared
resource ceilings as interactive panes.

#### Scenario: A slow sandbox resolve does not freeze the UI

- **WHEN** a preset is applied in a worktree whose sandbox resolution is slow
- **THEN** the UI keeps rendering and the panes appear when resolution lands,
  rather than the loop blocking on the resolve

### Requirement: Worktree templates can reference a preset

A `[[worktree_templates]]` entry MAY set `preset = "<name>"`, exclusive with
its `layout` and `commands` (validation MUST reject combinations and warn on
an unknown preset name). Creating a worktree from such a template SHALL apply
the referenced preset as the initial layout; the template's own `agent` field,
when set, still governs the remembered agent.

#### Scenario: One definition serves creation and runtime

- **WHEN** a template sets `preset = "dev"` and a worktree is created from it
- **THEN** the new worktree opens with the `dev` preset's panes, identical to
  invoking the preset from the launch menu afterwards

### Requirement: Preset launch is a catalog capability

Triggering a preset from outside the compositor SHALL be a
`thegn_core::capability::CATALOG` row (`LaunchPreset`) with its own
`required_scope` distinct from workspace-focus scopes, projected per the
catalog's implement-or-excuse surface coverage. The trigger payload MUST carry
only the preset name — argv, env, and cwd always resolve from the receiving
instance's own config, never from the wire.

#### Scenario: A name is not a command injection

- **WHEN** a client with the launch scope sends a launch-preset trigger whose
  name matches no configured preset
- **THEN** nothing is executed and the error names the unknown preset; no
  payload field can supply commands, env, or cwd
