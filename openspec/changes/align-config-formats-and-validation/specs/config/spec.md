# config

## MODIFIED Requirements

### Requirement: Configuration layers in a fixed order

Configuration SHALL be resolved from built-in defaults, then
`$XDG_CONFIG_HOME/thegn/config.toml` (or `--config`), then the active named
profile's overlay `profiles/<name>/config.toml` (absent for the default
profile), then `THEGN_<SECTION>_<KEY>` environment overrides, then
`--set key=value`. A repository's selected `.thegn.*` overlay is a separate
repo-scoped layer and MAY carry `[sandbox]` (resolved through the trust clamp),
`[keybinds]`, `[notifications]`, `[issues]`, the `env` selector, and the
metrics detection/refusal table. A malformed or unknown value MUST warn and
fall back to the layer below — a launch is never blocked by configuration.

#### Scenario: Env beats file

- **WHEN** `config.toml` sets `base_branch = "main"` and `THEGN_BASE_BRANCH=develop` is set
- **THEN** the effective base branch is `develop`

#### Scenario: Profile overlay sits between file and env

- **WHEN** the shared `config.toml` sets `base_branch = "main"`, the active
  profile's overlay sets `base_branch = "trunk"`, and `THEGN_BASE_BRANCH` is
  unset
- **THEN** the effective base branch is `trunk`

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
problem. Lenient load MUST keep dropping unknown keys with at most a warning.

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

## ADDED Requirements

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
strict-validation problem count (and the repo overlay's, when run inside a
repository), in both the text report and the JSON document, pointing at
`thegn config validate` for the detail. Doctor MUST NOT duplicate the
validation logic — it consumes the same core validators the `config validate`
verb uses.

#### Scenario: A broken key surfaces in doctor

- **WHEN** the config file carries two strict-validation problems and
  `thegn doctor` runs
- **THEN** the report includes the config path with a problem count of 2 and
  names `thegn config validate` as the follow-up
