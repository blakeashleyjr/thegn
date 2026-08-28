# Chunk 2 — THE-84: a degraded daemon session is watchdog-bound, not blank forever

Closes THE-84 lane (1): after a daemon restart, a respawned login shell can
print nothing (cold in-pane direnv/devshell eval, or an rc hang — design.md
§1.3) and NOTHING guards it: the clean-shell watchdog only arms on the active
tab's shell-wait splash (`handlers/startup_watchdog.rs:31-33`), the splash
clears on the FIRST output bytes including invisible ones
(`loading/mod.rs:119-127`), and the reconnect ladder's silent reopen
(`pane.rs:1199-1215`) never even notifies the loop (its
`source.open(&reopen_spec)` recovery is DEBUG-logged only). See design.md
§1.4–§1.5.

## Files touched (exact paths)

- `crates/thegn-host/src/pane.rs` — `relay_exec`'s reconnect ladder
  (`:1199-1215`): after a successful `source.open(&reopen_spec)`, send
  `PaneEvent::SessionFallback(id)` (the event already exists,
  `pane.rs:82`; the attach path sends it at `:1093-1100`).
- `crates/thegn-host/src/pty_drain.rs` — `DrainCtx` gains the
  `degraded_at` field; the exits loop prunes entries.
- `crates/thegn-host/src/handlers/daemon_lifecycle.rs` —
  `handle_session_fallback` (`:254-279`): record the degrade moment into the
  new `degraded_at` map via `DrainCtx`; reword the status so the two degrade
  shapes are honest and distinct.
- `crates/thegn-host/src/handlers/startup_watchdog.rs` — `tick` gains a second
  candidate set: degraded panes.
- `crates/thegn-host/src/run.rs` — loop locals + ctx threading ONLY:
  `degraded_at: HashMap<u32, std::time::Instant>` declared beside
  `shell_watchdog_fired` (`:5969-5975`), passed into `DrainCtx`
  (`pty_drain.rs` field + construction site) and `StartupWatchdogCtx`
  (`:11078-11095`), pruned in the exits loop.

## Approach

1. **Emit the degrade.** In `relay_exec`, the ladder's reopen branch
   (`pane.rs:1199-1215`) currently: clears the stale session id, opens a fresh
   session, continues. Add: after a successful open, send
   `PaneEvent::SessionFallback(id)` + `wake()` before `continue` — the same
   notification the initial attach-degrade already sends, so the loop has ONE
   degrade event for both resurrection paths. Do NOT send it when the open
   fails (the existing exhausted-ladder error husk handles that).
2. **Record + message.** `DrainCtx` gains
   `pub degraded_at: &'a mut HashMap<u32, std::time::Instant>`. In
   `handle_session_fallback`: on entry, `degraded_at.insert(id, Instant::now())`
   (dedupe: a second fallback for the same pane re-stamps — the deadline
   restarts, which is correct: the pane got a NEW session). Status wording:
   - with a relaunch offer (existing `restore.relaunch` arm, `:264-267`):
     keep `:269-271` verbatim;
   - with a scrollback repaint only: `"Session died with the daemon — \
restored its last output into a fresh shell"`;
   - bare (no persisted payload — the ladder reopen of an `Open` pane):
     `"Session died with the daemon — opened a fresh shell"`.
     Keep `dirty`/`dirty_panes` handling exactly as-is (`:274-278`).
