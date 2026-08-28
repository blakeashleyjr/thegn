# THE-85 — security/test/bug review verdict

Branch `tg/the-85-attach-live-session`, reviewed 2026-08-28 at `8fe71bea`
(+ review fixes `b41434ab`, `01691afe`). Lane docs read in full: architect
`design.md`, `architect-review/verdict.md` (incl. corrections + accepted
deviations), and every chunk `Unverified` section — those formed the checklist.

## PASS

PASS

Ready for the merge queue (`thegn integrate` — not run here, per lane).

The binding `git merge main` was satisfied upstream (merge commit `6efa3ed8`);
verified: `git merge main` → "Already up to date", `git rev-list --count main
^HEAD` = 0. Full branch diff `git diff main...HEAD` reviewed file by file
(1,832 insertions across 11 source files + lane docs).

## Risk-surface audit (lead addenda a–f)

**(a) No blocking daemon/socket call on the loop — ✓**
`worktree_attach::probe` is async, connect-only (`connect_daemon`, never
`ensure_daemon` — asking must not spawn), with exactly two call sites: the
materialize worker (`materialize.rs`) and the prewarm worker (`run.rs`), both
inside `spawn_blocking` closures, results on the existing `SpecBatch` channel +
waker pulse. No new channel, wake source, or inflight set. `Handle::current()`
is captured on the loop; `main.rs` builds a **multi-thread** runtime, so
`Handle::block_on` from the blocking threads is sound — pinned by
`probe_without_a_live_daemon_degrades_to_empty_via_worker_block_on`.
**Review fix (`01691afe`):** the probe was _unbounded_ — it inherited the
status modal's deadline-less `request`, so a daemon that accepts but never
answers (wedged loop, locked DB) would hang the worker forever and leave the
tab stuck on its splash with **no shell fallback**. The whole exchange is now
bounded by `PROBE_TIMEOUT` (3s, the `ensure_daemon` health-poll order),
degrading to shells — pinned by a real wedged-accept unix socket test
(`silent_daemon_hits_the_probe_timeout_instead_of_hanging_the_worker`, ~3s).

**(b) Races — ✓ (one fixed)**

- _Session exits between probe and graft_: `ExecOpen::Attach` failure →
  `SessionFallback` → fresh session on the resolved (suppressed) shell spec,
  scrollback restore applied, honest status — never an error husk. Verified in
  `pane.rs` relay + `daemon_lifecycle::handle_session_fallback`.
- _Two tabs / two batches for one worktree_: the drain re-dedups targets
  against `panes.table` (`shown`) before planning.
  **Review fix (`b41434ab`):** the dedup read `provider_session()`, which is
  `None` until the relay _connects and announces_ — a window in which a second
  batch drained back-to-back (materialize + prewarm both fire on open) would
  attach the **same live session twice**, violating the design's
  `AlreadyShown` contract. Fixed by seeding `session_cell` at spawn for
  `ExecOpen::Attach` (the sid is known at birth; the relay re-seeds it
  identically, and fallback-to-fresh overwrites). Side benefit: a socket blip
  in the pre-announce window now reattaches the _agent_ session instead of
  silently opening the fallback shell. Pinned by
  `attach_pane_publishes_its_session_id_at_spawn_time`.
