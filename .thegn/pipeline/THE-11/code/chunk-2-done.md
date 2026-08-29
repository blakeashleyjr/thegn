# THE-11 chunk 2 completion

Implemented the host drawer lifecycle and picker chunk.

## Delivered

- Replaced the drawer cache value with persisted occupant IDs while decoding
  legacy `true` as `files` and `false` as closed; global state uses the fixed
  process-local global slot.
- Added `(scope-key, occupant-id)` pool keys, bounded eviction, global reuse,
  last-occupant toggling, worktree-over-global reconciliation, and process-exit
  cleanup.
- Added a scope-aware async registry spawner. Configured tools resolve cwd and
  `env:`/`file:` overlays off-loop, use `tool_drawer_argv`, common containment,
  and `spawn_argv_env_local`; stale results are dropped by both key components.
- Added the loop-side `handlers::drawer` context/drain and shared toggle,
  cycle, and selection transition entry points for chunk 3.
- Added `drawer-cycle` and `drawer-pick` actions, aliases, palette-visible
  action specs with no default chords, and the dedicated `drawer:<id>` picker
  row/key decoder. Existing main-palette rows remain action-only.
- Initialized the new `NamedCommand` fields in all host fixtures and
  config-generated records.

## Verification

- `cargo fmt --all` — passed.
- `git diff --check` — passed.
- `RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo nextest run -p thegn-core drawer`
  — 15 passed.
- No e2e, snapshot re-recording, built-binary invocation, migration, or live
  state-DB access was performed.

## Unverified

- `just quick thegn-host` and `cargo nextest run -p thegn-host drawer_state`
  reach the host compile but stop at the exhaustive `run.rs` action dispatch:
  chunk 3 owns that file and must add arms for `DrawerCycle` and `DrawerPick`.
  The host drawer/palette/keymap tests therefore could not run until that
  serial integration is applied.
- Completion-slot and control-schema ratchets remain unchanged; help and
  chrome/snapshot updates are owned by chunk 3.
