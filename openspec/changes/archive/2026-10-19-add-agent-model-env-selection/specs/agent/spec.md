## ADDED Requirements

### Requirement: An agent entry declares its harness, model, env overlay and permissions

An `[[agents]]`/`[[tools]]` entry MAY carry `model`, `env` and `permissions`;
`harness` SHALL name the harness that shapes the launch (precedence over `provider`; both set and disagreeing is a validation error). At every launch of the
entry thegn SHALL append the model through the harness's model flag, apply the
env overlay last with secret expansion, and seed the permissions into the
harness's per-worktree allow-list when the harness has one.

#### Scenario: A model rides on the harness flag

- WHEN an entry `command = "codex"` has `model = "o3"`
- THEN the interactive launch is `codex -m o3` and the headless launch is `codex exec <prompt> -m o3`

#### Scenario: A model on a flagless harness is a config error

- WHEN an entry whose harness has no model flag sets `model`
- THEN `thegn config validate` reports it and the model is never silently dropped

#### Scenario: The env overlay wins and never exports a literal secret ref

- WHEN an entry sets `env = { CLAUDE_CONFIG_DIR = "file:/p" }`
- THEN the launched process sees `CLAUDE_CONFIG_DIR` set to the file's contents, or unset (with a warning) when the file cannot be read

### Requirement: A pipeline stage overrides model, env and permissions

A `[[pipeline.stages]]` entry MAY set `model`, `env` and `permissions`. `thegn
session open --stage <name>` (and `AgentLaunch.stage`) SHALL layer them over
the agent entry: `model` replaces, `env` overlays key by key, a non-empty
`permissions` replaces. An unknown stage SHALL be an error.

#### Scenario: One entry, two tiers

- WHEN `coder` has `model = "a"` and stage `review` has `model = "b"`
- THEN `session open --agent coder` runs with `a` and `session open --agent coder --stage review` runs with `b`

### Requirement: pi is a launchable harness

The closed harness registry SHALL include `pi` with headless form `pi -p
{prompt}` and model flag `--model {model}`, no credential-home projection.

#### Scenario: A bare `pi` launch

- WHEN `session open --agent pi --prompt "x"` runs with no `[[agents]]` entry named `pi`
- THEN the daemon runs `pi -p 'x'`

### Requirement: The daemon honours registry changes without a restart

For each agent launch the daemon SHALL reload the config from the process's
recorded source (CLI overrides + path), falling back to its startup snapshot
only when the reload fails.

#### Scenario: A new entry is usable immediately

- WHEN an operator adds `[[agents]] name = "pipeline-pi"` while the daemon runs
- THEN the next `session open --agent pipeline-pi` resolves it
