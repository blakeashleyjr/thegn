# THE-38 chunk 2 completion

Implemented and committed host configuration health and CLI diagnostics.

## Changes

- Added `config validate --repo <path>` and a host-side collector covering the
  selected main TOML, active external profile TOML, and selected repo overlay.
  Diagnostics are file-prefixed; repo validation uses the core-selected winner,
  reports path-only shadow warnings, and preserves tolerant loading.
- Added effective config-path context to `config get` unknown-key errors and
  `config set` parse/type/validation failures without changing successful typed
  JSON output or rollback behavior.
- Added shared config-health paths/counts/detail guidance to doctor text and
  JSON, and carried the same context into doctor bundles without changing the
  doctor's exit policy.
- Registered `config validate --repo` as a structural path-completion slot.
- No control capability/schema, config key, completion allowlist, or other
  ratchet entries were added; env-overlay, home-manager, and config-enum
  surfaces remain unchanged from chunk 1.

## Verification

- `just quick thegn-host`
- `cargo nextest run -p thegn-host config`
- `cargo nextest run -p thegn-host doctor`
- `cargo nextest run -p thegn-host help`
- `cargo nextest run -p thegn-core completion`
- `cargo nextest run -p thegn-core config_validate`
- `git diff --check`

All scoped checks passed. The targeted host filters ran 76, 19, and 75 tests;
the core filters ran 42 and 15 tests. The quick check included the applicable
completion/help/control-schema ratchets, and no control-schema snapshot
changed.

## Unverified

Per the chunk policy, no full-workspace `just test`, `just ci`, coverage, or e2e
run was performed.

## Commits

- `31dbc000 feat(the-38): surface config health in CLI diagnostics`
- Completion summary commit follows this record.
