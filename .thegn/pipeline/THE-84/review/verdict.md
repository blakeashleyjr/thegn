# Security / Test / Bug review — THE-84 (daemon-restart resurrection: blank panes, lost agent relaunch)

**Verdict: PASS** — ready for the merge queue (`thegn integrate`).

Branch `tg/the-84-restart-blank-shell`, head at this verdict (two review fixes committed:
`4e0a3e2c`, `03fa5940`). Lane docs read (design, all three chunk specs + done notes,
architect-review verdict incl. its follow-ups and accepted deviations); full branch diff
(`git diff main...HEAD`, 12 source files + lane docs) reviewed file by file; every
coder **Unverified** section addressed below.

## 0. Lead addenda compliance

- **Merged main first.** The architect's merge (`fff5ea01`) was stale: main had moved 37
  commits (THE-78 startup-heal, THE-80 QoS sweep, THE-86 pipeline fold-actor). Re-merged
  clean (`1afebf84`) — and the re-merge immediately proved its worth: **the THE-86
  signature change to `daemon::agent_open::command_for` (new `continue_last` param) had
  textually-cleanly merged but broken the relaunch call site in `worktree_launch.rs`
  (6 args vs 7) — the branch did not compile.** Fixed (`03fa5940`): the resurrection
  relaunch passes `continue_last: false` (it resumes an EXPLICIT discovered session id,
  it never continues blind), with a comment pinning that reasoning. The merged main also
  left the ignored-result ratchet red via `pipeline_retry.rs` (`let _ = code;` on a plain
  i32 — a regex false positive, not a Result); fixed by renaming the unused param to
  `_code` (`4e0a3e2c`) so no allowlist entry is added. All ratchets re-run clean.
- **Mandated batteries, run on the final tree (post-fixes):**
  - `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example) |
test(control_schema)'` — **8/8**.
  - `cargo nextest run -p thegn-host -E 'test(complete) | test(help) |
test(catalog_tests) | test(platform_ratchet) | test(render_plan)'` — **110/110**.
  - `cargo nextest run -p thegn-host help` — **73/73** (help ratchet incl. the
    claimed-but-unwritten prose gate).
  - Plus the lane suites: worktree_launch 8, startup_watchdog 5, daemon_lifecycle
    fallback set, `pane::tests::ladder_reopen_after_drop_emits_session_fallback`,
    materialize/prewarm/agent_open filters — **66/66**; `agent::tests` — **34/34**.
  - `just quick thegn-host` clean; `cargo clippy -p thegn-host --tests` clean (zero
    warnings); forge-leak / async-trait / json-emit / element / ignored-result grep
    ratchets all clean; openspec artifacts unchanged by this branch (openspec CLI not on
    PATH in this shell — architect's 171/171 post-first-merge stands; my two fixes touch
    no spec files).

## 1. The four proofs the Lead demanded

### 1.1 Never relaunches into a worktree that is gone or remote

The relaunch never makes a placement decision: `remembered_agent_relaunch` only ever
REPLACES the first leaf's already-Ok-resolved shell spec, composed through the same
`launch_spec_synced_with` → `launch_spec_full` machinery a shell uses (same cwd, same
`GitLoc`/env resolution, same `env_halt_reason` / `provision_pending` /
`guard_unprovisioned_attach` gates that precede it in both workers). Both call sites
(materialize `maybe_materialize`, run.rs prewarm worker) run `env_halt_reason` BEFORE
the shell resolution — a non-local (remote/provider) env with failover off halts and the
relaunch's let-chain requires an `Ok` batch, so a remote worktree can no more receive a
relaunched agent than a shell. With failover on, shell and agent degrade to host
identically. A gone directory: no code path creates the worktree dir (`ensure_dirs` are
credential/HOME dirs, not the worktree; permissions seed is best-effort and logs), and
the spawn fails exactly as today's shell spawn fails — no new hazard, no path
manipulation from DB data (the DB row only SELECTS which configured `[[agents]]` entry
runs; the executed command always comes from config).

### 1.2 Never double-launches when THE-85 attach finds a live session

`apply_relaunch`'s first gate is `attach_is_empty`, fed by the THE-85 probe
(connect-only, 3s-bounded) at BOTH call sites — materialize folds after the probe,
prewarm `block_on`s the same probe before folding (verified in run.rs order: probe runs
first, then `apply_relaunch(&mut specs, …, attach.is_empty(), …)`). Pinned by
`relaunch_pins_the_first_leaf_and_a_live_attach_wins` (probe non-empty ⇒ batch
untouched). No second dedup mechanism was added: the relaunched session is
worktree-tagged end-to-end (spec cwd → LazyDaemonSource.worktree → SessionMeta.worktree),
so the next open ATTACHes via THE-85. Residual: a cross-instance TOCTOU between the
probe and the spawn is possible with two concurrent UIs opening the same worktree —
pre-existing and identical for shells, out of scope here.

### 1.3 Never resumes a session id that fails the shape check

Two independent layers: `thegn_core::agent_task::auto_resume_id` re-checks opt-in +
RESUME cap + `harness::session_id_ok`, and `daemon::agent_open::command_for` re-validates
and refuses (`bail!`) a bad id. `session_id_ok` is a strict charset check
(`[A-Za-z0-9._-]`, ≤256, non-empty) — no shell metacharacter can reach the harness
resume template, and the id originates from the user's own local session store (bounded
walk, MAX_SESSIONS=500 reads, worktree-cwd-filtered). The resume chain is driven
end-to-end in `resume_composes_the_harness_resume_form` (real discover → auto_resume_id
→ command_for) and `resume_without_a_session_launches_cold`.

