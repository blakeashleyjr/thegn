# config Specification

## Purpose

Configuration is a layered, tolerant, schema-driven surface: the Rust structs are the schema, the example file documents every key, environment overrides follow one naming rule and are complete by construction, unknown keys are reported by strict validation, and the home-manager module is checked against the same schema — so the three representations of the config can never drift apart.

## Requirements

### Requirement: Configuration layers in a fixed order

Configuration SHALL be resolved from built-in defaults, then
`$XDG_CONFIG_HOME/thegn/config.toml` (or `--config`, which changes the path but
not the TOML parser), then the active named profile's overlay
`profiles/<name>/config.toml` (absent for the default profile), then
`THEGN_<SECTION>_<KEY>` environment value overrides, then `--set key=value`.
A repository's selected `.thegn.*` overlay is a separate repo-scoped layer and
MAY carry `[sandbox]` (resolved through the trust clamp), `[keybinds]`,
`[notifications]`, `[issues]`, the `env` selector, and the metrics
detection/refusal table. A malformed or unknown value MUST warn and fall back
to the layer below — a launch is never blocked by configuration.

#### Scenario: Env beats file

- **WHEN** `config.toml` sets `base_branch = "main"` and `THEGN_BASE_BRANCH=develop` is set
- **THEN** the effective base branch is `develop`

#### Scenario: Profile overlay sits between file and env

- **WHEN** the shared `config.toml` sets `base_branch = "main"`, the active
  profile's overlay sets `base_branch = "trunk"`, and `THEGN_BASE_BRANCH` is
  unset
- **THEN** the effective base branch is `trunk`

### Requirement: The Rust structs are the schema

The `Config` struct tree SHALL derive `schemars::JsonSchema`; `thegn config schema`, strict validation, the MCP config resource and the test gates MUST consume that schema rather than a hand-maintained key list. Enumerated values MUST be declared with `config_enum!`, whose reserved marker and aliases are carried in the schema.

#### Scenario: A new enum is strict-checked by construction

- **WHEN** a `config_enum!` field is added to any config struct
- **THEN** `thegn config validate --strict` rejects an unknown value for it with no registration step

### Requirement: Every key is documented

`config/config.toml.example` SHALL document every section and key the schema defines (wildcard segments for map tables, array-of-tables for `Vec<struct>`), MUST parse as a `Config`, MUST validate clean, and is the source of the runtime-generated config-reference help page.

#### Scenario: Undocumented key fails the build

- **WHEN** a field is added to a config struct without a `# key = …` line in the example
- **THEN** the example-coverage test fails naming the section and key

### Requirement: Environment overrides are complete and exercised

Every top-level scalar key and every `section.key` one level deep SHALL either have a `THEGN_<SECTION>_<KEY>` override in `Config::env_overlay` or be pinned in `test/env-overlay-ratchet.txt` (shrink-only); every override that exists MUST be exercised by the env coverage unit test.

#### Scenario: New key without a knob

- **WHEN** a shallow key is added with neither an env line nor a ratchet entry
- **THEN** `env_overlay_coverage` fails naming the key

#### Scenario: Knob without a test

- **WHEN** an env line is added to `env_overlay` but not to `env_overlay_covers_every_knob`
- **THEN** the coverage test fails naming the knob

### Requirement: Unknown keys are reported by strict validation

`thegn config validate` SHALL strict-check every configuration layer it can
locate — the main file, the active profile overlay, and (when run inside a
repository or given `--repo <path>`) the repo `.thegn.*` overlay in whichever
of its supported formats it is written — reporting every key present in a
document but absent from that layer's schema as `path: unknown key`, with a
nearest-key hint when one is within two edits. Type failures MUST name the
dotted key and expected/actual type. Every problem MUST be prefixed with the
file that carries it. Map-valued tables accept any name; legacy sections the
loader already warns about are not double-reported; an absent layer is
skipped without comment. The exit code MUST be non-zero when any layer has a
problem. Syntax diagnostics MUST identify their source format. Lenient load
MUST keep dropping unknown keys with at most a warning.

#### Scenario: Typo'd key

- **WHEN** `[sandbox] enabeld = true` is validated in the main file
- **THEN** the report says `sandbox.enabeld: unknown key (did you mean `enabled`?)`

