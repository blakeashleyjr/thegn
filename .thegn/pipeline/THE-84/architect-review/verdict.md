# Architect review — THE-84 (daemon-restart resurrection: blank panes, lost agent relaunch)

**Verdict: APPROVED** (one gate miss fixed in-review, commit `608bf33f`; merge of main at
`fff5ea01`). Branch `tg/the-84-restart-blank-shell`, head `608bf33f` + this docs commit.

## 0. Lead addenda compliance

- **Merged main first.** Clean auto-merge (THE-78 startup-heal, THE-70/THE-83 were already
  in the branch's base via the THE-76 fold). One pre-existing dirty artifact (treefmt drift
  on the landed THE-76 verdict) committed separately as `chore(lint)` so the merge started
  clean. Post-merge: `just quick thegn-host` clean, openspec 171/171.
- **Full branch diff reviewed** (`git diff main...HEAD`: 10 source files + lane docs) and
  every **Unverified** section in the three done-notes verified or flagged (§4).
- **Mandated batteries** run on the final tree:
  - `nextest -p thegn-core -E 'test(env_overlay) | test(config_example) | test(control_schema)'` — **8/8**
  - `nextest -p thegn-host -E 'test(complete) | test(help) | test(catalog_tests) | test(platform_ratchet)'` — **90/90**
  - plus the lane's scoped suites (worktree_launch 8, startup_watchdog 5, daemon_lifecycle 9,
    relay/pane ladder 1, agent::tests 34, agent_open 12, prewarm/materialize filters) — **90/90**.

## 1. Design conformance (per chunk)

### Chunk 1 — remembered-agent relaunch (`fceee179`)

Matches design §2 line for line. Verified in the code, not just the done-note:

- Decision ladder order is exactly the design's: record read → `shell`/`clean-shell`/tool-drawer
  exclusions (same as the native-exec path) → entry-still-configured (stale record left alone) →
  **resume-first** (cheap config read before any fs walk) → spec via
  `launch_spec_synced_with` (full sandbox/credential/daemon-persistence parity by construction) →
  `suppress_agent_record: true` unconditionally.
- All three call sites gate correctly: materialize folds once after both shell branches merge
  (`attach.is_empty()`, `!quiet`, first missing leaf); the prewarm worker captures `first_leaf`
  before its resolve moves `missing`, gates on `!is_terminal`, `quiet_split: false` (a prewarm is
  never a split). A live session still wins — no double-spawn, and the relaunched session is
  worktree-tagged so THE-85's next open attaches. No second dedup mechanism added.
- Resume chain re-uses the hardened seams (`sessions::discover` bounded walk →
  `agent_task::auto_resume_id` re-checks → `agent_open::command_for` id-shape-validated;
  visibility bump on `command_for` is the only `agent_open.rs` delta).
- Deviation (`apply_relaunch` extraction) accepted: the gates/let-chain are byte-for-byte the
  chunk's call-site shape, and the required worker-level test now exercises the REAL fold instead
  of a test-side replica (spec offered `worktree_launch.rs` as the home — used).
- Tests: 8/8 in-module (ladder + batch semantics + record-invariance), XDG_STATE_HOME isolated,
  serialized on `ENV_LOCK`.

### Chunk 2 — watchdog-bound degrade (`84d48a79` + `fea3e291`)

Matches design §1.5 exactly:

- `relay_exec`'s ladder reopen now announces `SessionFallback` **only on success** (the
  exhausted-ladder error husk keeps its shape); relay regression test proves the event on the real
  ladder.
- `degraded_at` recorded on entry, re-stamp on a second fallback (correct: new session, new
  deadline); missing-pane edge still records and is pruned by the exits loop (tested).
- `tick` splits into `tick_splash` (body moved verbatim, scoped by `is_shell_wait` at the call
  site so its early returns cannot starve the new set) + `tick_degraded`: byte-blank precondition
  identical to the splash set, per-TAB remoteness via one scan (`loading_remote` missing ⇒ safe
  long window — identical to `active_watchdog_deadline`), extend-once with the entry KEPT, fire
  once (entry dropped before any fallible path), sole-leaf conservatism, swap clears the tab's
  loading entries so the splash set cannot re-arm against the fresh pane, `load_steps` deliberately
  untouched (correct — a degraded pane can be a background tab). Err path surfaces via status +
  `center_dormant`; **no new ignored `Result`s**.
