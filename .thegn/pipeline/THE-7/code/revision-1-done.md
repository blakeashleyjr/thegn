# THE-7 revision 1 completion

## Changes

- Resolved user themes through the live catalog at startup, reload, builder preview, and cycle selection; built-in names remain authoritative and configuration overrides layer on top.
- Preserved config comments and unrelated settings when Apply writes a selection, and persisted only fields intentionally edited in the builder.
- Made the builder a bounded popup with adaptive editor/preview layout; Apply and preview remain visible and non-overlapping at 80x24 and larger sizes.
- Debounced watcher changes without dropping queued Save, Import, or Apply requests.
- Rejected control, bidi, and other nonprinting theme names in the core import/user-theme paths.
- Reported deterministic built-in/user collisions with the source path and theme name in store and CLI flows.
- Updated the OpenSpec change artifacts to the approved Gogh-only, no-export scope.
- Added targeted regression coverage for resolution/reload, selective persistence, popup geometry, debounce ordering, unsafe names, and collision warnings.

## Verification

- `cargo fmt --all -- --check` — passed.
- `just quick thegn-host` — passed.
- `cargo nextest run -p thegn-core theme` — 53 passed.
- `cargo nextest run -p thegn-host theme_builder` — 6 passed.
- `cargo nextest run -p thegn-host theme_store` — 3 passed.
- `cargo clippy -p thegn-host --tests -- -D warnings` — passed.

## Unverified

- Direct `treefmt` could not run because `shfmt` is unavailable in the environment; the repository pre-commit `treefmt` hook passed for the committed changes, and Rust formatting passed separately.
- `openspec validate --all --strict` could not run because the `openspec` executable is unavailable.
- No e2e test or live-state database invocation was run.

## Disputed

None.

## Commits

- `af0da850` — runtime user-theme resolution.
- `3f745603` — override-preserving Apply and popup/store behavior.
- `6b7fa51f` — unsafe theme-name rejection.
- `599b182c` — compact apply events, CLI simplification, and OpenSpec alignment.
