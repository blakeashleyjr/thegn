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

### Requirement: Numeric duration policy has unit-aware supported bounds

Configuration schema and strict validation SHALL share supported maxima for
numeric durations: 31 days for periodic cadences and ten 365-day years for other
durations, expressed in each field's documented unit. Existing feature-specific
floors and zero/None disable or inheritance semantics SHALL remain intact.
Epoch timestamps MUST NOT be interpreted as durations.

#### Scenario: An explicit duration override exceeds its supported range

- **WHEN** a CLI, environment, profile, or authored config change introduces an out-of-range duration
- **THEN** its explicit layer or write is rejected before publication
- **AND** an existing config file retains its prior bytes on write rejection

#### Scenario: A permissive base file contains an invalid duration and security policy

- **WHEN** the base file successfully parses but one duration is outside its supported range
- **THEN** the runtime retains the parsed security settings and diagnoses the duration
- **AND** safe consumer conversion or destructive-policy quarantine handles that value without whole-file default fallback

### Requirement: Shared refresh cadence conversion cannot corrupt unrelated schedules

Every enabled periodic cadence SHALL resolve through a checked, nonzero slot
conversion without intermediate overflow. Nonintegral slot boundaries SHALL
round up. Disabled schedules SHALL remain explicit and MUST NOT use zero as a
modulo divisor. A shared ticker panic SHALL emit a worker-failure error and an
existing terminal wake without introducing idle polling.

#### Scenario: Adversarial cadence bypasses strict validation

- **WHEN** programmatic input supplies a cadence of 2^61 seconds or u64::MAX
- **THEN** runtime conversion produces a bounded nonzero schedule with identical debug and release semantics
- **AND** unrelated periodic schedules retain their own due decisions

#### Scenario: The shared refresh worker unwinds

- **WHEN** the worker panics
- **THEN** it publishes one explicit failure diagnostic and pulses the existing waker
- **AND** ordinary worker shutdown emits neither a failure diagnostic nor an extra wake

#### Scenario: The running worker receives hostile enabled cadences

- **WHEN** the production thread is started with each cadence family set to 2^61 or u64::MAX through normal config projections
- **THEN** later Model, Pr, HostHeal, and MainRefMoved requests still arrive through its output channel
- **AND** disabled optional schedules emit no requests
- **AND** the integration fixture retains, closes, and joins its worker using bounded clock permits and completion receipts

### Requirement: Static CLI output bypasses configured startup

The host SHALL classify parsed commands by borrowed exhaustive intent before startup migration and profile reroot. Config path/schema, API list/coverage/schema, and shell completion registration SHALL print through their existing static sources without loading or installing effective configuration, opening a controller, or creating startup state. All other commands SHALL retain their existing configured route in this change.

#### Scenario: Unread configuration does not block static output

- **WHEN** an existing static command is invoked with a missing, malformed or unread FIFO config path and invalid config override text
- **THEN** it exits successfully and prints its existing output without evaluating that configuration

#### Scenario: Migration and profile canaries survive static output

- **WHEN** private legacy config/state roots exist and a named profile is selected by flag or environment
- **THEN** static output leaves those roots and bytes unchanged and creates no profile, database, log or migration marker

#### Scenario: Output compatibility survives early dispatch

- **WHEN** config path uses an explicit relative path, completions use the tg executable alias, or stdout closes early
- **THEN** paths retain their spelling, registrations target the invoked alias, and closed stdout remains a successful best-effort output

#### Scenario: Configured siblings do not gain static authority

- **WHEN** config get, API call, automation test, Open with no-launch, Integrate with dry-run, Setup or no command is classified
- **THEN** it does not take the static return and existing configured behavior is preserved

### Requirement: Configuration reload preserves observed connectivity

Installing network configuration SHALL preserve connectivity evidence and
recovery history. Changed thresholds SHALL apply without manufacturing network
success or failure. The state visible to hot readers SHALL match the observed
state machine after a policy installation.

#### Scenario: Reload while offline

- **WHEN** the application is offline and identical configuration is reloaded
- **THEN** normal network refreshes remain paused and the scheduled recovery
  probe remains available at its configured cadence

#### Scenario: Reload during a failure streak

- **WHEN** configuration is reloaded after failures below the offline threshold
- **THEN** the accumulated failures remain evidence for the next network result

### Requirement: Runtime diagnostics remain useful across reloads

