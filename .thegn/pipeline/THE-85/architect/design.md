# THE-85 — Attach a worktree tab to its live daemon agent session: architect design

Branch `tg/the-85-attach-live-session`. Written 2026-08-28 against `42984dc4`.

Everything below cites `file:line` from this worktree as of HEAD.

---

## 0. What is actually there today (evidence)

### 0.1 How a sidebar click opens a worktree tab

- A click/Enter on a worktree row lands in
  `crates/thegn-host/src/handlers/sidebar_activate.rs:14` (`activate_row_target`).
  A resident group is a pure focus move (`RowTarget::Tab` arm, :31); a
  non-resident row is a workspace switch (`RowTarget::Workspace` arm, :85 →
  `run.rs:1990` `switch_workspace` → cold resurrect via
  `switch_to_workspace_tab`, run.rs:1510).
- Which rows exist at all is decided by `gather_groups`
  (`crates/thegn-host/src/sidebar.rs:1385`): a **loaded** workspace lists only
  its live `session.worktrees` groups (→ `RowTarget::Tab`); a **dormant** one is
  synthesized from DB rows with `RowTarget::Workspace` targets (:1423–1450).
- Either way, the tab's **panes are spawned lazily**: while the active tab has
  missing leaves the loop kicks `maybe_materialize`
  (`run.rs:7968–8020` → `handlers/materialize.rs:59`), whose `spawn_blocking`
  worker resolves the launch spec — `launch_spec_synced(&cfg, &wt, None,
"shell")` (`materialize.rs:194`, `:222`) — and sends a `SpecBatch`
  (`handlers/provision.rs:15–21`) back; the loop drain `drain_specs`
  (`provision.rs:364`) calls `Panes::materialize_with_specs`
  (`panes.rs:724`), which spawns each missing leaf via `spawn_argv_env`
  (→ daemon-routed fresh session, `panes.rs:424` → `spawn_daemon_backed`,
  `panes.rs:484`). **That fresh login shell is the blank tab.**
- The neighbor **prewarm** worker pre-spawns the same shell specs for adjacent
  worktrees/tabs before focus (`run.rs:7756–7800`, targets from
  `panes.rs:964` `prewarm_requests`) — so by the time the user clicks, a
  worktree can already hold live shell panes and have **no missing leaves**.

### 0.2 The headless agent and the adopt intent

- `thegn session open --agent <role> --worktree W --prompt … --adopt --bind`
  (`cmd/session.rs:33–66`) calls the daemon's `sessions.open` + AgentLaunch
  composition (`daemon/agent_open.rs:27`), which resolves the same
  sandbox/credential-wrapped `LaunchSpec` an interactive pane gets and opens a
  daemon session whose `SessionInfo.worktree` carries the path
  (`thegn-svc/src/control/mod.rs:57–72`).
- `--adopt` writes an `adopt_session` intent row (`daemon/service.rs:559–573`).
  `--bind` stamps `worktrees.agent = <role>` (`daemon/agent_open.rs:108–114`),
  which is what resurrection and the sidebar's agent attribution read.
- **The adopt drain already exists** — the issue body and the `--adopt` help
  note (`cmd/session.rs:49–53`: "the compositor side of the graft is not wired
  yet") are **stale** on this branch. `handlers/adopt.rs` (landed in
  `7e65ffd8`) claims the rows (hydrate fetch at `hydrate.rs:2486`, model field
  `chrome.rs:433–439`, loop drain `run.rs:8989–9015` + `:9399–9411`) and
  grafts via `adopt::apply` → `graft` (`adopt.rs:136`, `:227`) — a
  `spawn_daemon_backed(attach = Some(sid))` split into the worktree group's
  active tab.
- The gap that matters here: an intent for a worktree that is **not a resident
  group** is dropped with `DropReason::NoGroup` (`adopt.rs:112`, status at
  `:182–186`). A pipeline worktree the user never opened has no resident group,
  so `--adopt` cannot help — the session stays headless until someone runs
  `thegn attach`.

### 0.3 The attach mechanism that already exists (and the precedent to reuse)

