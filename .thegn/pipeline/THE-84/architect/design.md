# THE-84 — Daemon-restart resurrection: blank panes, lost agent relaunch

Issue: after `thegn daemon stop` + restart (repro base main `17c90f98`, i.e.
**pre-THE-85**), worktrees whose headless pipeline sessions died with the old
daemon open as a BLANK pane when clicked: `session snapshot --text` empty,
process `/run/current-system/sw/bin/zsh -l` in `Ss+` for minutes (journal:
`Started [systemd-run] zsh -lc "${SHELL:-/bin/sh} -l"`), no prompt ever — and
`worktrees.agent` was overwritten `pipeline-coder` → `shell`.

Scope per Lead: THE-85 (`64255352`, in main) already attaches tabs to **live**
sessions and suppresses the agent-record overwrite on the materialize/split/
preset shell paths. This lane is what remains: (1) why the respawned login
shell can sit blank forever and why the clean-shell watchdog never rescued it;
(2) a resurrected tab with a remembered agent must relaunch that agent
(resume-aware); (3) the remaining paths that still record `shell` over a
remembered agent.

---

## 0. Verified on main first (THE-85 status)

- `git merge-base --is-ancestor 64255352 HEAD` — THE-85 is in this branch's
  base. Its attach machinery: `handlers/worktree_attach.rs::probe`
  (3s-bounded, connect-only) → `live_for_worktree` (filters
  `exited_at_ms.is_none()`, `worktree == wt`, not-already-shown) →
  `plan` zips newest targets onto missing leaves →
  `panes.rs::materialize_with_specs` warm-reattach branch
  (`crates/thegn-host/src/panes.rs:786-836`) attaches through the one door
  (`spawn_daemon_backed`, `ExecOpen::Attach`).
- D4 record suppression is in place at the three shell paths THE-85 touched:
  `handlers/materialize.rs:204-212` (warm-spare branch) and
  `:242-250` (post-provision branch), `run.rs:5092-5103`
  (`spawn_worktree_shell_pane`), `handlers/launch.rs:163-175` (presets).
  All pass `suppress_agent_record: true`; `agent.rs:3055-3066` is the guarded
  write.
- **What THE-85 did NOT change**: the materialize/prewarm workers still
  resolve a plain **shell** for every missing leaf when no live session exists
  (`materialize.rs:204-212`, `:242-250`, `run.rs:7854-7862` — choice
  hard-coded `"shell"`). A dead prior session ⇒ a shell tab, never the
  remembered agent. That is lane (2).

## 1. Why a login zsh produces no prompt — evidence, not hypotheses

### 1.1 Live reproduction (prebuilt tg-the-85 binary, isolated scratch worktree)

`thegn session open --worktree /tmp/the84-wt --agent pipeline-coder` (the
worktree registered in the DB, agent resolving to an interactive harness)
produced a session whose **daemon-side screen captured and served output with
zero attached clients** (`session snapshot --text` rendered a full screen;
input + snapshot round-tripped). So the daemon's PTY pump, session actor,
scrollback and snapshot machinery are healthy on this box — a freshly spawned
daemon session is not generically blank.

⇒ The blank pane is specific to the **resurrected-tab respawn paths** and/or
what the respawned shell does in that environment, plus the absence of any
guard once it is blank. Three concrete mechanisms, with file:line:

### 1.2 The two respawn paths after a daemon restart

**Path A — UI restarted with the daemon.** Clicking the tab re-hydrates the
layout; the leaf has a persisted `pane_sessions` record (`provider = "daemon"`)
pointing at the DEAD session. `materialize_with_specs`
(`panes.rs:786-836`) warm-reattaches: `spawn_daemon_backed(..., Some(dead_id))`
→ relay `attach` fails → `source.open(&fallback)` (the resolved shell spec)
→ the daemon spawns a FRESH login-shell session — exactly the journal line
(`systemd-run … zsh -lc "${SHELL:-/bin/sh} -l"`, the Systemd sandbox family's
`enter_argv` shape, `thegn-core/src/sandbox.rs:2020-2035` + `:2110-2114`).
THE-85's attach probe finds nothing (all old sessions are dead), so the shell
spec wins.

**Path B — UI stayed up across the daemon restart.** The pane's relay ladder
(`pane.rs:1110-1215`): `SessionEnd::Dropped` → reattach same session fails
(daemon restarted; sessions gone) → **silently opens a FRESH session** via
`reopen_spec` (`pane.rs:1199-1215`, `LazyDaemonSource::open` even re-spawns a
daemon — `daemon/client.rs:56-86`). This path emits **no**
`PaneEvent::SessionFallback` (that event is sent only on the initial
`ExecOpen::Attach` degrade, `pane.rs:1053-1100`), no status line, and no
splash — the reopen is logged at DEBUG only.

