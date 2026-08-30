# THE-55 chunk 1 completion

Implemented the substrate-free core session migration policy and two-store DB
seam.

- Added typed allowlisted carriers for worktrees, groups/tabs, sidebar state,
  dispatches/notes, pin state, plans, conflicts, liveness facts, fingerprints,
  import results, and cleanup counts.
- Added exact active-session/worktree selection, segment-boundary sidebar
  matching, strict group/UI/pin conflicts, target-owned worktree handling,
  target-first resume detection, stable sanitized fingerprints, deterministic
  dispatch parent/note remapping, and daemon/pane ID clearing at import.
- Added the synchronous `SessionMigrationStore` seam and explicit v61 SQLite
  snapshot/import/read-back/cleanup implementation. Source cleanup is exact-row
  cleanup and does not use the broad worktree cascade.
- Added path-only existing-target profile resolution without rerooting or
  applying target credentials.
- Registered `Verb::MigrateSession` as Admin and CLI-only, plus the two
  completion catalog slots. No control route, API call, config key, palette
  action, schema snapshot, or completion ratchet entry was added.

## Verification

- `XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp RUSTC_WRAPPER= just quick thegn-core`
- `cargo nextest run -p thegn-core session_migration` (7 passed)
- `cargo nextest run -p thegn-core verb_scope_table_is_exhaustive` (passed)
- `cargo nextest run -p thegn-core every_verb_has_exactly_one_row` (passed)
- `cargo nextest run -p thegn-core completion` (42 passed)
- `cargo nextest run -p thegn-core resolves_existing_target_without_rerooting_or_credentials` (passed)

## Unverified

- Full workspace gates (`just test`, `just lint`, `just ci`, coverage, smoke,
  and e2e) were not run per the chunk/dev-loop policy.
- Host CLI/daemon orchestration, help ratchets, and the unchanged control
  schema snapshot remain for chunk 2.
