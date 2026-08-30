# THE-38 chunk 1 completion

Implemented and committed core configuration validation and generated-reference coverage.

## Changes

- Added `config_repo` with the TOML/YAML/JSON repo-overlay format API, candidate discovery, precedence/shadow diagnostics, tolerant loading, and format-neutral parsing.
- Shared the schema walker between trusted `Config` validation and repo overlays; repo diagnostics now include format, dotted path, expected type, and actual type while retaining nearest-key hints and legacy-key tolerance.
- Preserved repo trust clamping, metrics command-collector refusal, TOML > YAML > YML > JSON precedence, and malformed-overlay fallback behavior.
- Updated config-reference wording to describe example values and added schema/example key coverage tests in core and integration tests.

## Verification

- `just quick thegn-core`
- `cargo nextest run -p thegn-core config_validate repo_overlay config_example config_reference env_overlay_coverage hm_module_drift`
- `git diff --check`

All scoped checks passed. The filtered nextest run executed 25 tests with 3591 tests skipped.

## Unverified

Per the chunk policy, no full-workspace `just test`, `just ci`, coverage, or e2e run was performed.

## Commit

`709bcdf3 feat(the-38): validate all config documents`