#### Scenario: Typo'd key in a repo overlay

- **WHEN** `thegn config validate` runs inside a repo whose `.thegn.toml`
  contains `[sandbox] enabeld = true`
- **THEN** the report names the `.thegn.toml` path alongside
  `sandbox.enabeld: unknown key` and the command exits non-zero

#### Scenario: Profile overlay is covered

- **WHEN** the active profile's `config.toml` overlay contains an unknown key
- **THEN** `thegn config validate` reports it prefixed with the overlay's path

#### Scenario: Type error names key and file

- **WHEN** the main file contains `[sandbox] enabled = "false"`
- **THEN** the report names the main file and `sandbox.enabled`, including the
  expected and actual types, and the command exits non-zero

### Requirement: The home-manager module derives from the schema

`nix/hm-module.nix` SHALL render only keys that exist in the `Config` schema and SHALL offer, for every `lib.types.enum` option, only values some `config_enum!` accepts (canonical or alias).

#### Scenario: Stale option

- **WHEN** the module renders a key the schema no longer has
- **THEN** `hm_module_drift` fails naming the key

### Requirement: Config command diagnostics carry source context

`thegn config get` and `thegn config set` SHALL include the effective config
path when reporting an unknown key, parse failure, or validation failure.
`config get --json` MUST continue to return the effective value with its real
type, and `config set` MUST retain its atomic rollback behavior.

#### Scenario: Set failure identifies file

- **WHEN** `thegn config set sandbox.enabled not-a-bool` would make the config
  invalid
- **THEN** the error names `sandbox.enabled` and the config file path, and the
  invalid value is not written

### Requirement: One configuration format per trust tier

The trusted layers (main file, profile overlay) SHALL be TOML only. The
repo-root overlay SHALL be read from `.thegn.toml`, `.thegn.yaml`,
`.thegn.yml`, or `.thegn.json` — in that precedence order, first existing
file wins. When more than one `.thegn.*` candidate exists in a repo root, the
load MUST warn once, naming the file used and the file(s) ignored, by path
only (never echoing file contents).

#### Scenario: A shadowed overlay is named

- **WHEN** a repo root contains both `.thegn.toml` and `.thegn.yaml`
- **THEN** the `.thegn.toml` is applied and a warning names `.thegn.yaml` as
  ignored

#### Scenario: YAML is not read at the trusted layers

- **WHEN** `$XDG_CONFIG_HOME/thegn/` contains a `config.yaml` and no
  `config.toml`
- **THEN** the load proceeds on defaults exactly as if no config file existed

### Requirement: Doctor reports configuration health

`thegn doctor` SHALL report the loaded config file's path and its
strict-validation problem count, plus the active profile and repo overlay
paths and counts when present, in both the text report and the JSON document,
pointing at `thegn config validate` for the detail. Doctor MUST NOT duplicate
the validation logic — it consumes the same collector and core validators the
`config validate` verb uses.

#### Scenario: A broken key surfaces in doctor

- **WHEN** the config file carries two strict-validation problems and
  `thegn doctor` runs
- **THEN** the report includes the config path with a problem count of 2 and
  names `thegn config validate` as the follow-up

#### Scenario: Doctor includes an active profile

- **WHEN** an active profile has a readable `profiles/<name>/config.toml`
- **THEN** doctor reports that profile path and its strict-validation problem
  count alongside the main configuration health

### Requirement: The pipeline stage chart is declarative, validated configuration

Configuration SHALL carry an optional multi-stage agent pipeline as an ordered
list of stages, each declaring a name, the agent that runs it, a prompt
template, a concurrency budget, an advisory timeout, an optional next stage, and
what to do when a stage worker blocks. The table SHALL default to empty, and an
absent table MUST behave exactly as an empty one.

The chart is **structure, not judgment**: thegn SHALL validate it and MAY
display it, and **no thegn code path may advance the next stage, enforce the
concurrency budget, or fire the timeout** — those fields are read by a
supervising agent, which resolves the whole table as one machine-readable
document. Removing the table MUST NOT change any behaviour other than what is
validated and displayed.

#### Scenario: An absent chart is inert

