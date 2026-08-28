# THE-85 chunk 2 — done: an attached agent's exit leaves a screen + relaunch offer, never a silent shell

Branch `tg/the-85-attach-live-session`, commit **`a1b0a168`** with the exact
required subject:
`feat(host): an attached agent's exit leaves a screen + relaunch, never a silent shell (THE-85)`

Files touched (exactly the chunk's two paths — chunk-1 files untouched):

- `crates/thegn-host/src/handlers/crash.rs`
- `crates/thegn-host/src/pty_drain.rs`

## What was implemented (design §D3 / chunk steps 1–5)

1. **Classification** — `pty_drain.rs`: new pure helper
   `is_daemon_agent_exit(daemon_backed, program)` = daemon-backed ∧
   `!pane::is_routine_pane(program)`. `handle_exit` computes
   `daemon_agent_exit` while the pane is still in the table (before
   `panes.table.remove`). Empty/unnamed programs are routine ⇒ never take the
   new path; plain daemon shells keep today's behavior exactly.
2. **`crash.rs`** —
   - `prep_leaf_for_respawn` gained `keep_cmd: bool` (4th param, before
     `scrollback_tail`); the command record survives when `keep_cmd || failed`.
     Doc comment updated (clean exit drops the cmd _unless_ `keep_cmd`, so the
     respawned shell arms the Enter-to-relaunch overlay the status promises).
   - New pure `agent_exit_status(program, code)` →
     `agent claude exited (code 0) — Enter: relaunch · Esc: shell`
     (`None` renders `code ?`). Same `— … · …` plain-text hint shape as the
     other statusbar lines; no glyph icons, no color literals, no caps work.
3. **Sole arm** — `prep_leaf_for_respawn(..., keep_cmd = daemon_agent_exit, ...)`
   (transport-loss semantics untouched); in the `LeaveForMaterialize` status
   the agent case sets `agent_exit_status(...)` instead of
   "Pane exited; restarting shell…". `RespawnAction` / crash-count /
   connect-failure logic unchanged (a clean agent exit is not `failed`, so no
   connect-failure marking; a crashing shell still trips `GiveUp`).
4. **Non-sole arm** — the leaf is still removed (fan-out tabs must not
   accumulate a husk per finished stage — design §D3 tradeoff) but the exit is
   announced with the same `agent_exit_status` line, gated on `is_active_tab`
   for consistency with the sole arm (a background tab's exit must not clobber
   the visible status line; watching the fan-out tab IS the announced case).
5. **Native-exec respawn path untouched** — the honest status covers both the
   Enter-overlay outcome and the remembered-agent respawn (`panes.rs`
   `db.worktree_agent` rule); chunk 1's D4 keeps `worktrees.agent` meaningful.

Invariants: everything runs in the existing exit-drain path on the loop (no
I/O, no daemon calls, no new wake sources; `left_for_materialize` contract
unchanged); no new `ACTION_SPECS` action (Enter/Esc reuse the existing
`pending_relaunch` interception) ⇒ no help-page/ratchet work; no new ignored
`Result`s; existing `// best-effort:` comments intact.

## Tests

Added (3):

- `crash.rs`: `prep_leaf_for_respawn_keep_cmd_arms_relaunch_on_clean_agent_exit`
  (clean agent exit keeps `pane_cmds`; leaf stays; session record dropped);
  `agent_exit_status_renders_both_code_arms_and_none` (code 0, code 1, `?`).
- `pty_drain.rs`: `daemon_agent_exit_classifies_attached_agent_panes`
  (daemon+`claude` ⇒ true; daemon shells `bash`/`zsh`/`""` ⇒ false;
  non-daemon agent program ⇒ false).

Existing pair kept green with `keep_cmd = false`
(`..._failed_exit_keeps_cmd_drops_session`,
`..._clean_exit_drops_stale_relaunch`); transport-loss and empty-tail tests
updated for the new param, unchanged semantics.

Results (scoped per dev-loop policy — no workspace-wide gates, no e2e):

- `just quick thegn-host` — clean (after the final edits).
- `cargo nextest run -p thegn-host crash:: pty_drain::` — **22/22 passed**
  (includes all of the above).
- Note: during iteration the test compile transiently failed twice on errors
  in chunk-1's in-flight files (`materialize.rs`, `run.rs`,
  `worktree_attach.rs` — a sibling was editing them concurrently). I did not
  touch those files; re-ran after their state settled and everything above is
  green on the settled working tree.

## Done criteria

- [x] `cargo nextest run -p thegn-host crash:: pty_drain::` green;
      `just quick thegn-host` clean.
- [ ] Manual check (needs chunk 1's attach path landed + a live headless
      agent session) — see Unverified.
- [x] `[daemon] enabled = false` behaves as before — by construction:
      classification requires `is_daemon_backed()`; pinned by the classifier
      test's non-daemon arm (no live run — see Unverified).

## Unverified

- **Manual end-to-end behavior** (chunk's second done-criterion): attaching to
  a live `thegn session open --agent … --headless` session, letting the agent
  exit, and observing final screen + honest status + Enter-relaunch /
  Esc-shell — not exercised (no manual/e2e runs in this stage; chunk 1's
  attach path was still landing concurrently). The chunk-1-dependent pieces I
  rely on (attach, `suppress_agent_record`) are exercised by chunk 1.
- **Live daemon / live in-process exits**: the classifier and bookkeeping are
  unit-tested, but no live daemon session was driven; the "split shell exits
  silently as before" regression check is by construction
  (`is_routine_pane` short-circuits) rather than observed.
- **"Enter: relaunch" honesty on non-host backends**: `set_pending_relaunch`
  arms only when the respawn spec's backend is `"host"` (pre-existing
  materialize rule). The design/chunk direct the status unconditionally for
  agent exits, so on a sandboxed worktree the status could over-promise
  Enter. Not addressed (out of the chunk's scope as specified) — flagging for
  review.
- `is_active_tab` gate on the non-sole-arm status: the chunk text says "set
  the same status line" without specifying visibility gating; I gated on the
  active tab to match the sole arm and avoid clobbering the global status
  line from a background tab. Reviewer may prefer unconditional.