### 1.4 The fallback-shell path never records `shell` over a remembered agent

Writer audit (grep, independently re-run): the only `set_worktree_agent` writers are the
guarded write in `launch_spec_full` (`suppress_agent_record` + `clean-shell` + tool
exclusions), wizard, `--bind`, tracker agent dispatch (real user choices), and the
daemon's `sessions.open` agent path (records the agent, not a shell). The watchdog's
clean-shell swap goes through `spawn_clean_shell_pane` → `native_shell_exec` (no spec
compose, no DB write) or `launch_spec_center(.., "clean-shell")` — excluded from the
guarded writer — so the record survives a swap; the daemon*lifecycle fallback handler
writes nothing. Chunk 3's regression tests pin the prewarm (`prewarm_spec_leaves_the*
worktrees_agent_alone`) and sandbox-argv paths on BOTH Ok and Err resolution paths; the
relaunch passes `suppress_agent_record: true` unconditionally
(`the_record_is_never_written_by_a_relaunch`).

## 2. Watchdog bound (stated, as demanded)

A degraded pane (`SessionFallback` — warm-reattach miss or reconnect-ladder reopen) is
watchdog-bound at **8s of byte-blank screen for a local tab, 300s for a remote tab or
unknown remoteness (missing `loading_remote` entry defaults to the safe long window),
extend-once to 600s for remote** — the same numbers and byte-blank precondition
(`history_tail(1).trim().is_empty()`) as the pre-existing splash-armed set. Safety
properties verified in code and tests: entry dropped before any fallible path (fire
once), re-stamp on a second fallback (new session ⇒ new deadline, and the relay ladder
itself is bounded so flap-forever is unreachable), exits-loop + lazy output prune
(`pty_drain::prune_output_degraded`, gated on map non-emptiness — one bool check per
drain), sole-leaf-only swap, swap clears `loading_state`/`loading_remote` so the splash
set cannot re-arm against the fresh pane, splash body scoped by `is_shell_wait` so its
early returns cannot starve the degraded set, err path surfaces via status +
`center_dormant` (no new ignored `Result`s), the reconnect ladder's reopen now announces
`SessionFallback` only on success (regression test drives the real ladder). "Must not
kill a healthy slow shell": a pane that printed anything drops its entry
(`degraded_pane_that_produced_output_is_not_swapped_and_drops_its_entry`,
`healthy_resumed_session_is_untouched`); the thegn-managed direnv warm is bounded (20s)
and completes before the pane exists, so it is never inside the window. Residual,
deliberate: a local shell whose in-pane rc hook (cold `use flake`) exceeds 8s of silence
IS swapped once — identical exposure to the pre-existing splash-armed contract, and the
swap is once-only, non-destructive (rc-free clean shell, config untouched), and
status-announced.

## 3. Swallowed errors / injection / ratchets — hunt results

- No error swallowing on user-invoked paths: DB reads are Option-chained fail-opens to
  shell (correct degrade); `Db::open().ok()?` in the relaunch is a fail-open to
  today's behavior; `command_for(..).ok().map(..)` likewise; the swap's Err path surfaces
  a status and goes dormant. The one new `let _ =` (relay's `SessionFallback` send) is
  the sanctioned send-to-possibly-gone-consumer + waker-pulse pattern, and pane.rs is
  already allowlisted.
- No injection surface: resume id charset-validated twice; agent command sourced from
  config only; DB agent name matched against configured entries before use
  (`cfg.agent_command(&name)?`).
- Ratchets: worktree_launch.rs kept OFF the ignored-result allowlist (architect's fix
  verified); my `pipeline_retry.rs` merge fix keeps the merged tree clean without adding
  an entry; color/glyph/platform/forge/json/element ratchets green on the final tree;
  help ratchet green (no new actions/keybinds — none of the lane adds UI verbs).
- `#[cfg_attr(not(test), expect(dead_code))]` tripwire on the now-test-only
  `agent::launch_spec` is a genuine guard: any new production caller must re-decide
  `suppress_agent_record` or the build fails.

## 4. Tests for the failure paths — gaps found and closed / accepted

- **Closed (review):** the post-merge compile break (§0) — no test could have caught it
  short of compiling the merged tree, which is exactly why the merge-first step exists.
- **Accepted deviations (from architect-review, re-examined, concur):** remote-tab full
  swap arc untested (needs `Instant` ~610s in the past — unrepresentable on young
  machines; the swap path is shared with the asserted local case, remote only changes
  deadline math + status const); live daemon stop/restart choreography not driven (e2e
  excluded per Lead) — **owed: one human smoke or the e2e stage at/after land**, unit
  tests do drive the real relay ladder, real `spawn_clean_shell_pane`, and the real
  fallback handler.

## 5. Frame impact / e2e

This lane changes frames (new status strings on fallback/swap; relaunch replaces the
spawned process after a restart). Grep of `test/muse/snapshots/` confirms NEITHER the
old ("Persistent session expired; …") NOR the new ("Session died with the daemon — …",
"… swapped in a clean shell") status strings are pinned by any snapshot, and the
degrade/relaunch scenarios require a daemon restart, which e2e does not drive —
**no `just e2e-update` owed**.

## 6. Verdict

PASS. The three lanes implement the design as specified; all four Lead proofs hold in
code and tests; the watchdog bound is stated and enforced; the record-audit invariant
holds; the merged tree compiles and is gate-clean. Land via `thegn integrate`.
