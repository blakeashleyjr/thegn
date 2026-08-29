# Tasks

## 1. Core: overlay validation, shadow warning, and reference drift (thegn-core)

- [ ] 1.1 Extract the repo candidate/parser seam to `config_repo.rs`; retain
      TOML > YAML > YML > JSON precedence, trust clamps, metrics refusal, and
      tolerant loading. Add a path-only shadow warning, deduplicated for one
      load — **unit tests** for precedence and two candidates.
- [ ] 1.2 Generalize the strict-validation walk to a schema root/value shape;
      keep `validate_str` for `Config`, and expose
      `validate_repo_overlay_str(body, format)` over `RepoConfigFile` —
      **unit tests** for valid TOML/YAML/JSON, syntax errors, unknown keys with
      hints, dotted type errors, and dynamic maps (95% line gate applies).
- [ ] 1.3 Add generated config-reference coverage derived from the schema and
      shipped example; change wording from exact code defaults to example
      values. Do not add a default-value heuristic or a config key.

## 2. Host: multi-layer validation, context, and doctor health (thegn-host)

- [ ] 2.1 Add `--repo <path>` and a host `config_health` collector. Iterate
      main TOML → active profile TOML → selected repo overlay; prefix every
      finding with its path, return non-zero for any finding, and silently skip
      absent optional layers.
- [ ] 2.2 Make `config get/set` include effective config-path context without
      changing typed JSON or atomic rollback. Make `doctor` consume the same
      collector in text and JSON, including profile/repo counts and the
      `thegn config validate` follow-up. Preserve bundle JSON behavior.
- [ ] 2.3 Register `config validate --repo` in the single completion catalog
      as structural path completion. Do not change the completion ratchet,
      env-overlay ratchet, control schema, help ratchets, or config enum count.

## 3. Docs + spec text

- [ ] 3.1 Update §7 of `docs/ARCHITECTURE.md` and
      `docs/help/configuration.md`: trusted TOML-only contract, active profile,
      actual repo tables/formats, tolerant load, strict validation, and
      example-value wording; remove every nonexistent `--strict` claim.
- [ ] 3.2 Synchronize this proposal, design, tasks, and config spec. Confirm
      `config/config.toml.example` needs no change because this change adds no
      key and deliberately does not auto-generate the curated example.

## 4. Validation

- [ ] 4.1 Per chunk, run the scoped `just quick <crate>` and
      `cargo nextest run -p <crate> <filter>` commands in its chunk spec;
      run `just openspec-validate` for the synchronized draft. Do not run
      full-workspace CI or e2e as part of this change.
