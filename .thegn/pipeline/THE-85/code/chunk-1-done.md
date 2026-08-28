# THE-85 chunk 1 — completion summary

Branch `tg/the-85-attach-live-session`. Implemented per `.thegn/pipeline/THE-85/code/chunk-1.md`
(design D1/D2/D4/D5).

## What landed

- **New `crates/thegn-host/src/handlers/worktree_attach.rs`** — attach-on-open
  decision logic, registered in `handlers/mod.rs`:
  - `AttachTarget { session, program }` and `AttachPlan { assignments, surplus }`.
  - `live_for_worktree` (pure): keeps rows whose worktree matches and
    `exited_at_ms.is_none()`, drops ids already `shown`, sorts newest-first
    (`created_at_ms` desc), maps to `AttachTarget`.
  - `plan` (pure): zips newest-first targets onto the missing leaves; the rest
    become `surplus`, truncated to `max_new_panes` (the caller passes the tab's
    remaining headroom under `MAX_PANES_PER_TAB = 16`, so
    existing + assignments + surplus ≤ 16).
  - `probe`: async daemon read via `connect_daemon` — **never** `ensure_daemon` —
    then `sessions()` then the pure filter; any error ⇒ empty vec + debug log.
  - `graft_surplus`: thin wrapper over the generalized `adopt::graft`, newest
    first, returns how many landed.
- **`handlers/adopt.rs`**: `graft` is `pub(crate)`, takes `(gi, ti)` explicitly
  (clamped like `active_tab_mut`); `apply` passes the group's active tab.
  No behavior change for the intent drain.
- **`handlers/provision.rs`**: `SpecBatch` tuple → named struct
  `{ group, worktree, tab, origin, specs, attach }`; `drain_specs` destructures
  accordingly. Before `materialize_with_specs` it re-dedups the batch's targets
  against live `panes.table` daemon sessions, plans onto the leaves that will
  actually spawn (excluding leaves with a persisted daemon record — those
  reattach their own prior session), and after a successful materialize grafts
  `surplus` into the batch's tab; the overflow is named in the status line
  ("N more live agent session(s) — `thegn attach <id>`", active tab only).
- **`panes.rs`**: `materialize_with_specs` gains
  `attach: &[(u32, AttachTarget)]`; the daemon branch now takes the session id
  from the persisted record `.or_else(attach lookup)`, with `set_fallback_restore`
  logic identical for both sources. `spawn_daemon_backed` gains an optional
  `label` override of `pane::program_name(argv)` (empty labels ignored);
  attach-sourced panes are labeled with the daemon-recorded agent program.
- **`handlers/materialize.rs`**: `MaterializeTx` gains `rt: tokio::runtime::Handle`
  (captured via `Handle::current()` at the run.rs construction site — the loop
  runs inside `rt.block_on`). The non-terminal worker arm probes after specs
  resolve (connect-only, empty `shown`; drain re-dedups) and ships targets on the
  batch. Both `"shell"` resolutions now use `launch_spec_synced_with(...,
LaunchExtras { suppress_agent_record: true, .. })` (D4).
- **`run.rs`**: prewarm worker does the same probe (Handle captured before the
  `spawn_blocking`, skipped for terminal groups / daemon-off) and fills `attach`
  on its struct batch; its shell resolve also suppresses the agent record;
  `spawn_worktree_shell_pane` uses the new `launch_spec_center_with` with
  `suppress_agent_record: true` (D4). Additions stay inside the existing
  prewarm/materialize blocks.
- **`agent.rs`**: `launch_spec_center_with(cfg, wt, branch, choice, extras)`;
  `launch_spec_center` remains the `LaunchExtras::default()` wrapper.
- **`cmd/session.rs`**: the stale `--adopt` NOTE replaced with the real contract
  (grafted immediately when the worktree is open; attaches on open otherwise).
- **`direnv_warm.rs`**: removed the now-unused `launch_spec_synced` default-extras
  wrapper (its only callers moved to the `_with` variant; dead code would fail
  clippy `-D warnings`); doc links in `agent.rs` updated to the `_with` variant.

## Tests

- New: `worktree_attach` — 6 tests (worktree/exited/shown filtering, newest-first
  sort; plan assignment order, surplus cap, empty leaves/targets/zero budget).
- New: `agent_tests::shell_materialize_with_suppressed_record_leaves_the_worktrees_agent_alone`
  — `launch_spec_full` with `suppress_agent_record: true` and choice `"shell"`
  does NOT rewrite `worktrees.agent` (worktree row registered first, since
  `set_worktree_agent` is UPDATE-only); without the flag `"shell"` still records
  (pinned).
- Updated: the two `provision.rs` daemon-disabled tests construct `SpecBatch` as
  the struct; three `panes.rs` materialize tests pass the new `attach: &[]` param.
- Scoped verification (dev-loop policy, no workspace-wide gates):
  - `just quick thegn-host` — clean (clippy `-D warnings`, lib/bin).
  - `cargo clippy -p thegn-host --all-targets -- -D warnings` — clean (covers the
    touched test code; `just lint`'s clippy line is the same command workspace-wide).
  - `cargo nextest run -p thegn-host worktree_attach` — 6/6.
  - `cargo nextest run -p thegn-host handlers::adopt handlers::provision materialize` — 17/17.
  - `cargo nextest run -p thegn-host agent::` — 32/32 (includes the new pin).
  - `cargo nextest run -p thegn-host panes::tests::materialize` — pass.
  - rustfmt (treefmt's rustfmt config: `skip_children=true --edition 2024`) run
    over every touched file.

## Deviations from the chunk spec (deliberate, minimal)

- `probe` does **not** take the `tokio::runtime::Handle` parameter the spec
  listed: `probe` is awaited via `rt.block_on(probe(...))` at the call site (the
  handle riding `MaterializeTx` / the prewarm closure), so a handle parameter
  would be dead. The spec's own call-site description (`rt.block_on(
worktree_attach::probe(...))`) matches this shape.
- `direnv_warm::launch_spec_synced` (default-extras wrapper) was deleted rather
  than left unused — with both callers moved to `launch_spec_synced_with` it
  became dead code, which `clippy -D warnings` rejects. Its doc contract moved
  onto `launch_spec_synced_with`.

## Unverified

- Manual/interactive done-criteria were NOT exercised here (no live daemon UI
  run from this chunk stage): attaching to a real `thegn session open --agent …`
  session (streaming output visible + typable, `attached_clients >= 1`), the
  two-session split / third-beyond-cap status line, and the DB check after
  opening a `--bind`-recorded worktree. The pure/plan/struct/test surfaces
  covering them are unit-tested as listed above.
- e2e not run (per lead addendum; no frame-affecting chrome change is expected —
  attach is pane spawns via the existing materialize path).
- `just test` / `just ci` (full-workspace) not run — pre-push gate territory.
- The runtime probe path (`rt.block_on` inside `spawn_blocking`) is compile- and
  clippy-verified but not executed in a test; it mirrors `handle_session_fallback`'s
  daemon-read pattern and the design's off-loop contract (§0.6).
- Surplus-split panes are labeled by the fallback argv (adopt::graft's existing
  behavior, unchanged here); only the primary attach leaf carries the agent's
  program label. Cosmetic follow-up if the review wants labels on splits too.
