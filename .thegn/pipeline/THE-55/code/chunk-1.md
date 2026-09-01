# Chunk 1 — core migration policy, DB seam, and catalogs

## Scope

Implement the substrate-free migration model and the two-store DB seam. This chunk owns selection, allowlisted row carriers, conflict rules, deterministic dispatch ID remapping, fingerprints, target-profile path resolution, capability registration, and completion classification. It must not add a CLI, daemon call, palette action, config key, or control route.

## Files touched

- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/profile.rs`
- `crates/thegn-core/src/control.rs`
- `crates/thegn-core/src/capability.rs`
- `crates/thegn-core/src/completion/catalog.rs`
- `crates/thegn-core/src/store/mod.rs`
- `crates/thegn-core/src/session_migration.rs` (new)
- `crates/thegn-core/src/store/session_migration.rs` (new)
- `crates/thegn-core/src/db_session_migration.rs` (new)

Do not modify `docs/api/control-v1.json` or `test/completion-slot-ratchet.txt`: this CLI-only capability has no control wire delta, and both new value slots must be classified rather than pinned. If a test proves a ratchet text change is necessary, stop and resolve the catalog classification instead of adding debt.

## Approach

1. Add a public, substrate-free `session_migration` module containing typed carriers for the selected worktree, groups/tabs, sidebar UI keys, dispatch rows/notes, source/target profile names, liveness inputs, conflict decisions, import result, and audit counts. Keep opaque JSON/text as opaque strings; never parse pane trees or command output in core.
2. Implement pure policy functions:
   - select by exact worktree path and active session;
   - include all matching groups and their tabs;
   - include only segment-anchored sidebar `collapse:`/`pin:`/`pin_ordinal:` keys for selected groups;
   - include only `session_state.pin_state` while excluding `active_tab`, global layouts/caches, live attention, and credentials by construction;
   - reject differing group/UI conflicts, accept identical prior imports as resume, and leave an existing target worktree row target-owned;
   - allocate dispatch IDs in ascending source-ID order, map notes and in-set parents, clear out-of-set parents and all daemon `session_id`s, and clear pane-session maps;
   - generate a stable sanitized fingerprint without exposing row contents.
3. Add a path-only helper in `profile.rs` that resolves an existing target profile from the active profile's base without calling `reroot` or applying credential environment. Reject missing and normalized source==target cases. Preserve existing profile lock semantics.
4. Add a sync `SessionMigrationStore` seam under `store/`; implement it for `Db` in the new sibling module, following the repository rule that DB query/transaction code lives outside `db.rs`. Expose source snapshot, target preflight/import, target read-back, and source cleanup operations. Each write operation is one SQLite transaction; there is no pretend cross-DB transaction.
5. Use explicit current v61 column lists. Import dispatches and notes in separate parent/child order, preserve artifact/chunk paths and reports/notes, merge only `session_state.pin_state` without clobbering target `active_tab`, and never query credential/account/token tables. Source cleanup must be exact-row cleanup and must not call broad worktree deletion.
6. Register `Verb::MigrateSession` in `Verb::ALL`, `required_scope` as `Admin`, the `sessions.migrate` CLI-only capability row, and the completion catalog rows `(session move, worktree)=Worktree` and `(session move, to_profile)=Profile`. Keep `API_CALLS` and the control schema untouched.

## Tests to add/run

Tests must be deterministic and offline. Core migration tests use `Db::open_memory()` for source and target and seed the relevant rows directly; do not use the host or a live daemon.

- Pure policy tests: exact-path selection, multiple groups, segment-boundary UI keys (`api` does not match `api-v2`), pin-state-only session merge, excluded whole-session/global/credential tables, strict group/UI/pin conflicts, target-worktree-wins, identical-import resume, and deterministic fingerprints.
- In-memory DB tests: target transaction writes all included rows, preserves target active tab while importing pin state, clears pane/session IDs, remaps dispatch IDs and parent/note references, read-back fingerprint matches, source cleanup is exact, and a repeated import is idempotent.
- Profile tests: existing default/named target resolution, missing target, normalization, and source==target rejection without changing process environment.
- Catalog/control tests: exhaustive verb scope table, capability catalog coverage, and CLI-only surface classification.

Run only:

```text
just quick thegn-core
cargo nextest run -p thegn-core session_migration
cargo nextest run -p thegn-core verb_scope_table_is_exhaustive
cargo nextest run -p thegn-core completion
```

The exact test filter names may be qualified to the module if the crate reports duplicate names. Do not run a migration or the built binary. Do not run `just test`, `just ci`, a full workspace compile, or e2e.

## Dependency/overlap

No dependency on chunk 2 at implementation time. Chunk 2 depends on the public types and `SessionMigrationStore` API from this chunk, so Lead must serialize the chunks: land chunk 1 first, then rebase/compile chunk 2. Within the requested chunk split, files are disjoint: chunk 2 must not edit any file listed here.

## Done criteria

- All policy and in-memory DB tests above pass with the scoped commands.
- Core has no imports from `thegn-host`, `thegn-svc`, tokio, terminal, or renderer code; no live daemon/session process is created.
- The implementation copies only the allowlisted rows and never reads/copies credentials, config overlays, or profile-global state; `session_state.pin_state` is the sole allowed session-state field.
- Dispatch ID remapping, parent handling, note mapping, pane/session clearing, strict conflicts, resume fingerprint, and exact source cleanup are covered by tests.
- `sessions.migrate` is Admin and CLI-only; no control route/API call/snapshot change exists.
- Completion catalog and help/control ratchet checks have no newly pinned debt.
- Commit exactly as:

```text
feat(core): add session profile migration policy and store
```
