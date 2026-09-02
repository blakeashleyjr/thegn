# Chunk 1 — core CI-log contract, cache, config, and catalog

## Files touched

- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/ci_log.rs` (new)
- `crates/thegn-core/src/config_ci.rs`
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-core/src/config_tests.rs`
- `crates/thegn-core/src/db.rs`
- `crates/thegn-core/src/db_migrate.rs`
- `crates/thegn-core/src/db_ci.rs` (new)
- `crates/thegn-core/src/db_workspace.rs`
- `crates/thegn-core/src/store/cache.rs`
- `crates/thegn-core/src/capability.rs`
- `crates/thegn-core/src/control.rs`
- `crates/thegn-core/src/mcp/state.rs`
- `config/config.toml.example`
- `docs/help/configuration.md`
- `test/env-overlay-ratchet.txt`

## Approach

1. Add the pure, substrate-free `ci_log` module with bounded UTF-8 tailing,
   hard byte cap, secret redaction, metadata, dedupe, and retention helpers.
   Unit-test every secret shape, truncation boundary, and idempotence property.
2. Extend `CiConfig` with documented cache policy and the default-off autofix
   mode. Add a trusted workspace overlay plus `repo_ci` resolver; keep
   repo-authored `.thegn.*` from enabling autofix. Validate ranges and templates
   without adding a new task kind or AI dependency. Pin non-env-settable new
   keys in the env-overlay ratchet with reasons.
3. Add the additive v62 `ci_log_cache`/handoff-dedupe schema, verifier, migration
   regression test, cache implementation sibling, object-safe store methods, and
   worktree cleanup. Cache failures remain best effort.
4. Add `CiRuns`/`CiLogs` to `Verb`, `ALL`, scope mapping, and the single catalog
   with only HTTP/CLI/MCP surfaces. Add parameterized `ci_runs`/`ci_logs` specs
   to the existing MCP router. No gRPC/proto or plugin gap is created.
5. Update config/help docs in the same commit. Do not regenerate unrelated
   artifacts.

## Overlap and dependency

No file overlap with chunks 2 or 3. This chunk is first and chunks 2 and 3 are
serially dependent on its core types, store methods, catalog IDs, verbs, and
MCP specs. The coder owns the core files listed here only.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core ci_log`
- `cargo nextest run -p thegn-core db_migrate`
- `cargo nextest run -p thegn-core capability`
- `cargo nextest run -p thegn-core env_overlay_coverage`
- `cargo nextest run -p thegn-core config_example`
- `cargo nextest run -p thegn-core completion`

Do not run `just test`, `just ci`, a full-workspace compile, e2e, a migration,
or the built binary. If a manual binary invocation is unavoidable, set
`XDG_STATE_HOME` to a fresh temporary directory first.

## Done criteria

- Core has no tokio, rusqlite, subprocess, provider, or terminal dependency in
  `ci_log.rs`; pure tests cover all new policy.
- v62 is additive/idempotent, verified before stamping, preserves pre-v62 rows,
  and deletes log rows with a deleted worktree.
- All stored/exposed log text is bounded and redacted; `redacted` and
  `truncated` are explicit metadata.
- `ci.runs` and `ci.logs` each have exactly one catalog row, correct read scope,
  exact MCP specs, and no accidental gRPC/plugin promise.
- Config example/help and the env-overlay ratchet pass; no stale ratchet entry
  remains.
- Commit exactly as: `feat(the-48): add CI log core contract and catalog`
