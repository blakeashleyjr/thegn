# THE-85 — architect review verdict

Branch `tg/the-85-attach-live-session`, reviewed 2026-08-28.
Design: `.thegn/pipeline/THE-85/architect/design.md` · Chunks: `../code/chunk-{1,2}.md` (+ done docs).

## VERDICT: APPROVED

Reviewed as `git diff main...HEAD` after the binding `git merge main`
(`6efa3ed8` — clean, no conflicts; THE-70/72/73/83 all landed without
touching this branch's hunks). Corrections I applied are committed as
`accfe405`; none warranted a revision chunk.

## Design conformance

- **D1 (attach-on-open at materialize)** — implemented exactly as specified:
  `handlers/worktree_attach.rs` is a sibling module with pure
  `live_for_worktree` / `plan`, a connect-only `probe` (never
  `ensure_daemon`), and `graft_surplus` reusing `adopt::graft`. The probe
  rides the existing spec workers (materialize + prewarm), no new channel,
  no new wake source, no inflight set. Every daemon call stays off the loop;
  the drain-side plan runs before `materialize_with_specs` consumes leaves,
  re-dedups against live `panes.table` daemon sessions, excludes leaves with
  persisted daemon records (they reattach their own session), and the
  stale-batch guard in `drain_specs` protects the `(gi, ti)` targeting.
- **D2 (surplus → splits, capped)** — budget computed from the tab's leaf
  count so `existing + assignments + surplus ≤ cap` holds; overflow is named
  in the status line (`N more live agent session(s) — thegn attach <id>`),
  active tab only.
- **D3 (exit leaves a screen + honest status)** — implemented per spec; two
  gaps found and fixed (below). The `is_active_tab` gating on the non-sole
  status was an implementer judgment call — **accepted**: it matches the
  sole arm, and a background tab's exit must not clobber the visible status.
- **D4 (agent-record suppression)** — all three shell-resolution surfaces
  (materialize fast path + post-provision, prewarm,
  `spawn_worktree_shell_pane`) pass `suppress_agent_record: true`; pinned by
  a new `agent_tests` test that also pins the unsuppressed old behavior.
- **D5 (`--adopt` help note)** — corrected as specified.
- **Invariants** — no new `poll_input` site, no color/glyph literals, no
  platform `#[cfg]`, render decision untouched (attach is an ordinary pane
  spawn through the existing drain), no new ignored `Result`s, no
  help-page/action additions (Enter/Esc reuse `pending_relaunch`). run.rs
  additions stayed inside the existing prewarm/materialize blocks.

## Corrections applied (commit `accfe405`)

1. **Exit classifier excluded runtime wrappers (bug).** `is_routine_pane`
   checks shells/unnamed only, so a daemon-routed pane on a sandboxed
   (bwrap) or remote (ssh) worktree — whose spawn _argv labels the wrapper_ —
   was classified as an agent exit. Every plain shell exit there would have
   read `agent bwrap exited (code 0) — Enter: relaunch · Esc: shell`.
   `is_runtime_wrapper` is now `pub(crate)` and composed into
   `is_daemon_agent_exit`; test arms pin bwrap/ssh/systemd-run.
2. **Relaunch promise made honest (design gap my D3 spec carried).** The
   Enter overlay arms only when `pane_cmds` holds a captured command —
   persist-time capture (workspace switch / quit / rename), host backend
   implied. In the _primary_ flow (dispatch → open → watch → exit, no
   persist in between) there is no captured command, so the old status
   promised keys that do nothing. `agent_exit_status` now takes
   `relaunch: bool`: the sole arm passes "was a command captured", the
   non-sole arm always `false` (its leaf is removed — Enter/Esc have nothing
   to intercept). The statusbar never lies; the hint appears exactly when it
   works.
3. **Surplus splits labeled with the agent.** `adopt::graft` gained a
   `label` param; the surplus path passes the daemon-recorded program
   instead of labeling a live agent "zsh". Adopt drain passes `None`
   (unchanged).
4. **`MAX_PANES_PER_TAB` made the single source.** run.rs's three local
   `MAX_PANES` consts alias it — the attach budget and the pane guards can't
   drift. (Design allowed "pass as a param"; deduping was trivial.)
5. **Probe runtime-path test.** Chunk 1 listed the `spawn_blocking` +
   `Handle::block_on` probe as unverified; new test pins both the no-daemon
   degrade (empty targets) and the production call shape (multi-thread
   runtime keeps driving IO while the worker waits).

## Chunk "Unverified" dispositions

- _Runtime probe path_ — **now verified** (correction 5).
- _Surplus-split labels_ — **now fixed** (correction 3).
- _Enter honesty on non-host backends_ — **now fixed** (correction 2).
- _`is_active_tab` gating_ — **accepted** as implemented (see D3 above).
- _Manual/e2e: live attach to a real `session open --agent` session,
  two-session split, third-beyond-cap status, DB check after opening a
  `--bind`ed worktree, agent-exit screen + relaunch_ — **still open**, needs
  a live daemon run; all unit-testable surfaces are pinned. Do this before
  ship; `just e2e` needs no new baselines (attach only fires when a daemon
  session exists for the worktree — not in the hermetic e2e env).
- _Full-workspace gates_ — pre-push territory per dev-loop policy; scoped
  checks re-run green after corrections (`cargo clippy -p thegn-host
--all-targets -- -D warnings` + 71 targeted nextest tests).

## Follow-ups (non-blocking, recorded here for the roadmap)

1. **Seed the relaunch command at attach.** The Enter hint in the primary
   flow could be restored by having the daemon expose the session's launch
   argv (or a relaunch line) on `SessionInfo`, letting the drain seed
   `pane_cmds` for the attached leaf. Cross-crate (svc control API + daemon
   - host); worth a small follow-up change if the hint matters in practice.
2. **Status picker for surplus sessions** — design non-goal; revisit only if
   the 16-pane cap ever bites.
