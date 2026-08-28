# THE-85 chunk 2 — an attached agent's exit leaves a screen + relaunch offer, never a silent shell

Design: `.thegn/pipeline/THE-85/architect/design.md` (D3).
Branch `tg/the-85-attach-live-session`. Evidence line numbers verified at
`42984dc4`; re-check each site before editing.

## Files touched (exact paths)

- `crates/thegn-host/src/handlers/crash.rs` — `prep_leaf_for_respawn` gains
  `keep_cmd`; new pure `agent_exit_status`
- `crates/thegn-host/src/pty_drain.rs` — `handle_exit` classifies agent
  session exits and sets the honest status

**Overlap:** file-disjoint from chunk 1 (`handlers/worktree_attach.rs`,
`handlers/{mod,adopt,materialize,provision}.rs`, `panes.rs`, `run.rs`,
`agent.rs`, `cmd/session.rs`) — may run in parallel. Integrate after chunk 1
if serialized (its done-criteria assume the attach path exists to test
against, but the code does not depend on it).

## Approach

Follow design §D3. Today (`pty_drain.rs:542` `handle_exit`):

- the pane's program and daemon session id are captured before removal
  (`:544–552`),
- a **sole** leaf goes through `prep_leaf_for_respawn` (`crash.rs:74–110`)
  and the materialize pipeline respawns a **shell** under "Pane exited;
  restarting shell…" (`:929–934`), keeping the remembered command only when
  the exit failed,
- a **non-sole** leaf is silently removed (`:938–946`).

Both violate the issue's expectation (3) for **agent** sessions. Changes:

1. **Classify the exit** in `handle_exit`, before `panes.table.remove(&id)`
   (`:575`):
   `let daemon_agent_exit = p.is_daemon_backed() && !crate::pane::is_routine_pane(&exited_program)`.
   `is_daemon_backed` is `pane.rs:593`; `is_routine_pane` is `pane.rs:290`
   (true for shells/unnamed panes). A plain daemon shell exiting keeps
   today's behavior exactly — only agent/non-routine programs take the new
   path.
2. **`crash.rs`**:
   - `prep_leaf_for_respawn` (`:74`) gains a `keep_cmd: bool` parameter;
     the command record (`tab.pane_cmds`) is kept when `keep_cmd` OR the
     existing failed-exit rule says so. Callers: the one drain call site plus
     this file's tests. Rationale: a clean agent exit must still arm the
     Enter-to-relaunch overlay (Enter types the remembered command into the
     respawned shell — the mechanism `panes.rs:884–893` already wires).
   - New pure helper `agent_exit_status(program: &str, code: Option<i32>) ->
String` returning e.g.
     `agent claude exited (code 0) — Enter: relaunch · Esc: shell` (code
     `None` renders `?`). Plain ASCII text, no glyph literals.
3. **`pty_drain.rs` sole arm** (`:929–936`): when `daemon_agent_exit`,
   call `prep_leaf_for_respawn(..., keep_cmd = true)` and set
   `ctx.model.status = agent_exit_status(...)`. The `RespawnAction` /
   crash-count logic is unchanged (a _shell_ that keeps crashing still trips
   `GiveUp`; an agent exit with code 0 is not `failed`, so no
   connect-failure marking).
4. **`pty_drain.rs` non-sole arm** (`:938–946`): when `daemon_agent_exit`,
   still remove the pane (a fan-out tab must not accumulate one husk per
   finished stage — the documented tradeoff in design §D3) but set the same
   `agent_exit_status` line so the exit is announced, never silent.
5. Leave the native-exec respawn path alone: with chunk 1's D4 in place, a
   native-exec worktree's `worktrees.agent` still names the agent, so its
   respawn relaunches the agent outright (`panes.rs:813–830`); the honest
   status from step 3 covers both outcomes.

The relaunch affordance itself is pre-existing: `pending_relaunch` arms the
Enter/Esc interception (`run.rs:21351–21395`) and the respawned leaf repaints
the captured final screen (`repaint_scrollback`, `panes.rs:877–883`, tail
captured at `pty_drain.rs:556–575` and stored by `prep_leaf_for_respawn`).
This chunk only changes WHEN the command survives and WHAT the status says.

## Invariants to respect

- All of this runs inside the existing exit-drain path on the loop: no I/O,
  no daemon calls, no new wake sources; the materialize respawn stays
  off-thread (the `left_for_materialize` return contract is unchanged).
- No new `ACTION_SPECS` action (Enter/Esc reuse the existing interception) ⇒
  no help-page/ratchet work.
- No color/glyph literals; status text is plain.
- New ignored `Result`s: none expected; keep existing `// best-effort:`
  comments intact.

## Tests to run (scoped — do NOT run workspace-wide gates while iterating)

```sh
just quick thegn-host
cargo nextest run -p thegn-host crash
cargo nextest run -p thegn-host pty_drain
```

New tests to write:

- `crash.rs`: `prep_leaf_for_respawn` with `keep_cmd = true` keeps
  `pane_cmds` on a **clean** exit (extends the existing
  `prep_leaf_for_respawn_clean_exit_drops_stale_relaunch` /
  `..._failed_exit_keeps_cmd_drops_session` pair — keep those green with
  `keep_cmd = false`); `agent_exit_status` renders both code arms and `None`.
- `pty_drain.rs` tests: if the existing test harness covers `handle_exit`
  end-to-end, pin the classification (daemon + non-routine ⇒ new status;
  daemon shell ⇒ old behavior). Otherwise cover the decision via a small
  pure helper if one is extracted — keep the change minimal.

## Done criteria

- [ ] `cargo nextest run -p thegn-host crash:: pty_drain::` green;
      `just quick thegn-host` clean.
- [ ] Manual check (with chunk 1 landed): `thegn session open --agent … --headless`,
      open the worktree tab, let the agent exit ⇒ the tab shows the final
      screen, the status names the agent + code, Enter reruns the remembered
      command, Esc leaves a plain shell. A plain shell `exit` in a split
      still closes that split silently.
- [ ] A `[daemon] enabled = false` session (in-process pane) exiting behaves
      exactly as before (classification requires daemon-backed).
- [ ] Commit with EXACTLY this subject (conventional, branch-only):

```
feat(host): an attached agent's exit leaves a screen + relaunch, never a silent shell (THE-85)
```