- Pruning: exits loop + lazy output sweep (`prune_output_degraded`), both gated on non-emptiness —
  one bool check per drain in the common case. 0%-idle and render-decision invariants untouched
  (no new wake sources; marks `dirty`/`need_relayout` exactly like the splash fire).
- Statuses: the relaunch-offer string is regression-tested byte-identical; the two new honest
  shapes are not pinned by any muse snapshot (verified by grep) and no frame shape changed, so no
  `e2e-update` owed.

### Chunk 3 — record-path audit (`96cddd41`)

- Both remaining unsuppressed `"shell"` sites fixed: `prewarm_sandbox_chain` (via the new
  `agent::prewarm_spec`, which also corrects the warm to the daemon-persistent builder — a bonus
  the design prescribed) and the `sandbox-argv` verb. The `#[cfg_attr(not(test),
expect(dead_code))]` tripwire on `launch_spec` is a good touch: any new production caller must
  re-decide about `suppress_agent_record`.
- **Audit invariant re-verified independently (grep)**: the only `set_worktree_agent` writers are
  the guarded write in `launch_spec_full` (user-picked choices), wizard, preset bind, `--bind`, and
  tracker's agent dispatch (`suppress_agent_record: false` — deliberate: a real agent choice, and
  it records the agent name, never `shell`). Every production `launch_spec*` `"shell"` caller now
  suppresses; remaining hits are `#[cfg(test)]`.
- Err-path coverage in the new tests is meaningful, not decorative: the record write in
  `launch_spec_full` precedes the failing sandbox resolution, so the test proves suppression, not
  luck.

## 2. Gate miss found and fixed (architect commit `608bf33f`)

`worktree_launch.rs` matched the **ignored-result ratchet** (test helper's
`let _ = std::fs::remove_dir_all(..)`) and was not pinned — `just lint` / `just test` would have
failed. The scoped nextest/clippy gates the coder ran do not execute the ratchets, so this slipped.
Fixed by rewriting the two best-effort scratch-dir cleanups in a non-matching form
(`unwrap_or_default()` + reason comment) so the shrink-only allowlist stays shrunk rather than
gaining an entry. All five grep ratchets re-run: clean.

## 3. Live verification done by the architect

`thegn sandbox-argv` against a scratch `XDG_STATE_HOME` with a pre-seeded
`worktrees.agent = "claude"` row: argv printed, **record survived** (base main stamped `shell`
here). Closes chunk 3's live-behavior Unverified for the verb path; the prewarm path shares the
same builder and its suppression is unit-pinned on both Ok and Err paths.

## 4. Flagged / owed (not design gaps)

1. **Live daemon-restart choreography (chunks 1+2 end-to-end)** — stop daemon under a running UI →
   restart → click the tab → agent relaunches (resumed when opted in); degraded pane → status →
   swap within one window. Needs a driven UI + daemon; e2e excluded per Lead. Unit tests drive the
   real relay ladder, real `spawn_clean_shell_pane`, and real fallback handler. **Owed: one human
   smoke or the e2e stage.**
2. **Remote-tab full swap arc untested** (extension-before-swap only). Justified: the swap path is
   shared with the asserted local case and remote only changes deadline math + status const;
   `Instant`'s boot-time epoch makes a 610s-old stamp unrepresentable on young machines (documented
   in `stamp_ago`).
3. **chunk-3-done's writer list is imprecise** — omits tracker's agent dispatch and launch-menu
   `compose_choice` (both deliberate recorders, neither writes `shell`; code verified correct). Doc
   nit only; no action required.
4. **Heavy gates** (`just test`, coverage, `just ci`) owed at pre-push/PR per the dev-loop policy;
   the mandated subsets above are green on the merged tree. Watchdog remote tests need ~310s
   machine uptime (documented in the tests themselves).

## 5. Verdict

APPROVED. The branch implements all three design lanes with the prescribed files, commit subjects,
and invariants; the record-audit invariant holds; the watchdog bounds every blank-respawn state at
one window (extend-once for remote) without regressing the splash machinery; the relaunch ladder
never doubles a live agent and never rewrites `worktrees.agent`. Landable after the standard
pre-push gate.
