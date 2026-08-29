# Chunk 2 — service reads, cache transport, scans, and creation lifecycle

Commit subject (exact): `feat(the-32): wire submodule git seam and lifecycle`

## Files touched

- `crates/thegn-svc/src/git/submodule.rs` (new)
- `crates/thegn-svc/src/git/mod.rs`
- `crates/thegn-svc/src/git/plumbing.rs`
- `crates/thegn-host/src/glyph_types.rs` (new)
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/glyph_refresh.rs`
- `crates/thegn-host/src/warmcache.rs`
- `crates/thegn-host/src/sidebar.rs`
- `crates/thegn-host/src/loc_scan.rs`
- `crates/thegn-host/src/measure/loc.rs`
- `crates/thegn-host/src/measure/disk.rs`
- `crates/thegn-core/src/disk.rs`
- `crates/thegn-host/src/git_worktree.rs` (new)
- `crates/thegn-host/src/cmd/wt.rs`
- `crates/thegn-host/src/wizard.rs`
- `crates/thegn-host/src/handlers/tracker.rs`
- `crates/thegn-host/src/daemon/service.rs`
- `crates/thegn-host/src/workspace_create.rs`
- `crates/thegn-host/src/remote_sync.rs`
- `crates/thegn-host/src/agent.rs`
- `crates/thegn-core/src/remote.rs`

Chunk 1 is a prerequisite because this chunk consumes its types/config. Chunk
3 is serial after this chunk because it consumes the hydrated sidebar/change
payloads. The file set is disjoint from chunks 1 and 3; do not “clean up” the
panel or merge files here.

## Approach

1. Add the service submodule module and defaulted `GitBackend` operations for
   state, raw gitlink diffs, bounded local summaries, initialization, and
   conflict metadata. CLI/bridge calls use argv and existing scrubbed/bounded
   execution. `GixGit` delegates to the CLI fallback. Extend the existing
   glyph batch rather than spawning a per-field process. Empty/missing
   `.gitmodules`, `off`, unavailable objects, and provider failures are
   independent `Result` degradations.
2. Replace the persisted positional glyph tuple with a named cache record in
   `glyph_types.rs`. Deserialize both the legacy eight-element array and the
   new named record; default only the new submodule field. Update all
   merge/cache/persist/seed sites atomically, retaining last-known-good values
   on partial read failure and the existing TTL/active cadence. Publish through
   the existing channel and pulse `TerminalWaker`.
3. Add the submodule bit to `GitGlyphs` and expose its raw data to chunk 3’s
   display model. Add boundary-aware LOC scanning that excludes normalized
   submodule descendants. Keep disk’s physical-byte total inclusive exactly
   once and ensure submodule directories never become synthetic worktree/cache
   children. Add fixture tests for a populated nested repo and registered
   nested worktrees, including the distinction between disk bytes and LOC.
4. Add `git_worktree.rs` as the shared host-side creation pipeline and route
   existing `wt new`, TUI, daemon, and tracker creation through it. Existing
   core path/branch policy may remain, but new submodule commands and any
   touched submodule lifecycle operation must go through `GitBackend`; do not
   add core shelling. Initialization is recursive, off-loop, progress-bearing,
   and non-fatal after the worktree exists. Preserve `wt new` stdout as one
   path and send progress/failures to stderr.
5. Extend local clone, provider provisioning, and bundle/remote materialization
   with the same effective `SubmoduleMode`. Keep remote script generation pure
   and shell-quoted; execution remains in the host/provider runner. Reuse the
   repo-trust approval store with a distinct normalized URL/path request. A
   denial leaves a usable checkout and a visible notice. Never enable
   `protocol.file.allow` in production.

## Tests to run

Fixture rules: `protocol.file.allow=always` and `commit.gpgsign=false` only for
test repositories; set `XDG_STATE_HOME` to a fresh temp directory for any DB
or CLI invocation.

- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc submodule`
- `cargo nextest run -p thegn-svc plumbing`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host glyph_scan`
- `cargo nextest run -p thegn-host measure`
- `cargo nextest run -p thegn-host worktree`
- `cargo nextest run -p thegn-host workspace_create`

Do not run `just test`, `just ci`, a full compile, e2e, migration, or the built
binary.

## Done criteria

- Every new git operation is observable in the service/host seam; no new core
  subprocess or loop-side blocking call exists.
- Bridge/local/gix fallback reads are batched, independently degradable, and
  pulse the waker through the existing workers.
- Legacy glyph cache rows load without a migration and new rows preserve the
  submodule field; failed reads do not fabricate clean state.
- LOC excludes submodule source while disk counts physical bytes once; tests
  lock the accounting policy and the existing nested-worktree arithmetic.
- All four local creation callers share the same post-add lifecycle, and clone
  / provider / bundle paths honor `auto` versus `off`, trust, recursive init,
  non-fatal errors, and stdout/stderr contracts.
- Relevant env-overlay and help/config generation checks are updated in the
  same commit; control/completion snapshots remain unchanged.
- `git diff --check` is clean.
- Commit exactly as `feat(the-32): wire submodule git seam and lifecycle`.
