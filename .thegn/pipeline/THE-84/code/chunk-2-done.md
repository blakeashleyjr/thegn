# Chunk 2 — THE-84: a degraded daemon session is watchdog-bound, not blank forever

Status: **complete**. Branch `tg/the-84-restart-blank-shell`, staged after chunk 1
(`a39c1471`) as the chunk spec requires (run.rs is shared — strictly serial).

## Commits

- `84d48a79` — `wip(the-84): chunk 2 stage 1 — the reconnect ladder's reopen announces SessionFallback`
  (pane.rs: emit + relay test; incremental commit per the lead addenda)
- `fea3e291` — `fix(the-84): a degraded daemon session is watchdog-bound, not blank forever`
  (the exact commit subject the chunk spec prescribes; all other files)

## What landed

1. **Emit the degrade** (`crates/thegn-host/src/pane.rs`). The reconnect ladder's
   reopen branch now sends `PaneEvent::SessionFallback(id)` + `wake()` after a
   successful `source.open(&reopen_spec)` — the same notification the initial
   attach-degrade already sends, so the loop has ONE degrade event for both
   resurrection paths. Not sent when the open fails (the exhausted-ladder error
   husk owns that shape).
2. **Record + message** (`crates/thegn-host/src/pty_drain.rs`,
   `crates/thegn-host/src/handlers/daemon_lifecycle.rs`). `DrainCtx` gained
   `pub degraded_at: &'a mut HashMap<u32, std::time::Instant>`.
   `handle_session_fallback` inserts the degrade moment on entry (a second
   fallback re-stamps — the deadline restarts, correct for a NEW session) and
   rewords the statuses:
   - relaunch offer: `"Persistent session expired; press Enter to relaunch (Esc for a shell)"` — kept verbatim (regression-tested byte-identical);
   - scrollback repaint only: `"Session died with the daemon — restored its last output into a fresh shell"`;
   - bare (no persisted payload — the ladder reopen): `"Session died with the daemon — opened a fresh shell"`.
     `dirty`/`dirty_panes` handling unchanged. The body moved to
     `apply_session_fallback` over bare state slices so it is unit-testable
     without a `DrainCtx` (the same split `detach_exited_terminal` uses in that
     file); `handle_session_fallback` stays the thin `DrainCtx` entry.
