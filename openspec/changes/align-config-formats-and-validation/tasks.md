# Tasks

The repo already provided tri-format repo parsing, profile loading, trust
clamping, example coverage, generated-reference registration, home-manager
drift checking, and the associated ratchets. These tasks cover the audit gaps
and the documentation/spec synchronization only.

## 1. Core validation and reference coverage (`thegn-core`) — complete

- [x] Add `config_repo.rs` with `OverlayFormat`, candidate discovery,
      TOML > YAML > YML > JSON precedence, path-only shadow diagnostics, tolerant
      winner loading, and the existing trust-clamp/metrics-refusal behavior.
- [x] Generalize the schema walker while retaining `validate_str` for
      `Config`; expose structured `validate_repo_overlay(body, format)`
      diagnostics for the real `RepoConfigFile` schema. Cover valid documents,
      syntax errors, unknown keys/hints, dotted type errors, dynamic maps, and
      shadowed candidates with core unit tests.
- [x] Strengthen generated config-reference coverage from the schema/example
      key set and describe example values, without adding a config key or a
      defaults-accuracy comparison.

## 2. Host validation and diagnostics (`thegn-host`) — complete

- [x] Add `config validate --repo <path>` and the `config_health` collector;
      validate main TOML, active profile TOML, and the selected repo overlay in
      order, prefix findings with their owning path, skip absent optional layers,
      and return non-zero for strict problems.
- [x] Add effective config-path context to `config get/set` while preserving
      typed JSON output and atomic rollback. Make doctor consume the same collector
      in text, JSON, and bundles, including paths, counts, warnings, and the
      `thegn config validate` follow-up without changing doctor exit policy.
- [x] Register `config validate --repo` as a structural path-completion slot.
      Do not change completion allowlists, env-overlay/home-manager/help ratchets,
      config-enum counts, control capabilities, or control-schema snapshots.

## 3. Documentation and OpenSpec synchronization — complete

- [x] Update architecture §7 and `docs/help/configuration.md` with the
      trusted TOML-only contract, active profile order, actual repo tables and
      precedence, tolerant loading, strict validation, shadow warnings, and
      example-value wording. Do not document a separate lenient validation flag.
- [x] Synchronize this proposal, design, tasks, and config delta with the
      landed core/host APIs. Keep the curated `config/config.toml.example` and
      canonical `openspec/specs/config/spec.md` unchanged.

## 4. Scoped verification — complete

- [x] Run `just quick thegn-core` and
      `cargo nextest run -p thegn-core config_reference`.
- [x] Run `just quick thegn-host` and
      `cargo nextest run -p thegn-host help`.
- [x] Run `just openspec-validate` and `git diff --check`.

Full-workspace `just test`, `just ci`, coverage, and e2e are intentionally not
part of this change's per-chunk verification; they remain pre-PR gates.
