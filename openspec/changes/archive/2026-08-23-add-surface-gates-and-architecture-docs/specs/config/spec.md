## ADDED Requirements

### Requirement: Configuration layers in a fixed order

Configuration SHALL be resolved from built-in defaults, then `$XDG_CONFIG_HOME/thegn/config.toml` (or `--config`), then `THEGN_<SECTION>_<KEY>` environment overrides, then `--set key=value`; a repository's `.thegn.toml` MUST overlay only `[sandbox]`. A malformed or unknown value MUST warn and fall back to the layer below — a launch is never blocked by configuration.

#### Scenario: Env beats file

- **WHEN** `config.toml` sets `base_branch = "main"` and `THEGN_BASE_BRANCH=develop` is set
- **THEN** the effective base branch is `develop`

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

`thegn config validate --strict` SHALL report every key present in the document but absent from the schema as `path: unknown key`, with a nearest-key hint when one is within two edits; map-valued tables accept any name; legacy sections the loader already warns about are not double-reported. Lenient load MUST keep dropping unknown keys with at most a warning.

#### Scenario: Typo'd key

- **WHEN** `[sandbox] enabeld = true` is validated strictly
- **THEN** the report says `sandbox.enabeld: unknown key (did you mean `enabled`?)`

### Requirement: The home-manager module derives from the schema

`nix/hm-module.nix` SHALL render only keys that exist in the `Config` schema and SHALL offer, for every `lib.types.enum` option, only values some `config_enum!` accepts (canonical or alias).

#### Scenario: Stale option

- **WHEN** the module renders a key the schema no longer has
- **THEN** `hm_module_drift` fails naming the key
