# Chunk 2 — host drawer lifecycle, pooling, and keyboard picker

Commit subject (exact): `feat(drawer): add scoped occupant lifecycle and picker`

## Scope

Consume the core registry and extend the existing drawer state machine to one
visible occupant, arbitrary configured tools, worktree/global scope, persisted
occupant IDs, and a dedicated keyboard picker. Keep all cold resolution and
secret/env expansion off the event loop. Preserve local ephemeral PTY
ownership; do not route drawer panes through the daemon.

## Files touched

- `crates/thegn-host/src/drawer_state.rs` — replace bool-only state with
  occupant IDs, scope-aware pool/request keys, configured-command resolution,
  global/worktree reconciliation, legacy `true` decoding, and exit cleanup;
  reuse `contain_drawer_argv` and `spawn_argv_env_local`.
- `crates/thegn-host/src/handlers/drawer.rs` — new loop-side drawer handler
  context/drain and shared open/close/switch/cycle transitions extracted from
  `run.rs`; all loop work remains I/O-free.
- `crates/thegn-host/src/handlers/mod.rs` — register the drawer handler.
- `crates/thegn-host/src/palette.rs` — build the dedicated drawer picker with
  `drawer:<occupant-id>` pending-selection keys; keep main palette rows
  action-only.
- `crates/thegn-host/src/keymap.rs` — add `DrawerCycle` and `DrawerPick`
  actions, key serialization/parser aliases, and no-default-chord behavior.
- `crates/thegn-host/src/keymap_specs.rs` — add palette-visible action specs,
  keywords, and help-facing labels; use empty default chord arrays to avoid a
  global collision.
- `crates/thegn-host/src/agent_output.rs` — initialize new `NamedCommand`
  fields in host fixtures.
- `crates/thegn-host/src/agent_tests.rs` — initialize new fields in test
  constructors.
- `crates/thegn-host/src/daemon/agent_open.rs` — initialize new fields in
  config-generated tool records.
- `crates/thegn-host/src/handlers/launch.rs` — initialize new fields in launch
  records.
- `crates/thegn-host/src/handlers/worktree_launch.rs` — initialize new fields
  in worktree launch records.
- `crates/thegn-host/src/merge_driver.rs` — initialize new fields in merge
  driver fixtures.

## Approach and invariants

Use a pool key `(scope-key, occupant-id)`: the existing slugged worktree key
for worktree scope and one global sentinel for global scope. Persist worktree
records under the existing drawer state directory and the global record under a
separate global slot. Decode legacy `true` as `files`, retain `false` as
closed, and write occupant IDs only after updating the in-memory cache.

The active worktree's open worktree occupant takes precedence on switch;
otherwise an open global occupant resumes. Switching stashes the outgoing
pane, and cycling/picking uses the same transition path so state and pooling
cannot diverge. A stale async result must match both key components before it
opens. Process exit removes the pane and clears the corresponding persisted
occupant.

Configured commands use the existing `NamedCommand.command`/`env`,
`crate::panes::tool_drawer_argv`, existing `expand_env_ref` off-loop, and
the existing containment wrapper. A configured tool never becomes a
file-manager provider. Missing/dangling entries degrade to an omitted picker
row; a command that exits closes only its own occupant.

The handler API should be consumable by chunk 3's `run.rs` integration without
putting new registry policy back into the god file. Do not add a daemon session,
DB migration, repo-local command loading, or a live-state invocation.

## Overlap/dependency

Depends on chunk 1's core types and config schema; run serially after chunk 1.
It is file-disjoint from chunk 3: chunk 3 owns `run.rs` integration and chrome.
Chunk 3 must follow this chunk because it wires these handler/palette/action
APIs into the loop. The host `NamedCommand` constructor files listed above are
owned here so chunk 1 and chunk 2 do not overlap.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host drawer_state`
- `cargo nextest run -p thegn-host drawer`
- `cargo nextest run -p thegn-host palette`
- `cargo nextest run -p thegn-host keymap`

Use temporary state paths for any test that opens state; do not run the built
binary, a migration, e2e, or a full-workspace build.

## Done criteria

- Files remains occupant zero; arbitrary eligible `[[tools]]` entries can be
  cycled/picked without vendor branches.
- Worktree and global pools are isolated by key; global is reused across live
  worktree switches but is not daemon-persistent across detach/quit.
- Legacy flags, off-loop persistence, bounded eviction, async deduplication,
  containment, and process-exit cleanup are covered by focused tests.
- `drawer-cycle` and `drawer-pick` are parsed, palette-visible, and routed
  through the dedicated picker gate without weakening the main palette's
  action-row invariant.
- Help/env/completion/control ratchet impact is reported to chunk 3; no
  completion or control entries are added here.
- `git diff --check` passes.
- Commit exactly as: `feat(drawer): add scoped occupant lifecycle and picker`.