The application SHALL bound memory used to suppress identical runtime config
warnings. Suppression SHALL distinguish actual diagnostic content and source
identity/content, allowing changed actionable diagnostics to appear. Strict
validation SHALL continue reporting the complete diagnostic set. Config log
reconciliation SHALL preserve default third-party warning suppression and SHALL
NOT override explicit user logging directives.

#### Scenario: Unchanged legacy configuration is loaded repeatedly

- **WHEN** background hydration reloads an unchanged source with legacy keys
- **THEN** repeated warnings from that source are suppressed while retained in
  the bounded recent-diagnostic cache

#### Scenario: Configuration introduces a different warning

- **WHEN** the source content or actionable diagnostic changes
- **THEN** the new warning is emitted rather than hidden by an earlier warning

#### Scenario: Log level reconciles without an explicit environment filter

- **WHEN** the configured file sink changes its level during startup
- **THEN** application records follow that level and the default suppression of
  third-party warnings remains active

### Requirement: Host augmentation preserves effective definition precedence

When augmenting configuration with captured persisted host definitions, the
system SHALL retain a same-name declarative host and SHALL synthesize a missing
selectable environment from that effective winning host. Both placement and SSH
settings SHALL come from the same winner. An explicitly configured environment
SHALL remain unchanged by augmentation.

#### Scenario: Declarative local host shadows persisted SSH

- **WHEN** a declared local host and a persisted SSH host have the same name
- **THEN** the synthesized environment SHALL resolve locally and SHALL NOT inherit the persisted SSH destination

#### Scenario: Declarative SSH host shadows another persisted reach

- **WHEN** a declared SSH host shadows a same-name persisted host
- **THEN** its synthesized environment SHALL resolve with the declared destination, port, transport, forwarding and connection settings

#### Scenario: Winning reach has no pane transport

- **WHEN** the effective host reach is Iroh or cloud
- **THEN** augmentation SHALL NOT synthesize an SSH or local pane environment from a losing persisted definition
- **AND** selecting that absent environment SHALL retain the resolver's existing unresolved-selection diagnostic

#### Scenario: Explicit environment takes precedence

- **WHEN** an environment already exists for the augmented host name
- **THEN** augmentation SHALL preserve that environment and its existing resolution semantics

#### Scenario: Unshadowed persisted host

- **WHEN** no declarative host or explicit environment shadows a captured persisted host
- **THEN** supported SSH/local pane environments SHALL use that persisted definition's placement and connection settings
- **AND** Iroh/cloud definitions SHALL retain their existing absence of synthesized pane environments

### Requirement: Strict persisted host-definition capture

The core HostStore SHALL offer a distinct bounded host-definition capture
operation on an already-authorized connection. It MUST validate supported schema
version, ordinary main table shape and raw rows in one read transaction and MUST
refuse malformed or excessive source rather than return partial successful data.
Its result MUST NOT represent launch permission, runtime containment or freshness
after the capture. Existing display readers SHALL preserve their compatibility.

#### Scenario: Malformed persisted definition

- **WHEN** a config-bearing row has invalid JSON, duplicate keys, unknown fields,
  invalid enum values, wrong SQL types, invalid UTF-8 or invalid/duplicate names
- **THEN** strict capture returns a bounded typed error without source contents
- **AND** the row is not silently omitted or coerced into a valid default host

#### Scenario: Malformed definition shadowed by declarative configuration

- **WHEN** a malformed persisted definition has the same name as a valid declarative Local host
- **THEN** actual checked-store capture still returns the typed source error for invalid JSON, enum spelling or unknown fields
- **AND** it does not yield a successful snapshot or modify the declarative configuration or persisted row
- **AND** this source refusal does not represent a completed final-composition or launch-admission boundary

#### Scenario: Concurrent writer

- **WHEN** another connection updates schema version or host definitions after
  the capture transaction's first schema read
- **THEN** the capture retains one coherent version/schema/data snapshot
- **AND** a later capture independently rechecks version and source

#### Scenario: Resource bounds and nullable inventory

- **WHEN** host inventory or raw definitions exceed fixed count/byte/schema/JSON bounds
- **THEN** capture refuses explicitly without truncating the source
- **AND** NULL definitions count as inventory but do not become user host definitions

#### Scenario: Caller transaction and connection policy

- **WHEN** the connection already has an active transaction
- **THEN** strict capture refuses without committing or rolling back caller state
- **AND** capture never opens/migrates a DB or changes connection-wide policy

