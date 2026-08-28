# THE-85 chunk 1 — attach-on-open: worktree tabs open onto their live daemon agent sessions

Design: `.thegn/pipeline/THE-85/architect/design.md` (D1, D2, D4, D5).
Branch `tg/the-85-attach-live-session`. Evidence line numbers in the design
were verified at `42984dc4`; re-check each site before editing.

## Files touched (exact paths)

- `crates/thegn-host/src/handlers/worktree_attach.rs` — **NEW** module
- `crates/thegn-host/src/handlers/mod.rs` — register it
- `crates/thegn-host/src/handlers/adopt.rs` — visibility/generalization of `graft`
- `crates/thegn-host/src/handlers/materialize.rs` — worker probes sessions, fills batch
- `crates/thegn-host/src/handlers/provision.rs` — `SpecBatch` tuple → struct + drain applies attach
- `crates/thegn-host/src/panes.rs` — `materialize_with_specs` gains the attach input
- `crates/thegn-host/src/run.rs` — prewarm worker probe + `MaterializeTx` rt + shell suppress
- `crates/thegn-host/src/agent.rs` — `launch_spec_center_with` (extras-taking variant)
- `crates/thegn-host/src/cmd/session.rs` — correct the stale `--adopt` help note

**Overlap:** file-disjoint from chunk 2 (`handlers/crash.rs`, `pty_drain.rs`)
— may run in parallel. Integrate chunk 1 first if serialized: chunk 2's status
wording assumes the attach path exists but does not depend on its code.

## Approach

Follow design §D1/D2/D4/D5. Summary of the mechanical steps:

1. **New module `handlers/worktree_attach.rs`** (mirror `handlers/adopt.rs`'s
   doc style — say what problem it closes and which door it reuses):
   - `pub(crate) struct AttachTarget { pub session: String, pub program: String }`.
   - `live_for_worktree(sessions: &[SessionInfo], worktree: &str, shown: &[String]) -> Vec<AttachTarget>` — **pure**:
     keep rows whose `worktree.as_deref() == Some(worktree)`,
     `exited_at_ms.is_none()`, and whose id is not in `shown`; sort newest
     first (`created_at_ms` desc); map to `AttachTarget`.
   - `plan(leaves: &[u32], targets: Vec<AttachTarget>, max_panes: usize) -> AttachPlan` — **pure**:
     zip newest-first targets onto `leaves` in order; the rest are `surplus`,
     capped so `existing + assignments + surplus <= max_panes`.
   - `async fn probe(rt: tokio::runtime::Handle, dcfg: &DaemonConfig, worktree: &str, shown: Vec<String>) -> Vec<AttachTarget>`:
     `crate::daemon::client::connect_daemon(dcfg)` (**never** `ensure_daemon` —
     a probe must not spawn a daemon), then `client.sessions()`, then the pure
     filter. Any error ⇒ empty vec (shells are the honest fallback; log at
     debug).
   - `graft_surplus(...)`: thin wrapper over the generalized `adopt::graft`
     (below) that splits each surplus session into tab `(gi, ti)` and sets
     `focused_pane`; returns how many landed.
   - Unit tests for the two pure functions (see Tests).
2. **`handlers/adopt.rs`**: change `graft`'s signature from
   `(sid, group: usize, …)` (active-tab-only, `adopt.rs:227`) to take
   `(gi, ti)` explicitly; `apply` passes `session.worktrees[group].active_tab`.
   Make it `pub(crate)`. No behavior change for the intent drain.
3. **`handlers/provision.rs`**: convert the `SpecBatch` tuple
   (`:15–21`) into a named struct
   `{ group, worktree, tab, origin, specs, attach: Vec<AttachTarget> }` and
   update `drain_specs` (`:364`) destructuring accordingly. After the
   `materialize_with_specs` call (`:520`) succeeds, if `!batch.attach` is
   empty run the **pure** plan (leaves were consumed by materialize — compute
   the plan BEFORE the call, pass assignments in, graft `surplus` after) so:
   assignments ride into `materialize_with_specs`, surplus becomes splits.
   `MAX_PANES` is `run.rs:18699` (16) — pass it through the ctx or reuse the
   const (move it to a shared spot only if trivial; otherwise pass as a param).
4. **`panes.rs`**: `materialize_with_specs` (`:724`) gains
   `attach: &[(u32, AttachTarget)]`. In the per-leaf loop, extend the existing
   daemon branch (`:761–800`): the session id is
   `tab.pane_sessions.get(old)` (persisted record, current behavior)
   `.or_else(|| attach lookup for old)`. To label the pane with the agent
   rather than the fallback shell, thread the target's `program` through
   `spawn_daemon_backed` (`:484`) as an optional label override of
   `crate::pane::program_name(argv)` (add a parameter; update its ~4 call
   sites). Keep the `set_fallback_restore` logic identical for both sources.