3. **Watch degraded panes** (`crates/thegn-host/src/handlers/startup_watchdog.rs`).
   `tick` now runs two candidate sets behind one shared `center_dormant` gate:
   the splash-armed body moved verbatim to `tick_splash` (gated by
   `is_shell_wait` at the call site so its early returns cannot starve the
   degraded sweep), and `tick_degraded` sweeps `degraded_at`:
   - candidate = pane exists, `history_tail(1).trim().is_empty()` (the SAME
     byte-blank precondition as the splash set), `t0.elapsed() > deadline`;
   - deadline = `effective_watchdog_deadline(base, remote, extended)` with the
     pane's TAB remoteness resolved by a single `worktrees` scan for a tab whose
     `center.pane_ids()` contains the pid; `loading_remote` missing ⇒ `true`
     (safe long window — identical policy to `active_watchdog_deadline`);
     unresolvable tab ⇒ skip;
   - remote first expiry ⇒ one extension via `watchdog_should_extend`
     (`shell_watchdog_extended` latch reused) and the entry STAYS so the doubled
     window keeps guarding the pane;
   - fire ONCE per pane: entry dropped first, WARN with
     pane/session/program/secs context, then the swap via
     `crate::run::spawn_clean_shell_pane` +
     `crate::panes::replace_single_dead_center_pane` when the pane is its
     leaf's sole pane (multi-leaf only logs and drops the entry — the splash
     watchdog's single-pane conservatism); non-sole panes never refire;
   - status: local — `"Session died with the daemon and the fresh shell never produced output — swapped in a clean shell. `thegn doctor bundle` captures diagnostics."`;
     remote — the splash fire's remote variant verbatim
     (`REMOTE_DEGRADED_SWAP_STATUS` const, kept separate so the splash fire
     block stayed untouched);
   - the swap also clears the tab's `loading_state`/`loading_remote` entries —
     a lingering shell-wait splash would re-arm the splash-armed watchdog
     against the FRESH pane. `model.load_steps` is NOT cleared directly (a
     degraded pane can be in a background tab; `load_steps` re-derives next
     iteration from the active key, so clearing it here could clobber the
     ACTIVE tab's splash);
   - errors: no new ignored `Result`s — the swap's `Err` surfaces via status +
     `center_dormant` exactly like the splash fire.
4. **Prune** (`pty_drain.rs`). The exits loop drops exited panes' entries;
   `prune_output_degraded(degraded_at, panes)` (called after the drain's parse
   pass) lazily drops entries whose pane has printed anything, gated on a
   non-empty map (the `any_clearable_splash` pattern — one bool check per drain
   in the common case). Narrow params so tests drive it directly. Pane ids are
   monotonic; a missed prune is harmless memory (commented at the prune site).
5. **Loop wiring** (`crates/thegn-host/src/run.rs`) — loop local
   `degraded_at: HashMap<u32, std::time::Instant>` beside
   `shell_watchdog_fired`/`shell_watchdog_extended`, threaded into `DrainCtx`
   and `StartupWatchdogCtx`. No other run.rs logic touched.

## Tests (all scoped per the dev-loop policy)

Added:

- `startup_watchdog` (5): degraded blank pane past the local deadline swaps
  once (real clean-shell spawn: entry removed, leaf replaced, status names the
  respawn, dirty+relayout; second tick is a no-op); degraded pane that produced
  output ⇒ no swap + the drain sweep drops the entry; degraded remote tab ⇒ one
  extension before any swap (entry stays; doubled window holds the same stamp);
  unknown remoteness ⇒ the safe long window (no premature fire, extension still
  latches); healthy resumed session ⇒ no splash/status/swap.
- `daemon_lifecycle` (4): bare fallback records `degraded_at` + bare status;
  scrollback-only fallback sets the restored status; relaunch payload keeps the
  existing status byte-identical (regression); fallback for a missing pane
  still records but sets no status.
- `pane.rs` relay: `ladder_reopen_after_drop_emits_session_fallback` — an
  `ExecOpen::Open` daemon pane whose session drops, whose reattach fails, and
  whose reopen succeeds emits `SessionFallback(9)` then relays the reopened
  session; close kills the reopened session server-side. (A sibling of the
  existing attach-path test — extending that one in place would have broken its
  sid-seed/kill assertions; it passes unmodified.)

Gates run: `just quick thegn-host` (clean, run twice — after each stage),
`cargo nextest run -p thegn-host` over `startup_watchdog`, `daemon_lifecycle`,
`tab_keys`, `provision`, `loading::`, `pty_drain`, and the pane relay tests —
**131/131 passed**. The splash-armed watchdog's existing behavior is covered by
those suites (`tab_keys`' watchdog-crossfire regressions, `provision`'s
shell-wait arming tests) and passes unmodified; the splash body itself moved
verbatim.

## Invariants / ratchets

- 0% idle: the degraded sweep runs inside `tick`, which already runs per loop
  iteration, and is gated on a non-empty map; the lazy drop is one bool check
  per drain. No new wake sources.
- Render decision: the fire marks `dirty`/`need_relayout` exactly like the
  existing watchdog fire path; nothing recomposes chrome.
- No color/glyph literals, no platform cfg, no new action ids (help ratchet
  untouched), no new ignored `Result`s.
- e2e: not run (per instructions). No muse snapshot contains the fallback
  statuses and no frame shape changed, so `just e2e-update` is not required
  (matches the chunk spec's finding).

## Unverified

- **Live daemon-restart flow not manually exercised.** The done-criteria
  behavior (stop the daemon under a running UI, restart it, watch the tab
  degrade → status → swap within one watchdog window) was verified at the unit
  level — the tests drive `tick_degraded` through the REAL swap
  (`spawn_clean_shell_pane` spawns an actual clean shell), the real relay
  ladder, and the real fallback handler — but the full live daemon
  stop/restart choreography was not run (it needs a driven UI session; e2e was
  out of scope per instructions). Recommend the review stage (or a human) do
  one `thegn daemon` restart against a live UI.
- **Remote-tab full swap arc not asserted** (only the extension-before-swap).
  Firing the remote swap in a test needs an `Instant` ~610s in the past, which
  a machine with <10 min uptime cannot represent (`Instant`'s epoch is boot
  time on Linux — documented in `stamp_ago`). The swap code path itself is
  shared with the local case (asserted end-to-end); remote only changes the
  deadline math and the status const.
- **Watchdog tests need ~310s machine uptime** for the remote-extension stamps
  (same `Instant`-epoch caveat; local-deadline tests only need ~9s).
- `just test` / `just ci` / coverage / e2e not run — heavy gates are pre-push
  per the dev-loop policy; the pre-push hook will run them.

## Overlap notes for siblings

Only the five files the chunk spec lists were touched. `run.rs` changes are
exactly the loop local + the two ctx construction sites — no shared-file
reordering. `pty_drain.rs` was otherwise untouched by other chunks. No
conflicts expected with chunk 3's run.rs work beyond the usual adjacent-line
rebase-in-queue noise (strictly serial per the spec, so none in practice).