### 1.3 Why the shell can print nothing (and stay `Ss+`)

A daemon-side login shell that never prints is "alive, foreground, zero bytes".
The host-side direnv warm is bounded (`direnv.rs:34` `WARM_NOW_TIMEOUT = 20s`,
`direnv_warm.rs:36-51`) and happens off-loop before the spec exists, so it
cannot blank the pane. What is **unbounded and silent** is the in-pane
environment entry: the login shell's rc hook (`direnv`/`use flake`) or the
env's devshell entry re-evaluates inside the sandbox, and a cold `nix` eval
prints nothing for minutes — matching `Ss+` + zero bytes exactly. The same
signature is produced by any rc-file hang (the exact premise the clean-shell
watchdog was built for, `agent.rs:91-101`). Which of the two fired in the
user's env cannot be pinned post-hoc — the design therefore does not try to
fix the shell; it (a) bounds the blank STATE regardless of cause (lane 1 fix)
and (b) makes the productive respawn the default (lane 2).

### 1.4 Why the clean-shell watchdog did not fire (the fixable gap)

`handlers/startup_watchdog.rs::tick` — the only guard against "blank pane,
live shell" — cannot see any of these panes:

1. **Arms only on the active tab's splash, in shell-wait shape**
   (`startup_watchdog.rs:31-33`; `model.load_steps` is derived per active tab,
   `run.rs:11053-11062`). Path B never seeds a splash at all. Path A seeds one,
   but…