### Requirement: Checked host composition before returning configuration data

The core SHALL offer an additive checked operation over caller-owned layered
Config and a strict HostDefinitionsSnapshot, plus a HostStore capture wrapper.
The wrapper MUST capture all source rows before composition. Composition MUST
use the existing effective winning-host merger and validate the admitted final
configuration with the existing project schema and semantic rules before
returning immutable configuration data. Legacy loaders SHALL remain unchanged.

#### Scenario: Effective winner and explicit environment

- **WHEN** valid persisted SSH shares a name with declarative Local or SSH
- **THEN** the final host and synthesized environment use the declarative winner
- **AND** an explicitly configured environment is preserved in full
- **AND** actual environment resolution retains the existing effective placement

#### Scenario: Invalid source or final configuration

- **WHEN** source capture fails, including a malformed shadowed row, or final schema/semantic validation fails
- **THEN** the operation returns a fixed typed error and no HostComposedConfig
- **AND** returned errors and successful result Debug expose no config contents
- **AND** no provider, config loader, migration or secret resolution is invoked

#### Scenario: Bounded structural and effective-profile work

- **WHEN** pre/post composition exceeds the checked byte, structural, stage, profile, rule or effective-profile work limits
- **THEN** it returns Bounds before semantic validation or effective profile cloning
- **AND** each nonempty profile charges the original base clone even when replacing its rules
- **AND** inclusive at-limit positive fixtures retain valid existing behavior

#### Scenario: Existing semantic compatibility

- **WHEN** undefined host references, disabled subsystems or non-pane reaches are otherwise accepted by existing validation
- **THEN** checked composition retains those existing semantics without inventing stricter global rules
- **AND** legacy validation keeps all diagnostic messages in their original order
- **AND** checked validation stops after its first failed batch before cloning later profiles

#### Scenario: Scope of successful data

- **WHEN** checked composition succeeds
- **THEN** its result represents existing final configuration validity only
- **AND** first schema initialization may construct environment-derived defaults for metadata without installing them into the supplied Config
- **AND** launch adapter, freshness, opening policy and runtime containment remain separate obligations

### Requirement: Standalone host capture SHALL distinguish source absence from refusal

An internal capture operation SHALL return absence only for a genuinely missing
path component observed and reverified within its supported namespace, with no
orphaned sidecar beside a missing base. Existing unreadable,
unsupported, malformed or incompatible state SHALL produce a typed refusal,
never a successful empty registry. It SHALL NOT initialize or migrate state.

#### Scenario: Missing private state file

- **WHEN** inspection reaches a missing component in an otherwise supported namespace
- **THEN** capture reports absence without creating a file or parent directory.

#### Scenario: Absence changes before publication

- **WHEN** a retained ancestor changes or the first missing component is created before final verification
- **THEN** capture refuses the changed observation without reading SQL or publishing absence.

#### Scenario: Sidecar without a base

- **WHEN** the base is missing but an adjacent WAL, SHM or journal object exists at verification
- **THEN** capture returns a typed refusal without creating a base or deleting the orphan.

#### Scenario: Unreadable source

- **WHEN** a supported existing parent cannot be searched or its database cannot be read
- **THEN** capture returns a typed inspection/open refusal rather than absence, and a fresh capture can succeed after permissions are restored.

#### Scenario: Invalid existing state

- **WHEN** an existing DB contains incompatible version or malformed host definitions
- **THEN** capture returns a typed error without silently selecting a default registry.

### Requirement: Standalone capture SHALL preserve WAL reads and state its limits

The operation SHALL use normal read-only SQLite locking and current WAL state,
allowing SQLite-managed sidecar activity. It SHALL NOT substitute immutable or
procfd-only main-file reads. Platform observations SHALL be separate from launch
authorization and SHALL NOT claim hostile same-UID path replacement resistance.

#### Scenario: Uncheckpointed committed host definition

- **WHEN** a supported private WAL database has a committed uncheckpointed host row
- **THEN** capture reads it through the strict same-transaction snapshot operation.

#### Scenario: Unsupported or changed namespace

- **WHEN** Linux inspection observes a symlink, unsafe type or writable ancestor,
  unsupported filesystem, or identity change
- **THEN** capture refuses without falling back to a tolerant opener.

#### Scenario: Component not yet integrated

- **WHEN** only the standalone capture component is installed
- **THEN** it does not activate startup or launch authority, worker deadlines,
  global policy installation or receiver behavior.