5. **`handlers/materialize.rs`**: `MaterializeTx` gains
   `rt: tokio::runtime::Handle`; capture `Handle::current()` in the caller
   (run.rs `:8002` construction site — the loop runs inside `rt.block_on`,
   `main.rs:939`, so this is infallible). In the worker after specs resolve —
   only for the non-terminal arm and only when
   `panes::Panes::daemon_route_enabled()`'s equivalent holds (daemon cfg
   present; the worker has `cfg` → `cfg.daemon`) —
   `rt.block_on(worktree_attach::probe(...))` with `shown` = sessions already
   announced by live panes is NOT knowable off-thread: pass an empty `shown`
   here and let the drain-side plan re-dedup against `panes.table` (cheap,
   correct). Put the targets into the batch.
   Also switch both `"shell"` resolutions (`:194`, `:222`) to
   `launch_spec_synced_with(..., LaunchExtras { suppress_agent_record: true,
..Default::default() })` (D4).
6. **`run.rs`**:
   - prewarm worker (`:7756–7800`): same probe (it already runs inside
     `spawn_blocking` with `cfg` cloned; capture the Handle before the
     closure), fill `attach` on its batch; switch its `launch_spec_synced`
     (`:7797`) to the suppress variant.
   - `spawn_worktree_shell_pane` (`run.rs:5021`, spec at `:5059`): use the new
     `launch_spec_center_with` with `suppress_agent_record: true` (D4 — the
     split/new-pane shell is not an agent choice either).
   - `MaterializeTx` construction site gets `rt`.
   - Update the two `SpecBatch` send sites to the struct form.
     Keep run.rs additions inside the existing blocks (ratchet: do not grow new
     top-level items).
7. **`agent.rs`**: add `launch_spec_center_with(cfg, worktree, branch, choice,
extras)` next to `launch_spec_center` (`:2980`), which forwards to
   `launch_spec_full`; keep `launch_spec_center` as the
   `LaunchExtras::default()` wrapper (other callers unchanged).
8. **`cmd/session.rs`**: replace the `--adopt` NOTE (`:49–53`) with the real
   contract: the intent is consumed by a running compositor (granted
   immediately when the worktree is open here, `handlers/adopt.rs`; otherwise
   the session attaches when the worktree is opened).

## Invariants to respect (from CLAUDE.md / ARCHITECTURE.md)

- The probe is **off-loop** (`spawn_blocking` + `block_on`), like the spec
  resolve it sits beside; nothing new polls or blocks on the loop; no new
  channel/wake source (the batch already rides `spec_tx` + waker).
- `render_plan::plan` inputs unchanged: attach = pane spawn + `need_relayout`
  (splits) exactly like today's materialize spawns.
- No color/glyph literals, no `#[cfg]` outside `platform/`, no new ignored
  `Result`s without a `// best-effort:` comment (probe failure → empty vec +
  debug log; send failures keep the existing best-effort comments).
- `thegn-core` untouched (no substrate/coverage-gate impact).
- No new `ACTION_SPECS` action ⇒ no help-page/ratchet work in this chunk.

## Tests to run (scoped — do NOT run workspace-wide gates while iterating)

```sh
just quick thegn-host
cargo nextest run -p thegn-host worktree_attach
cargo nextest run -p thegn-host handlers::adopt
cargo nextest run -p thegn-host handlers::provision
cargo nextest run -p thegn-host materialize
cargo nextest run -p thegn-host agent::   # suppress_agent_record pins
```

New tests to write:

- `worktree_attach`: `live_for_worktree` filters exited/tombstone rows
  (`exited_at_ms` set), matches the worktree, sorts newest-first, drops
  already-shown ids; `plan` assigns newest→first leaf, caps surplus at
  `max_panes`, handles empty leaves/targets.
- `agent_tests`: `launch_spec_full` with `suppress_agent_record: true` and
  choice `"shell"` does NOT write `worktrees.agent` (register the worktree row
  first — `set_worktree_agent` is UPDATE-only, see `agent_tests.rs:436`);
  without the flag it still does (pin the old behavior).
- Existing suites must stay green: `handlers::adopt` plan tests,
  `provision.rs:688–751` daemon-disabled claim tests (they construct
  `SpecBatch` — the struct conversion updates them), `panes.rs` materialize
  tests.

## Done criteria

- [ ] `cargo nextest run -p thegn-host` passes for every filter listed above.
- [ ] `just quick thegn-host` clean (clippy `-D warnings`, fmt).
- [ ] Opening a worktree whose path has a live `session open --agent …`
      session attaches the pane to it (manual check: `thegn session open …`
      then click the row in `just start name=dev`; the agent's streaming
      output is visible and typable) — verify via `thegn session list`
      `attached_clients >= 1` while the tab is open.
- [ ] A worktree with two live sessions opens with a split; the older session
      is the split; a third beyond the cap shows in the status line.
- [ ] A daemon-disabled (`THEGN_NO_DAEMON=1`) launch spawns shells exactly as
      before (probe skipped, no behavior change).
- [ ] Opening a worktree whose wizard/`--bind`-recorded agent is `X` no longer
      rewrites `worktrees.agent` to `shell` (DB check after open).
- [ ] `--adopt` on a resident group still grafts immediately; the CLI help no
      longer claims the intent is unconsumed.
- [ ] Commit with EXACTLY this subject (conventional, branch-only):

```
feat(host): open worktree tabs onto their live daemon agent sessions (THE-85)
```
