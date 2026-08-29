# Chunk 1 — core merge-queue view policy and workspace projection

## Files touched

- `crates/thegn-core/src/merge_queue_view.rs` (new)
- `crates/thegn-core/src/lib.rs`
- `crates/thegn-host/src/sidebar.rs`

Do not touch rendering, input handlers, statusbar composition, help pages, or
config files in this chunk.

## Approach

Add a substrate-free `thegn_core::merge_queue_view` module with unit-tested
`MqTier`, `MqRollup`, `rollup(statuses)`, `MqTokenFit`, and `fit_token(...)`.
Use the existing `attention::MqStatus` vocabulary and priority exactly as
specified in `architect/design.md`; `Landed` is silent and unknown strings are
already filtered by `MqStatus::parse` at the host seam. Core must not import
termwiz, host `Seg`/`Tok`, terminal capabilities, or vendor/provider code.

Add `SidebarRow::mq_rollup: Option<MqRollup>` with the same defaulting pattern
as the other derived row fields. During the existing `build_rows` denormalizing
pass, preserve `mq_status` on worktree rows and compute a workspace rollup from
that workspace's child worktree rows keyed by `workspace_slug`. Do not use
`SidebarStatus::repo_scope` as membership; it is an active attention scope.
The projection must include dormant and collapsed workspaces because their
children are still emitted before visibility filtering.

## Dependencies / overlap

This is the first chunk and has no dependency on another chunk. Chunks 2 and 3
depend on the public core types and `SidebarRow::mq_rollup`, so they run after
this commit. Files are disjoint from both later chunks.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core merge_queue_view`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host sidebar::tests`

If the host filter is narrower after implementation, use the exact new
workspace-rollup test module name and retain the package-scoped command shape.
Do not run e2e or a full-workspace build.

## Done criteria

- Core tests pin all tier/status/count and full→marker→hidden width boundaries.
- Workspace rows carry a rollup; worktree rows retain their existing status;
  empty/landed-only workspaces carry `None`; no cross-workspace queue leakage.
- `thegn-core` remains substrate-free and no ratchet allowlist grows.
- The coder commits early and finishes with this exact commit subject:
  `feat(the-9): add merge queue token policy`