2. **…the splash clears on the FIRST output bytes**
   (`loading/mod.rs:119-127` `should_clear_splash_on_output` → drain clears the
   entry on any `PaneEvent::Output`), and a login shell emits invisible bytes
   before its prompt (e.g. zsh's `\e[?2004h` bracketed-paste enable). Splash
   gone ⇒ `is_shell_wait` false ⇒ watchdog disarmed while the screen is still
   visually blank.
3. **Single-leaf, active-tab, once-only**: candidate requires
   `pane_ids().len() == 1` and the tab being active (`startup_watchdog.rs:35-66`),
   and `shell_watchdog_fired` allows exactly one swap per tab
   (`run.rs:5969`). The fallback clean shell itself rides the same
   daemon+systemd spawn path, so a systemic blank survives the one swap.

Net: a respawned login shell that prints nothing leaves a blank pane
indefinitely — precisely the reported "minutes, no prompt".

### 1.5 Fix (lane 1): bound the blank state on the degrade path

- **Emit the degrade.** `relay_exec`'s reconnect-ladder reopen
  (`pane.rs:1199-1215`) sends `PaneEvent::SessionFallback(id)` after a
  successful `source.open(&reopen_spec)` — same event the attach path already
  uses (`pane.rs:1093-1100`), so the loop has ONE degrade notification for
  both paths.
- **Record the degrade moment.** `handlers/daemon_lifecycle.rs::handle_session_fallback`
  (`:254-279`) gains a loop-local `degraded_at: HashMap<u32, Instant>` insert
  (DrainCtx threads the map; loop locals in `run.rs` beside
  `shell_watchdog_fired`, `run.rs:5969-5975`) and an honest status line
  (distinguishing "reattached dead session, opened a fresh shell" from the
  existing relaunch-offer case, `:269-273`).
- **Watch degraded panes.** `startup_watchdog::tick` gains a second candidate
  set: panes in `degraded_at` whose screen is still byte-blank
  (`history_tail(1).trim().is_empty()` — the same precondition as
  `:70-77`) past `watchdog_deadline(remote)` for their tab (`loading/mod.rs:25`,
  8s local / 300s remote + the extend-once policy). Fires once per pane, then
  swaps via the existing `spawn_clean_shell_pane` (`run.rs:5112-5146`) and
  names the cause in the status line. Entries are dropped on pane exit
  (the exits loop already collects pane ids, `pty_drain.rs:287-294`).
  A resumed remote VM shell that prints within its window clears silently —
  no splash, no noise on the normal suspend/resume path.

This bounds every blank-respawn state at one watchdog window without touching
the splash machinery, and it makes the NEXT occurrence self-diagnosing (status
line + WARN log with pane/session/argv context).

## 2. Lane (2): a resurrected tab relaunches its remembered agent

### 2.1 The gap

`openspec/specs/agent/spec.md` ("The worktree remembers its agent") already
contracts: "session resurrection relaunches the remembered agent". It is not
implemented: both spec-resolving workers hard-code `"shell"`
(`materialize.rs:204-212`, `:242-250`; prewarm worker `run.rs:7854-7862` —
and prewarm **spawns** panes for sibling tabs/neighbouring worktrees,
`panes.rs:990-1020`, so a background-prewarmed worktree would never relaunch
either). The resume machinery exists in core and has **no callers**:
`thegn_core::agent_task::auto_resume_id` (`agent_task.rs:601-618`), the
`HarnessCaps::RESUME` seam (`harness.rs:50-53`, `resume_command` `:245-249`),
the bounded discovery walker `thegn_svc::sessions::discover`
(`sessions.rs:41-129`, `MAX_SESSIONS = 500`, worktree-filtered), and the
resume-command composer `daemon/agent_open.rs::command_for`
(`:133-161`, id-shape-validated, refuses non-RESUME harnesses).

### 2.2 Design

New module `crates/thegn-host/src/handlers/worktree_launch.rs` (off-loop
callers only) with one decision function:

```rust
/// THE-84: when the attach probe found NO live session for this worktree,
/// a full bring-up relaunches the worktree's remembered agent as the FIRST
/// missing leaf's process — resuming its last harness session when the
/// [[agents]] entry opted in (`resume = true`) and the harness advertises
/// RESUME. `None` ⇒ keep the resolved shell spec.
pub(crate) fn remembered_agent_relaunch(
    cfg: &Config, worktree: &str, leaf: u32,
) -> Option<(u32, LaunchSpec)>
```

Decision ladder (each gate fail-open to `None` = today's shell):

1. `Db::open().worktree_agent(worktree)` → name. `None`/empty/`"shell"`/
   `"clean-shell"`/a tool drawer (`cfg.tool_command(..).is_some()`) ⇒ `None`
   (same exclusions as the native-exec path, `panes.rs:862-868`).
2. Entry not configured (`cfg.agent_command(name).is_none()`) ⇒ `None`
   (stale record after config churn — shell, record left for re-add).
3. Compose the spec via `direnv_warm::launch_spec_synced_with(cfg, worktree,
None, name, LaunchExtras { cmd_override, suppress_agent_record: true, .. })`
   — full sandbox/credential/cap parity by construction (the same call shape
   `agent_open::resolve` uses, `daemon/agent_open.rs:94-113`). The second
   bounded direnv warm is a cached no-op (the shell resolve just warmed it).
4. Resume: only when the entry's `resume == true` (cheap config read FIRST —
   the filesystem walk never runs for non-opted entries):
   `sessions::discover(cfg, SessionFilter { worktree: Some(wt), ..}, known)`
   → newest id → `agent_task::auto_resume_id(cfg, name, Some(id))` →
   `command_for(cfg, name, "", false, Some(id), None)` as `cmd_override`
   (`pub(crate)` visibility bump — one word). `auto_resume_id` returns
   `None` ⇒ cold launch, exactly the entry's interactive command.
5. `suppress_agent_record: true` — resurrection is not a choice event; the
   record already holds this agent, and a relaunch must never be able to
   _change_ it.

Call sites (both materialize shell branches `materialize.rs:204-212`/`:242-250`
and the prewarm worker `run.rs:7854-7862`): resolve shell specs as today, then
`if attach.is_empty() && !quiet { if let Some((leaf, spec)) =
remembered_agent_relaunch(...) { specs.replace leaf } }`. `!quiet` keeps
splits/adds as shells (a split is a shell gesture); the attach probe already
ran beside the spec resolve (`materialize.rs:252-260`, `run.rs:7864-7874`), so
a live session still wins and the agent is never doubled.

Properties:

- The relaunched session is **worktree-tagged** end-to-end
  (`spawn_argv_env` → `LazyDaemonSource.worktree` = cwd → `OpenSpec.worktree`
  → `SessionMeta.worktree`, `panes.rs:416-486`, `daemon/service.rs:451-468`),
  so the NEXT tab open attaches to it via THE-85 instead of relaunching again.
- Escape hatch: picking `shell` in the launch menu/wizard records `"shell"`
  (`handlers/launch.rs:130-143` `compose_choice` — deliberately unsuppressed),
  which gates relaunch off in (1). The wizard's record
  (`wizard.rs:164`) behaves the same.
- No config keys, no help-page actions, no spec deltas: the relaunch
  implements the existing `agent` spec requirement verbatim; the watchdog
  change has no openspec capability (checked — no watchdog spec exists).

## 3. Lane (3): remaining `shell`-over-agent record paths

Full audit of `worktrees.agent` writers — `set_worktree_agent` is called from
exactly four sites (grep, this tree):

| Site                                      | Guard                                                                 | Verdict                       |
| ----------------------------------------- | --------------------------------------------------------------------- | ----------------------------- |
| `agent.rs:3055-3066` (`launch_spec_full`) | `suppress_agent_record \|\| choice == "clean-shell" \|\| tool drawer` | guarded write; callers decide |
| `wizard.rs:164`                           | wizard choice                                                         | deliberate                    |
| `run.rs:10155` (preset bind)              | first preset agent                                                    | deliberate                    |
| `daemon/agent_open.rs:118`                | `--bind`                                                              | deliberate                    |

Unsuppressed `"shell"` callers of `launch_spec*` (i.e. the ones that hit the
guarded write with `choice = "shell"` and record it):

1. **`run.rs:5177` — `prewarm_sandbox_chain`** (kicked by ApplyLayout and
   ImportLayout, `run.rs:20342`, `:20362`): `launch_spec(&cfg, &wt, None,
"shell")` with `LaunchExtras::default()` ⇒ **records `shell`**, clobbering
   a remembered agent, as a side effect of warming. THE-85's D4 missed this
   site.
2. **`main.rs:1219` — the `sandbox-argv` verb**: a read-only debug print that
   resolves through `launch_spec(...)` ⇒ **records `shell`** as a side effect
   of _reading_ the argv.

Fix: both pass `suppress_agent_record: true` (`launch_spec_center_with` for
the prewarm — daemon-persistent correctness for a daemon-routed box — and
`launch_spec_full(..., suppress)` for the verb). The prewarm resolution moves
behind a tiny `agent::prewarm_spec` helper so the no-clobber property is unit
tested (mirror of `agent_tests.rs:474-527`).

## 4. Chunk plan (Lead-parallelization notes)

| Chunk                         | Files                                                                                                                                                       | Commit subject                                                                   |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| 1 — remembered-agent relaunch | `handlers/worktree_launch.rs` (new), `handlers/mod.rs`, `handlers/materialize.rs`, `run.rs` (prewarm worker call site), `daemon/agent_open.rs` (visibility) | `feat(the-84): a resurrected tab relaunches its remembered agent (resume-aware)` |
| 2 — bound the blank degrade   | `pane.rs`, `handlers/daemon_lifecycle.rs`, `handlers/startup_watchdog.rs`, `run.rs` (ctx threading)                                                         | `fix(the-84): a degraded daemon session is watchdog-bound, not blank forever`    |
| 3 — record-path audit fixes   | `run.rs:5177`, `agent.rs` (`prewarm_spec`), `main.rs:1219`, `agent_tests.rs`                                                                                | `fix(the-84): the last shell paths stop overwriting the remembered agent`        |

**All three touch `run.rs` ⇒ strictly serial, order 1 → 2 → 3** (chunk 2/3's
run.rs hunks are disjoint regions, but the Lead's rule is file-level). Each
chunk is otherwise self-contained: scoped tests (`just quick thegn-host` +
`cargo nextest run -p thegn-host <filter>`), no cross-chunk type dependencies
except the run.rs ctx plumbing in chunk 2 which lands wholly inside chunk 2.

Ratchet notes: no new color/glyph literals, no platform `#[cfg]`, no async
fn in provider traits, no new ignored `Result`s (DB reads are Option-chained),
no new actions/keybinds (help ratchet untouched), no god-file growth beyond
one-line call-site edits in `run.rs` (new logic in new/sibling modules, per
CLAUDE.md).

## 5. Test & done criteria

- Chunk 1: unit tests in `worktree_launch.rs` (decision ladder incl. resume
  gating with a fake harness home; record never changes) + a materialize
  worker-level test asserting the first missing leaf carries the agent argv
  when the probe is empty and the shell argv when it is not.
- Chunk 2: `startup_watchdog` unit test for the degraded-pane candidate
  (blank past deadline ⇒ swap; output before deadline ⇒ no swap); a
  `daemon_lifecycle` test that a fallback records `degraded_at` and the
  status names the respawn; `pane.rs` relay test that the ladder reopen emits
  `SessionFallback`.
- Chunk 3: `agent_tests.rs` regression — prewarm resolution and the
  sandbox-argv resolution leave `worktrees.agent` untouched
  (mirror of the THE-85 D4 test).
- Whole lane: `just quick thegn-host` per edit; `just test` (pre-push) once at
  the end; **no e2e** (per Lead), `just e2e-update` is NOT needed — no frame
  shapes change (status-line strings only, which e2e freezes already pin
  verbatim or tolerate; verified the new status strings only replace existing
  fallback statuses).