- There is exactly ONE way a daemon session becomes a pane:
  `Panes::spawn_daemon_backed(argv, cwd, env, center, attach: Some(sid))`
  (`panes.rs:484–540`), which builds `ExecOpen::Attach { session, cols, rows,
fallback: spec }` (`:519–524`) — the relay task attaches inside the task
  (never on the loop) and, if the session is already dead, **degrades to a
  fresh session running the fallback spec** (`pane.rs:1004–1036`, the
  `SessionFallback` contract).
- The warm-reattach branch of `materialize_with_specs`
  (`panes.rs:761–800`) already uses this for a _persisted_ `pane_sessions`
  record with `provider = "daemon"`. A headless CLI-opened session has no such
  record — that is the only reason it is invisible.
- `handlers/adopt.rs::graft` (`:227–273`) is a ready-made "attach session as a
  split leaf in a group's tab" primitive.
- The client side: `daemon/client.rs::connect_daemon` (discovery **without**
  spawning, `:29–58`), `ControlClient::sessions()`
  (`thegn-svc/src/control/client.rs:125`). `list_sessions` also returns
  tombstones (`daemon/service.rs:381–411`), so "live" must be
  `exited_at_ms.is_none()` (`control/mod.rs:73–84`).

### 0.4 The THE-84 side effect: opening clobbers `worktrees.agent` with "shell"

- Every plain-shell spec resolution records its choice as the worktree's
  remembered agent (`agent.rs:3040–3052`) unless `suppress_agent_record` is
  set, the choice is `clean-shell`, or it names a configured tool.
- `launch_spec_center` / `launch_spec_synced` are invoked with
  `LaunchExtras::default()` (`agent.rs:2980–2995`, `direnv_warm.rs:70–83`) at
  every shell materialize (`materialize.rs:194`, `:222`), prewarm
  (`run.rs:7797`) and split spawn (`run.rs:5059` `spawn_worktree_shell_pane`).
  So opening a worktree — or merely prewarming it — overwrites the agent the
  wizard recorded at create (`wizard.rs:164`) or `--bind` recorded at dispatch.
  Resurrection then relaunches a shell instead of the agent.

### 0.5 What happens today when an attached session exits

- `PaneEvent::Exit` → `handle_exit` (`pty_drain.rs:542`): the pane leaves
  `panes.table` (`:575`); the daemon session id and program name are captured
  first (`:544–552`).
- **Sole leaf**: `prep_leaf_for_respawn` (`crash.rs:74–110`) drops the session
  record, restores the output tail into `pane_scrollback`, and keeps the
  remembered command **only when the exit failed** — then the materialize
  pipeline respawns a **shell** under the status "Pane exited; restarting
  shell…" (`pty_drain.rs:929–934`). The user's agent silently becomes a shell.
- **Non-sole leaf**: `tab.center.remove(id)` (`pty_drain.rs:938–946`) — the
  pane vanishes with no exit line at all.
- The relaunch affordance already exists and is reused below:
  `pending_relaunch` arms an Enter-runs-command / Esc-dismisses interception
  (`run.rs:21351–21395`), fed by `materialize_with_specs`'s respawn path
  (`panes.rs:884–893`) and `handle_session_fallback`
  (`handlers/daemon_lifecycle.rs:254–279`).

### 0.6 The off-loop pattern for daemon reads (the "channel-fed model")

- The only daemon session list the loop ever sees is the status modal's
  on-demand probe: `handlers/status.rs::probe_sessions` — a tokio task calls
  `connect_daemon` + `sessions()` off the loop and delivers via
  `RefreshKind::DaemonSessions` (`hydrate.rs:302`) to the drain at
  `run.rs:10680`. No timer, no poll, socket touched only when work is in hand.