- **WHEN** a config file declares no pipeline stages
- **THEN** it validates clean, emits no warning, and every surface behaves as
  before

#### Scenario: The chart resolves as one document

- **WHEN** `thegn config get pipeline --json` runs against a configured chart
- **THEN** it emits the stage list as structured JSON — each stage's name,
  agent, prompt, concurrency, timeout, next and blocked-handling — including the
  defaults for keys the file omitted

### Requirement: A stage chart is strictly validated

Strict validation SHALL reject a chart that cannot be executed, reporting each
problem against the offending stage's index and name so the message points at a
line in the file. A stage MUST have a non-empty, unique name; MUST name an agent
that resolves either to a configured agent/tool entry or to a known coding-agent
harness id, on the same terms the agent-launch path accepts; MUST declare a
concurrency of at least one; and MUST NOT declare a next stage that names no
configured stage or that closes a cycle. Each cycle SHALL be reported once
rather than once per member.

A stage that no other stage's next edge reaches, and that is not the first
stage, SHALL raise a soft warning on the config-warning channel rather than an
error — it is reachable by explicit dispatch.

#### Scenario: An agent that names nothing launchable

- **WHEN** a stage's agent is a shell command line rather than a configured
  agent/tool name or a known harness id
- **THEN** validation fails naming that stage's index, name and the offending
  value

#### Scenario: A concurrency budget of zero

- **WHEN** a stage declares a concurrency of `0`
- **THEN** validation fails, because a stage that can never run is a typo rather
  than a way to disable one

#### Scenario: A cycle in the next edges

- **WHEN** three stages form a loop through their next edges
- **THEN** validation reports exactly one cycle error, naming the path, from the
  earliest-declared member

#### Scenario: An unreachable stage

- **WHEN** a stage is neither the first stage nor named by any other stage's next
  edge
- **THEN** the config load warns about it and nothing is blocked

### Requirement: Stage prompt templates are checked against a fixed variable set

Every stage's prompt template SHALL be checked at validation time against the
variables a stage worker's prompt may reference: everything an issue worker's
prompt may reference, plus the stage's own name, the artifact it writes, and the
artifact its parent stage wrote. An unknown placeholder MUST be a validation
error rather than an empty expansion at dispatch time.

thegn SHALL NOT render a stage prompt itself — the variable set exists to
validate the template, and substitution is the supervising agent's job.

#### Scenario: A typo in a stage prompt

- **WHEN** a stage's prompt references a placeholder that is not in the stage
  variable set
- **THEN** validation fails naming that stage's prompt key and the unknown
  placeholder

#### Scenario: Issue vocabulary stays valid for a stage

- **WHEN** a stage's prompt references the variables an issue worker's prompt
  uses
- **THEN** validation accepts them, because the stage variable set is a superset

### Requirement: Project spellings are canonical with bounded compatibility

Configuration SHALL document and emit `projects_dir`, `[project.<slug>]`,
`confirm_delete_project`, and `sidebar_project_sort` as canonical spellings.
Their workspace-named predecessors SHALL remain accepted with deprecation
warnings for three stable releases under a named removal policy. When both
forms are present, the canonical value MUST win and validation MUST identify
both locations. `THEGN_PROJECTS_DIR` and its legacy environment alias SHALL
follow the same rule. Home Manager SHALL expose a canonical project option and
retain a deprecated compatibility option during the window.

#### Scenario: Legacy config remains loadable

- **WHEN** a user loads a workspace-spelled key during the compatibility window
- **THEN** it retains its old behavior and a diagnostic names the canonical
  replacement and removal policy

#### Scenario: Canonical form wins a duplicate

- **WHEN** both `projects_dir` and `workspaces_dir` are set
- **THEN** `projects_dir` takes effect and validation reports both spellings

### Requirement: Provider and machine config terms remain distinct

Tracker-owned `workspace_id`, `workspace_slug`, and `project_id` fields and
internal/storage schema terms MUST NOT be rewritten by project-vocabulary
normalization.

#### Scenario: Tracker workspace id is configured

- **WHEN** a tracker account sets `workspace_id`
- **THEN** it parses unchanged and receives no thegn project-alias diagnostic