3. **Watch degraded panes.** In `startup_watchdog::tick`, after the existing
   splash-armed candidate block (keep it untouched — the splash machinery is
   not regressed), add the degraded-pane check:
   - For each `(pid, t0)` in `degraded_at` where the pane still exists:
     candidate only if the pane is byte-blank — the SAME precondition as
     `:70-77` (`history_tail(1).trim().is_empty()`) — and
     `t0.elapsed() > deadline`.
   - `deadline` = `effective_watchdog_deadline(base, remote, extended)` with
     the pane's TAB remoteness: resolve the pane's `(group, tab)` by scanning
     `ctx.session.worktrees` for a tab whose `center.pane_ids()` contains the
     pid (single scan; the same shape the Output path uses), then
     `loading_remote` lookup with the missing ⇒ `true` (safe long window)
     default — identical policy to `active_watchdog_deadline`
     (`loading/mod.rs:29-40`). A pane whose tab can't be found (corner/
     drawer panes never degrade here, but be safe) ⇒ skip.
   - Fire ONCE per pane: remove the map entry, log WARN with
     pane/session/program context, swap via the existing
     `crate::run::spawn_clean_shell_pane` (`run.rs:5112-5146`) into the
     pane's leaf (`crate::panes::replace_single_dead_center_pane` when the
     leaf is sole; if the pane is NOT its leaf's sole pane, only remove the
     entry and log — the multi-leaf swap is out of scope, mirror the
     single-leaf conservatism of the splash watchdog), set the status line:
     `"Session died with the daemon and the fresh shell never produced \
output — swapped in a clean shell. `thegn doctor bundle` captures \
diagnostics."` (local) / the existing remote-flavored variant for
     remote tabs.
   - A pane that printed anything is not blank ⇒ the precondition fails ⇒ the
     entry can be dropped lazily on first output: in the same drain where
     output lands, if `degraded_at` is non-empty, drop ids whose pane's tail
     is non-empty (cheap gate: skip the whole sweep when the map is empty —
     the `any_clearable_splash` pattern, `loading/mod.rs:103-110`).
4. **Prune on exit.** The exits loop already collects exited pane ids
   (`pty_drain.rs:287-294`): `degraded_at.remove(&id)` there. Pane ids are
   monotonic and never reused, so a missed prune is harmless memory, not a
   correctness bug — say so in a comment.

## Invariants / ratchets

- 0% idle: all of this runs inside existing drains/ticks — no new wake source,
  no new polling. The watchdog tick already runs per loop iteration
  (`run.rs:11078`); the degraded sweep is a HashMap scan gated on non-emptiness.
- Render decision: status-line + pane swaps mark `dirty`/`need_relayout`
  exactly like the existing watchdog fire path (`startup_watchdog.rs:120-146`).
- No new ignored `Result`s: the swap's `Err` is surfaced via status +
  `center_dormant` exactly as `:147-155` does today.
- No help-page changes: no new action ids (help ratchet untouched). No color/
  glyph literals. No platform cfg.
- e2e: the degrade paths are not driven by any muse spec (verified — no
  snapshot contains the existing fallback statuses), and no frame shapes
  change; `just e2e-update` is NOT required.

## Tests (scoped)

- `just quick thegn-host`
- `cargo nextest run -p thegn-host startup_watchdog`
- `cargo nextest run -p thegn-host daemon_lifecycle`
- `cargo nextest run -p thegn-host relay` (pane.rs relay tests)

Unit tests to add:

1. `startup_watchdog`: a degraded pane, blank screen, past the local deadline
   ⇒ swap fires once, entry removed, status names the respawn; a degraded pane
   that produced output ⇒ no swap and the entry is dropped.
2. `startup_watchdog`: degraded remote tab ⇒ one extension before the swap
   (`watchdog_should_extend` policy reused, `loading/mod.rs:62-70`).
3. `daemon_lifecycle`: a `SessionFallback` for a pane with no persisted
   payload records `degraded_at` and sets the bare-fresh-shell status; with a
   relaunch payload the existing status is byte-identical (regression).
4. `pane.rs` relay: an `ExecOpen::Open` daemon pane whose session drops and
   whose reattach fails then reopens ⇒ a `SessionFallback` event is emitted
   after the successful reopen (extend the existing
   `Some(PaneEvent::SessionFallback(9)) => {}` test at `:1857`).

## Done criteria

- [ ] All gates above green; `just quick thegn-host` clean.
- [ ] Behavior: stop the daemon under a running UI, restart it, let the tab's
      session degrade → the status line names the respawn, and a shell that
      produces no output is swapped for a clean rc-free shell within one
      watchdog window (8s local / 300s remote, extend-once). A healthy
      resumed session produces no splash, no status, no swap.
- [ ] The splash-armed startup watchdog behaves byte-identically for its
      existing cases (its tests pass unmodified).
- [ ] Commit subject (exact):

```
fix(the-84): a degraded daemon session is watchdog-bound, not blank forever
```

## Overlap / dependency

Touches `run.rs` (loop locals + two ctx construction sites) — **run AFTER
chunk 1, BEFORE chunk 3** (all three touch `run.rs`; strictly serial).
`pty_drain.rs` is otherwise untouched by other chunks.