- The materialize/prewarm workers are `spawn_blocking` tasks; the whole
  compositor runs inside `rt.block_on` (`main.rs:939`) and the loop already
  hands `Handle::current()` into long-lived machinery (`run.rs:1778/1797`,
  `Panes`' `rt`). A worker can therefore `handle.block_on(...)` a sessions
  probe — off-loop, like every other blocking resolve it already does
  (`launch_spec_synced` runs SQLite + sandbox probes on the same threads).

---

## 1. Decisions

### D1 — Attach happens in the spec pipeline, at materialize — not per click

One rule, stated once: **whenever a tab's missing leaves materialize, live
daemon sessions for the worktree claim the leaves (newest first) before any
shell spawns.** This covers the click-to-open path (0.1), the prewarm path, the
respawn path and the resurrect path, because they all converge on
`SpecBatch` → `drain_specs` → `materialize_with_specs`.

- New module `crates/thegn-host/src/handlers/worktree_attach.rs` (sibling
  module per the god-file guidance), holding:
  - `live_for_worktree(sessions, worktree) -> Vec<AttachTarget>` — **pure**
    filter/sort: `worktree` match, `exited_at_ms.is_none()`, newest
    (`created_at_ms`) first, deduped against sessions already shown by a live
    pane.
  - `plan(leaves, targets, max_panes) -> AttachPlan { assignments, surplus }`
    — **pure**: newest target → first missing leaf; the rest are surplus.
  - `probe(rt, dcfg, worktree) -> Vec<AttachTarget>` — the async daemon read
    (`connect_daemon`, never `ensure_daemon`: a materialize must not spawn a
    daemon as a side effect of asking a question).
  - `graft_surplus(...)` — splits surplus sessions into the batch's tab,
    reusing `adopt::graft` (generalized to take `(gi, ti)` so a prewarmed
    non-active tab is addressable; `adopt::apply` passes its active tab).
- `SpecBatch` (`provision.rs:15–21`) becomes a named struct carrying
  `attach: Vec<AttachTarget>` alongside the existing tuple fields.
- Both workers fill it: `handlers/materialize.rs`'s worker and run.rs's inline
  prewarm worker probe after resolving specs (probe skipped for terminal
  groups and when `daemon_route_enabled()` is false).
- `materialize_with_specs` gains an `attach: &[(u32, &AttachTarget)]`
  parameter. Its existing daemon branch (`panes.rs:761–800`) extends by one
  `or_else`: a leaf with no persisted record but a live assignment attaches
  with the resolved shell spec as the `fallback` — byte-for-byte the
  warm-reattach contract, so a session that dies in the race degrades via
  `SessionFallback` with the final screen restored, never into an error husk.
- Surplus is grafted in `drain_specs` after `materialize_with_specs` returns,
  capped so the tab stays within `MAX_PANES` (`run.rs:18699`); the remainder
  is named in `model.status` ("N more live agent sessions — `thegn attach
<id>`").

Why not a probe-per-click channel (the `RefreshKind` shape): it would race the
materialize worker (the shell could spawn before the probe lands, requiring an
in-flight gate on the materialize kick and a leaf-consumption story on the
reply). Folding the probe into the worker that already owns the slow resolve
keeps the decision atomic, keeps every daemon call off the loop, and adds no
new channel, no new wake source and no new inflight set.

The render contract is untouched: attaching is a pane spawn — the drain marks
`need_relayout` (splits) / `dirty` exactly as today's spawns do, so
`render_plan::plan` still sees only its existing `Skip`/`Panes`/`Full` inputs.
No color/glyph literals, no new `poll_input` site, no platform `#[cfg]`.

### D2 — Several live sessions: each becomes a pane in the tab's split (issue option 1)

Newest session takes the primary missing leaf (it is the most likely to still
be streaming); each older session becomes a right split via
`adopt::graft`'s mechanism, capped by `MAX_PANES`; anything beyond the cap is
reported in the status line rather than silently dropped. Sessions already
shown by a live pane are never attached twice (the `AlreadyShown` rule,
`adopt.rs:120–122`).

### D3 — An attached agent's exit leaves a screen + an exit line, never a silent shell

In `handle_exit` (`pty_drain.rs:542`), classify the exit: the pane was
daemon-backed **and** its program is not a routine shell
(`pane.rs:290` `is_routine_pane`) ⇒ an **agent session exit**.

- **Sole leaf**: still leave the leaf for the materialize respawn (existing
  shape), but keep the remembered command **even on a clean exit** (new
  `keep_cmd` flag on `prep_leaf_for_respawn`, `crash.rs:74`) so the respawned
  shell arms the Enter-to-relaunch overlay, and set the status from a new pure
  helper `crash::agent_exit_status(program, code)`:
  `agent <program> exited (code N) — Enter: relaunch · Esc: shell`. The
  respawned leaf repaints the final screen (`repaint_scrollback`,
  `panes.rs:877–883`) — final screen + exit line + keybind, all existing
  machinery. On a native-exec worktree the existing remembered-agent respawn
  (`panes.rs:813–830`, which reads the `--bind`-recorded `worktrees.agent`)
  relaunches the agent outright; D4 is what keeps that record meaningful.
- **Non-sole leaf**: remove the pane as today (a fan-out tab must not amass a
  husk per finished stage) but announce it — same `agent_exit_status` line.
  Documented tradeoff against the issue's "leaves its final screen": in a
  split the collapse is visible and announced; the sole-pane case is the
  reported bug and gets the full husk treatment.
- Plain shell exits (routine panes) keep today's behavior exactly.
- No new `ACTION_SPECS` entry: Enter/Esc on the armed overlay is the existing
  interception (`run.rs:21351–21395`), so the help ratchets are untouched.

### D4 — `worktrees.agent` is never overwritten by a plain shell

Every `"shell"` spec resolution passes `LaunchExtras { suppress_agent_record:
true, ..default }` through the existing `launch_spec_synced_with`
(`direnv_warm.rs:89`) and a new thin `launch_spec_center_with` (agent.rs,
mirroring `:2980`): the materialize worker (`materialize.rs:194`, `:222`), the
prewarm worker (`run.rs:7797`), and `spawn_worktree_shell_pane`
(`run.rs:5059`). A shell is not a choice of agent — the record is owned by the
wizard picker (`wizard.rs:164`), the launch menu, and `--bind`
(`agent_open.rs:108–114`). This is what makes D3's native-exec relaunch
relaunch the agent rather than degrade to "shell", and it is the whole of the
issue's item (5).

The now-false `--adopt` note in `cmd/session.rs:49–53` is corrected to
describe the real contract (graft when the worktree is open here; attach on
open otherwise).

### D5 — `--adopt` stays the immediate-graft door; attach-on-open covers the rest

For a resident group, `session open --adopt` grafts immediately (already
true — `adopt.rs:136`). For a non-resident worktree the intent is still
claimed and dropped (`NoGroup`), and the session attaches the moment the
worktree is opened (D1) — which is the pipeline's actual sequence (dispatch,
then open). No re-queueing machinery, no second mailbox.

---

## 2. Non-goals

- No sidebar badge for "has live sessions" (would need a fed session list per
  row; the dispatch roster's session column already attributes activity).
- No footer/session-picker UI for surplus sessions — the status line names
  them; a picker is a follow-up if the cap ever bites in practice.
- No change to `[daemon] enabled = false` behavior: no daemon route ⇒ no probe
  ⇒ shells exactly as today (the drain-side claim tests at
  `provision.rs:688–751` keep pinning the disabled-route contract).
- No change to `attach` semantics for native/provider panes (their reattach
  ladder is untouched; only the daemon branch gains an input).

---

## 3. Chunk map

| Chunk | Scope                                                                                 | Files                                                                                                                                                                               | Commit subject                                                                                   |
| ----- | ------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| 1     | D1+D2+D4+D5 — attach-on-open, surplus splits, agent-record suppression, adopt doc fix | `handlers/worktree_attach.rs` (new), `handlers/mod.rs`, `handlers/adopt.rs`, `handlers/materialize.rs`, `handlers/provision.rs`, `panes.rs`, `run.rs`, `agent.rs`, `cmd/session.rs` | `feat(host): open worktree tabs onto their live daemon agent sessions (THE-85)`                  |
| 2     | D3 — exit-of-attached-agent husk + honest status                                      | `handlers/crash.rs`, `pty_drain.rs`                                                                                                                                                 | `feat(host): an attached agent's exit leaves a screen + relaunch, never a silent shell (THE-85)` |

The chunks are file-disjoint and may run in parallel; chunk 1 carries the core
mechanism and should be integrated first if serialized. Neither chunk adds
help-page actions, config keys, specs deltas or new platform code; both keep
`handlers/*` module shape and the run.rs additions limited to the existing
prewarm/materialize blocks.
