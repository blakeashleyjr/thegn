# THE-23 chunk 1 completion

Implemented the core devcontainer contract for selection, parsing, inventory,
substitution policy, backend honorability, configuration, detection probes, and
provider pane CPU-cap wrapping. Added primary, JSONC, and variant fixtures.

## Verification

- `cargo check -p thegn-core --lib` passed with `RUSTC_WRAPPER=` and temporary runtime/build directories.
- `cargo clippy -p thegn-core -- -D warnings` passed with `RUSTC_WRAPPER=`.
- `cargo nextest run -p thegn-core devcontainer`: 59 passed.
- `cargo nextest run -p thegn-core config::tests::env_overlay`: 1 passed.
- `cargo nextest run -p thegn-core --test env_overlay_coverage every_`: 2 passed.
- Completion-slot, help-ratchet, and control-schema snapshot checks passed: 5 host tests and 1 service test.
- Read-only comparison confirmed the protected ratchet/snapshot files are byte-identical.

## Unverified

- `just quick thegn-core` could not run in this environment: the initial invocation could not use the runtime directory, and the temporary-directory retry was blocked by `sccache: Operation not permitted`. Equivalent direct core check and clippy commands passed.
- Full-workspace gates, migrations, live-state binary invocations, and e2e tests were not run per the dev-loop policy.
