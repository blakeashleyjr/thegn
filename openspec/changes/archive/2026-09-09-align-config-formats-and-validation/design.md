# Design — settle config formats, close validation blind spots

## Context

The config surface already has a schemars schema, strict validation for the
trusted TOML document, bidirectional example coverage, profile loading,
tri-format repo parsing, trust clamping, home-manager drift checks, and
runtime-generated config-reference registration. THE-38 adds the missing
shared repo-overlay validation, shadow visibility, layer aggregation, source
context, and generated-reference key assertion without adding a format
selector or a new config key.

## Format decision

| Layer                              | Trust           | Format(s)               | Contract                                    |
| ---------------------------------- | --------------- | ----------------------- | ------------------------------------------- |
| defaults                           | code            | —                       | Rust defaults                               |
| `config.toml` / `--config`         | user            | TOML                    | `--config` changes the path, not the parser |
| `profiles/<name>/config.toml`      | user            | TOML                    | active external profile overlay             |
| `THEGN_*` / `--set`                | user            | values / TOML fragments | overlays, not document readers              |
| repo `.thegn.{toml,yaml,yml,json}` | repo, untrusted | TOML, YAML, JSON        | TOML > YAML > YML > JSON                    |

Format tolerance belongs only at the repo edge. Trusted config writes,
example coverage, runtime reference generation, home-manager output, and
live reload are TOML-native; widening trusted readers would create parallel
paths without a demonstrated need.

## Core validation substrate

`crates/thegn-core/src/config_repo.rs` owns the repo candidate seam. Its
`OverlayFormat` identifies TOML, YAML, or JSON; `discover_repo_overlay` returns
all readable candidates in precedence order; `selected` is the winner and
`shadowed` exposes the rest. The tolerant loader keeps the winner-only
behavior: a malformed winner is ignored rather than falling through. A
path-only shadow warning is deduplicated for repeated loads of the same
candidate set.

`parse_overlay_value(body, format)` adapts each document to a
`serde_json::Value`. `validate_repo_overlay(body, format)` returns structured
`RepoOverlayDiagnostic` values, including the source format, dotted path, and
message. It invokes the shared schema walker with the actual
`RepoConfigFile` schema. `validate_str` remains the trusted `Config` entry
point and retains its semantic checks. The walker reports syntax, unknown
keys with nearest-key hints, expected/actual types, and enum problems while
accepting intentionally dynamic maps and avoiding duplicate reports for
legacy keys.

The repo schema is the real `RepoConfigFile`: trust-clamped `[sandbox]`,
`[keybinds]`, `[notifications]`, `[issues]`, the `env` selector, and the
metrics detection/refusal table. No repo authority is widened by validation.

## Host layer collection

`crates/thegn-host/src/cmd/config_health.rs` owns synchronous filesystem and
repository-path work. `collect` checks, in order:

1. the selected main TOML;
2. the active external profile TOML, when the non-default profile file exists;
3. the selected repo candidate, when cwd or `--repo` resolves to a repository.

Each finding carries its owning path. Missing optional layers are silent.
`config validate` renders findings and returns non-zero for strict problems;
repo shadow notices remain warnings. `config get` and `config set` include the
effective config path in relevant diagnostics while preserving typed JSON and
atomic rollback.

## Doctor health

Doctor consumes the same `ConfigHealth` collector rather than reimplementing
validation. Text reports include main/profile/repo paths, problem and warning
counts, and the follow-up `thegn config validate`. JSON places the same
information under `config_health`, with `main_path`, `profile_path`,
`repo_path`, `problem_count`, `warning_count`, `validate_command`, and per-layer
`path`/`problems` objects. The bundle reuses that JSON. Doctor remains a
synchronous diagnostic-only CLI path and does not alter exit policy.

## Documentation and ratchets

Architecture §7 and the configuration help page describe trusted TOML-only
layers, the active profile, repo precedence and tables, tolerant loading,
strict validation, and example-value wording. No separate lenient validation
flag is implied.
No new action, help page, config key, capability, control-schema row, or
ratchet entry is required. `config validate --repo` is a structural completion
value slot, owned by clap path completion.

The change does not touch render damage (`Skip`/`Panes`/`Full`), the compositor
event loop, SQLite schema, or the canonical OpenSpec spec.

## Alternatives and security

- A lenient-by-default validation flag was rejected: validate is already the
  explicit strict check, while loading remains tolerant.
- Validating every overlay during launch was rejected: it adds startup work and
  would conflict with the launch-never-blocked contract; doctor is the
  opt-in health view.
- A `[config] format` knob and trusted auto-detected YAML/JSON were rejected
  because they would multiply the trusted write and gate paths.

Repo files are untrusted. Diagnostics echo paths and schema-derived hints, not
file contents; shadow warnings are path-only. Existing trust clamping,
metrics command-collector refusal, and secret handling remain unchanged.

## Deferred question

Whether commented example values should be compared heuristically with code
defaults remains deferred because those values are intentionally illustrative.
