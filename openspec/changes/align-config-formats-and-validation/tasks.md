# Tasks

## 1. Core: overlay validation + shadow warning (thegn-core)

- [ ] 1.1 Generalize the strict-validation walk in `config_validate.rs` to take
      a schema root parameter; keep `validate_str` byte-compatible for the
      `Config` path — **unit tests** (existing suite must pass unchanged).
- [ ] 1.2 `validate_repo_overlay_str(body, format) -> Vec<String>` over
      `schema_for!(RepoConfigFile)`, parsing TOML/YAML/JSON to the shared
      value shape; expose a `pub` entry point — **unit tests**: typo'd
      `[sandbox]` key with hint, unknown top-level table, all three formats,
      syntax-error message per format (95% line gate applies).
- [ ] 1.3 `load_repo_overlay` / `repo_overlay_parse_error`: detect multiple
      `.thegn.*` candidates and `config_warn` naming the winner + ignored
      paths (paths only, never contents) — **unit test** with two files in a
      temp dir.

## 2. Host: multi-layer `config validate` + doctor line (thegn-host)

- [ ] 2.1 `cmd/config.rs::validate`: iterate main file → active profile
      overlay (`Config::profile_overlay_path`) → repo overlay (cwd or
      `--repo`); prefix every problem with its file path; non-zero exit on
      any problem; silent skip for absent layers. Add `--repo <path>` to
      `Action::Validate`.
- [ ] 2.2 `cmd/doctor.rs`: `config_health` in the JSON document + one text
      line (path, problem count, repo overlay when present), reusing the core
      validators.

## 3. Docs + spec text

- [ ] 3.1 `docs/help/configuration.md`: reconcile the two contradictory
      repo-overlay paragraphs to the real table set; delete all three
      `--strict` mentions; document tri-format precedence + the shadow
      warning. (Generated pages need nothing; no new action ids, so the help
      ratchets are unaffected. If any e2e snapshot renders this page,
      re-record with `just e2e-update` and review the diff.)
- [ ] 3.2 Confirm `config/config.toml.example` needs no change (no new keys
      are added by this change).

## 4. Validation

- [ ] 4.1 Run `just ci` once, when the implementation is complete (includes
      `openspec validate --all --strict`).