- _Another workspace's session with the same worktree path string_: match is
  exact-string `SessionInfo.worktree == group path`; a daemon is per-user/
  per-state-dir and a path identifies one worktree on disk, so a cross-path hit
  cannot name someone else's worktree, and any mismatch degrades safely to
  no-attach (today's behavior). Multi-client attach of one session is
  daemon-sanctioned (subscribers; last interactive writer wins resizes).

**(c) No stray bytes into the agent PTY — ✓**
The attach handshake is `source.attach(session, cols, rows)`: subscribe + one
resize (SIGWINCH-class metadata, not input). No `send_input` anywhere on the
open/attach path. Lease semantics checked and benign: an `Interactive` attach
**releases** the relay lease — the mechanism that would otherwise reap the
idle session after grace — so attaching extends the agent's life, never
shortens it.

**(d) Fallback shell never overwrites `worktrees.agent` — ✓**
All three shell-resolution surfaces (materialize fast path + post-provision,
prewarm, `spawn_worktree_shell_pane`) pass `suppress_agent_record: true`. The
sole remaining `launch_spec_center` caller is the explicit `clean-shell` menu
choice, which the recorder skips by contract. Pinned by
`shell_materialize_with_suppressed_record_leaves_the_worktrees_agent_alone`
plus the unsuppressed counter-arm.

**(e) Attached agent exit → screen + honest status; relaunch documented — ✓**
`is_daemon_agent_exit` = daemon-backed ∧ non-routine ∧ **non-wrapper** (bwrap/
ssh/systemd-run test arms pin the fix that stops every sandboxed shell exit
reading "agent bwrap exited"). Sole leaf: leaf left for the materialize
respawn, `keep_cmd` preserves the captured command on clean exit so the
Enter-overlay promise is armable, and `agent_exit_status(…, relaunch)` only
promises Enter/Esc when a command was actually captured (persist-time capture
⇒ host backend ⇒ the overlay can honor it) — the statusbar never lies.
Non-sole leaf: removed (no husk pile-up per finished stage), announced on the
active tab. Plain shell exits keep today's behavior. No new `ACTION_SPECS` —
Enter/Esc reuse `pending_relaunch`; the help ratchet suite (71 tests) is green
with zero allowlist changes. Classification is computed before the pane leaves
`panes.table` (order verified).

**(f) Render-plan purity — ✓**
Attach is an ordinary pane spawn through `materialize_with_specs` / `graft`;
the drain marks dirty/relayout through existing paths. Status writes use the
same `model.status` channel as every handler; `render_plan::plan`'s `Damage`
input is untouched (existing `daemon_attach_status_chrome_is_full` pins the
status→Full shape). No new poll site, color/glyph literal, or platform `#[cfg]`;
`test/` ratchet allowlists show a zero diff.

## User-visible acceptance (traced)

Clicking a pipeline worktree → group tab has missing leaves → loop kicks
`maybe_materialize` → `spawn_blocking` worker resolves the suppressed shell
spec **and** probes the daemon (`live_for_worktree`: un-exited, path-matched,
newest-first) → `SpecBatch { attach }` → `drain_specs` re-dedups, plans
newest→primary leaf + surplus (≤ `MAX_PANES_PER_TAB` headroom, aliased to the
new-pane/split guards; overflow named in status: "N more live agent session(s)
— `thegn attach <id>`") → `materialize_with_specs` attaches via
`spawn_daemon_backed(attach = Some(sid), label = agent program)` → warm
snapshot + live deltas in a pane labeled `pi`, **not a blank shell**. Surplus
sessions graft as splits via `adopt::graft`. Confirmed by trace + tests; the
live-daemon manual flow remains the chunks' open manual item (needs a real
daemon run — flagged by the architect too; do it before ship).

## Other adversarial checks

- **Swallowed errors:** probe failure → debug log + honest shell fallback;
  graft failure → counted, overflow surfaced in status; spawn failure → warn +
  shell. Remaining `let _` uses are waker pulses / channel sends (sanctioned
  class). Nothing user-invoked swallows silently.
- **Injection/paths:** session ids and program names flow only into RPC
  params and display strings (`format!`, not command construction); the
  worktree match is a plain compare, no interpolation. Socket discovery uses
  the user's own XDG paths; no new permission surface.
- **Stale-batch guard** covers the new attach/graft `(gi, ti)` targeting; the
  plan's overflow accounting (`fit − assignments − surplus`) loses nothing
  silently; `graft` clamps `ti` so an out-of-range tab degrades instead of
  dropping a session.
- **`adopt.rs` indexing** (`session.worktrees[group]`): safe — `plan()` has
  already validated `group < groups.len()` and dropped `Malformed`.
- Chunk "Unverified" dispositions: all of the architect's corrections verified
  present in code (wrapper classifier, honest relaunch, split labels,
  `MAX_PANES_PER_TAB` aliasing, runtime-path probe test).

## Frame-affecting note (e2e)

No hermetic-e2e frame changes: attach fires only when a **live daemon session
exists for the worktree** and the exit-status line only for **daemon-backed
non-routine panes** — neither occurs in the hermetic e2e env, so **no
snapshots need re-recording**. If a later change makes attach fire
hermetically (e.g. an in-process daemon fixture), re-record then.

## Follow-ups found (non-blocking; for the roadmap, not this merge)

1. **Explicit close kills an attached agent's session.** Close-pane / close
   tab / CloseWorktree drop panes with kill-on-drop ("a closed pane can't leak
   a live session into a relay lease") — right for pane-created shells,
   destructive for attach-on-open / adopted agent sessions the pane doesn't
   own: dismissing a surplus split kills that agent headless and silently.
   This is **pre-existing** (adopt drain, `7e65ffd8`, on main) — not
   introduced here — but THE-85 makes attached agents the routine case, so the
   exposure multiplies. Suggest detach-on-drop for attach-origin panes or a
   confirm-kill; a product decision, deliberately not made unilaterally in a
   review lane.
2. "Persistent session expired" is the status shown when a probed session
   exited between probe and graft (fallback-to-fresh path). Accurate enough,
   cosmetic wording only.
3. Architect follow-up #1 (seed the relaunch command from the daemon's launch
   argv on `SessionInfo`) stands, unchanged.

## Tests run (scoped; no full-workspace builds)

- `cargo nextest run -p thegn-host` filtered — **138 tests**: worktree_attach
  (pure plan/live arms, probe degrade, probe timeout), pane (incl. the new
  spawn-time sid test), pty_drain (classifier + exit arms), crash
  (`prep_leaf`/`agent_exit_status`), `drain_specs`, suppressed agent record,
  help ratchets (71), render_plan.
- `cargo clippy -p thegn-host --tests -- -D warnings` — clean.
- `just quick thegn-host` — clean.
- Not run (per dev-loop policy / lane): `just test`, `just coverage`, `just ci`,
  e2e — pre-push gate territory.

## Review commits

- `b41434ab` fix(the-85): seed attach pane's session id at spawn, closing the
  double-attach dedup race (review)
- `01691afe` fix(the-85): bound the attach probe (3s) — a wedged-but-accepting
  daemon must degrade to shells, not stall the materialize worker (review)

(Both unsigned: gpg-agent cannot prompt in this headless context; pre-commit
hooks ran — treefmt applied and enforced.)
