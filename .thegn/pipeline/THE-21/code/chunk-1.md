# THE-21 chunk 1 — pure core contract, config, and audit state

## Scope

Build the substrate-free automation contract and its trusted configuration and
storage seams. This chunk is foundational and must land before chunk 2.

## Files touched

- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/automation.rs` (new)
- `crates/thegn-core/src/config_automations.rs` (new)
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-core/src/notification.rs`
- `crates/thegn-core/src/db.rs`
- `crates/thegn-core/src/db_migrate.rs`
- `crates/thegn-core/src/db_automation.rs` (new)
- `crates/thegn-core/src/store/mod.rs`
- `crates/thegn-core/src/store/automation.rs` (new)
- `config/config.toml.example`

Do not touch catalog/control surfaces, host/service runtime, CLI, HTTP/gRPC/MCP
adapters, or ratchet snapshots owned by chunk 2.

## Approach

1. Add `thegn_core::automation` with typed normalized events, predicates,
   action plans, loop origin, injected-time matching, debounce/once-per-key,
   bounded rate limits, and explicit skip reasons. Keep it free of DB, tokio,
   filesystem, terminal, process, and provider dependencies. Unit-test every
   policy branch and deterministic ordering.
2. Add the config model/validator in its own module. Use `[automations]` for
   bounded runtime settings and `[[automations.rules]]` for the requested
   event→action rules; the draft’s simultaneous `[automations]` +
   `[[automations]]` spelling is invalid TOML. Rules are global/profile only,
   bounded, exactly one event plus one catalog action, and reject invalid
   globs/templates/limits. Add repo-overlay detection/warning without adding an
   `automations` field to `RepoConfigFile`; repo rule content must never enter
   effective config. Document every key in `config/config.toml.example`.
3. Add object-safe automation state/audit store traits and SQLite implementation
   in new modules. Add additive schema v62 tables `automation_state` and
   `automation_runs`, indexes, and bounded-retention query primitives. Preserve
   DB cache semantics and do not run a migration in this chunk.
4. Add `automation` and `automation_failed` notification kinds, updating all
   exhaustive kind lists/labels/priorities and core count/config-help tests.
5. Leave catalog ids as typed strings/validated references for chunk 2 to
   register. Do not add pin/lifecycle pseudo-actions or a generic invoke escape
   hatch here.

## Dependency/overlap

Chunk 2 depends on these types, config fields, DB/store methods, notification
kinds, and catalog ids. Chunk 2 must run serially after this chunk. Files are
otherwise disjoint: chunk 2 owns host/service adapters and snapshots.

## Tests to run

From the worktree, without invoking the built binary against live state:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core automation`
- `cargo nextest run -p thegn-core notification_kind`

If a focused migration/store filter is needed, use
`cargo nextest run -p thegn-core db` rather than a workspace build. No
`just test`, `just ci`, full-workspace compile, migration command, or e2e.

## Done criteria

- Core matching is pure, deterministic, unit-tested, and covers matching,
  debounce, once-per-key, rate limits, and origin loop prevention.
- Config validation rejects untrusted repo automations and unsafe/ambiguous
  rules; all new config keys are in the example and ready for chunk 2’s
  env-overlay ratchet update.
- v62 schema/store design is additive and bounded; no live DB was migrated.
- Core notification exhaustive tests pass; catalog registration is chunk 2.
- Commit exactly as: `feat(the-21): add pure automation rule engine`.
