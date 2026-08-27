# Design — add-pipeline-board

## Context

What already exists (verified in-tree on this branch, which is the tip of
`add-pipeline-roster-stages`):

- `agent_dispatches` carries `stage`, `parent_id`, `session_id`,
  `artifact_path` (v56), and `dispatch_for_exit(worktree, session_id)` attributes
  a pane exit to the right row when several stages share one worktree.
- `MonitorTab::Containers` is a complete worked example of an overlay list tab:
  ~10 match arms, a rebuild-time row cache the key handler indexes, a
  `visible()` presence gate, a liveness atomic that gates its expensive sampling,
  and a `MonitorAction` escalation channel for work the overlay cannot do itself.
- `SidebarRow::mq_status` is a complete worked example of evidence
  denormalization: a map on `SidebarStatus`, filled off-loop in
  `attention_status::collect_attention`, copied onto worktree rows in
  `sidebar::build_rows`, consumed at one draw site.
- `Panes::spawn_daemon_backed(argv, cwd, env, center, attach)` is the **only**
  path by which a daemon-owned session becomes a pane. `materialize_with_specs`
  uses it with `attach = Some(session)` for warm-reattach.
- `take_intents(kind)` is claim-and-delete-in-one-transaction; `focus_workspace`
  and `launch_preset` are drained on the hydration pass and applied on the loop.

What did not exist: any consumer of `adopt_session`.

## The adopt drain

### Which door was reused

`handlers::adopt::graft` calls `Panes::spawn_daemon_backed(..., Some(session))`
— the same call the warm-reattach branch of `materialize_with_specs` makes. No
second attachment path was invented. The consequences follow for free: the
adopted pane is a `Stream` pane over `DaemonSource`, its relay connects _inside
the task_ (so the attach never blocks a frame), the reconnect ladder degrades a
dead session to a fresh daemon shell using the fallback argv, `snapshot.rs`'s
`capture_pane_sessions` records it into `tab.pane_sessions` with `provider =
"daemon"` at the next persist, and a restart therefore warm-reattaches it like
any other pane.

The pane is grafted as a `center.split(tab.focused_pane, Dir::Row, id)` in the
target group's active tab — the `actions::open_command_pane` shape.

### Staleness policy (the choice this change makes)

**Drain-all with an age cutoff.** `take_intents` already deletes every row of
the kind, so the drain is inherently drain-all — which by itself fixes the
unbounded accumulation (rows had no reader at all before). Drain-all alone is
wrong, though: a mailbox filled while no compositor ran would apply _all_ of it
at the next launch, spawning panes for sessions that died hours ago — and
because a dead attach falls back to a fresh shell, those would even look alive.

So a claimed row older than `MAX_ADOPT_AGE_SECS` (300s) is **dropped, not
applied**: claimed out of the table, logged at debug, forgotten. Five minutes
covers a session opened while the UI was mid-launch or mid-switch; it does not
cover an overnight backlog. A row stamped in the _future_ (clock skew) counts as
fresh, never as infinitely stale.

Three further drops, all pure policy in `handlers::adopt::plan` and unit-tested
without a session, a daemon or a PTY:

- **malformed** payload or empty session id;
- **already shown** — a live pane already carries that daemon session id
  (re-adoption would double the pane, and the mailbox is not idempotent);
- **no group** — the named worktree is not resident in this session. Grafting
  across a workspace would need a cold resurrect, which is not an adoption; the
  row is dropped with a status line naming the worktree, because silence is the
  one outcome the user cannot diagnose.

`focus: false` is honoured literally: a fan-out of eight stage agents moves
nothing. `focus: true` switches to the group at the end of the batch (once, not
per row).

With `[daemon] enabled = false` there is no session to adopt; the drain says so
once in the status line and leaves the sessions headless — the documented
no-compositor outcome.

## The board

**Overlay tab, not a panel `Section`.** Concretely cheaper: no `panel:*`
help-context ratchet entry, no `section_keys.rs` dispatch-parity test, no
`Section::height` accounting, and the tab/footer/scroll/confirm chrome plus the
`MonitorAction` escalation channel all come for free. A panel section only wins
if the surface must be persistently docked beside the code, which a supervision
board is not.

`crates/thegn-host/src/monitor_pipeline.rs` is pure: `ordered_rows(dispatches,
stage_order, now_ms) -> Vec<PipelineRow>` plus the two evidence folds
(`stage_badges`, `stage_blocked`) and the age formatter. Ordering rules, all
exhaustively tested: configured stages in config order → unknown stages by name
→ `unstaged` (NULL stage) last; within a group roots oldest-first by
`(dispatched_at_ms, id)` with each row's chunk children immediately after it,
recursively. A `parent_id` that is not a member of the same stage group (pruned,
cross-stage, or self-referential) makes the row a root there rather than making
it unreachable; the walk is an explicit stack with a `seen` set, so a
supervisor-written cycle terminates and still lists every row exactly once, and
indent is capped at 4.

### Stage order and the config seam

`monitor_pipeline::stage_order(cfg)` is the single seam onto
`[[pipeline.stages]]`. `add-pipeline-config-and-skill` (change 2 of the same
plan) adds `thegn_core::config_pipeline` and a `Config.pipeline` field; until it
lands this returns empty and `ordered_rows` falls back to alphabetical stage
names — a stable order, just not the org chart's. Marked `TODO(pipeline-config)`
so the switch is one line and cannot be missed.

## Render + event loop

**Damage channel: never `chrome`.** The board's feed touches
`FrameModel::dispatches` and nothing else; a fresh sample that equals the current
roster raises no damage at all. The sidebar half raises at most the `sidebar`
channel (stage tags are `SidebarStatus` data, so they ride the existing
`hydration_eq` diff, which is exactly how `mq_status` behaves). Locked by
`render_plan::a_roster_update_is_a_bounded_diff_never_a_full_recompose`: sidebar
damage ⇒ `Incremental { sidebar }`, no damage ⇒ `Skip`, pane output ⇒
`Incremental { panes }`. This is the invariant inherited from the superseded
fleet spec (design.md:49-53), re-pointed at the roster.

The complementary fact is pinned in the same file by
`an_open_board_takes_the_overlay_rule_like_every_other_modal`: while the board
is _open_ it is a boxed layer, so `Overlays::layers` forces `Full` — the
pre-existing rule every modal (Containers included) lives under. The invariant
above is about the **feed**, not about the open modal, and the second test makes
sure that is read correctly rather than quietly weakened later.

**Wake path: none added.** `RefreshKind::Dispatches(Box<DispatchRoster>)` is
produced by one-shot `spawn_blocking` tasks that pulse the existing
`TerminalWaker` once and exit. They run: once at seed (so the otherwise-hidden
tab can discover it has rows), on every roster change, and on a 2s cadence
**only while the board is the live view** (`MonitorOverlay::wants_dispatches`,
the `wants_container_stats` gate one surface over). A closed board costs nothing.

The change kick comes from `pty_drain`'s dispatch stamp, which runs inside
`spawn_blocking` with no refresh sender in reach: it sets a process-global flag
(`monitor_pipeline::mark_roster_dirty`) which the loop — already awake, because
the pane exit dirtied the frame — claims on its next turn. A flag rather than a
plumbed channel precisely so nothing new can wake the loop.

## Sidebar evidence

`SidebarStatus::pipeline_stages` (path → stage of that worktree's most recent
**active** roster row) is filled in `attention_status::collect_attention`,
alongside the merge-queue read it already does. Deliberately _not_ the board's
feed: the board samples only while its tab is live, and the tag must stay honest
with the board shut. A finished row's stage is history, which is why the fold
takes the newest **active** row rather than the newest row.

`SidebarRow::pipeline_stage` is denormalized in the same `build_rows` pass as
`mq_status` and painted at one draw site (`sidebar_view::stage_tag`), faint,
truncated to 6 columns, immediately after the activity dot: dot = "is it working
/ does it want you", tag = "on what".

**No new state anywhere.** A `waiting_human` roster row reaches the red dot
through `AttentionInputs::stage_blocked_since`, which scores at the existing
`(Blocked, 0, AgentNeedsInput)` — the same coordinates an `AgentAttention`
notification scores at. So `row_is_blocked`, the `✋` badge, the "Needs you"
popup and the `Alt a` ring all cover pipeline stages without touching the
`ActivityState` FSM, the `AttentionReason` set, or `NotificationKind`.
`NotificationKind::StageBlocked` stays phase 2.

## Gates this change satisfies (and the ones it does not trip)

- **No action, no keybind, no help context.** The board is reached with the
  existing monitor-open action and then the existing overlay tab keys —
  `Tab`/`Shift-Tab`, `h`/`l`, `←`/`→`, and the digit keys, which index the
  _visible_ tab list (`handle_key`'s `Char('1'..='9')` arm). `Enter` on a row is
  handled inside the overlay's own key dispatch, like the Containers tab's
  `Enter`. So: no `ACTION_SPECS` entry, no `run.rs` action arm, no `docs/help/`
  `actions:` claim, and no `panel:*` context — the overlay-tab path
  (`docs/extending/`: "new overlay tab → ~5 match arms + array entry, NO
  help-context ratchet") is exactly the one taken.
- **Reachability**: `Tab`/`Shift-Tab`, `h`/`l` and `←`/`→` always reach the
  board; the digit keys reach it whenever it sits in the first nine _visible_
  tabs, which is every machine that hides at least one hardware tab (no
  discrete GPU, no battery, no thermal sensors, …). A machine showing all ten
  tabs at once reaches it by `Tab` only — the digit keys are `1`-`9` and this is
  the first change to push `ALL` past nine. Widening them is a whole-overlay
  decision, not a board one, so it is deliberately left alone here; a test pins
  both halves (`the_pipeline_board_is_reachable_by_tab_cycling_and_by_its_digit`).
- **Activation is `Enter` only, not click** — because the monitor overlay has
  no mouse row path at all today (the Containers tab is `Enter`-only for the
  same reason; `MonitorOverlay::box_rect`/`wheel` exist but nothing in `run.rs`
  routes a mouse event to the overlay). Adding one is a whole-overlay change
  that would benefit every list tab, and inventing a board-only click path would
  be the second door this change exists to avoid. `pipeline_key` takes no key
  argument, so wiring a click to it later is a call, not a refactor.
- **Color/glyph literals**: none introduced at a draw site. Status glyphs come
  from `AgentDispatchStatus::glyph()` (core, shared with `dispatch list`); tones
  are `Tok::Slot`/`Tok::Hue`, the caps chokepoint vocabulary. No box-drawing or
  block literal, so `test/glyph-literal-ratchet.txt` is untouched.
- **thegn-core coverage**: the one core change (`stage_blocked_since`) carries a
  unit test.
- **No schema change**, so no `SCHEMA_VERSION` bump.
- **`MonitorTab::ALL` grew from 9 to 10**, which auto-sizes the overlay's
  per-tab `scroll` array and `MonitorPrefs`; `key()` is a stable slug
  (`"pipeline"`), never the label, so preferences survive a relabel.

## Alternatives considered

- **A panel `Section::Pipeline`** — rejected: pays the help-context ratchet and
  the section-keys parity test for a surface nobody needs docked.
- **A new `ActivityState::Staged` / `NotificationKind::StageBlocked`** —
  rejected: the activity FSM is a state machine about _observed process
  behaviour_, and stage is not that. Evidence beside the dot keeps the FSM
  honest, which is the same argument `row_is_blocked` already won.
- **Reading the roster inside `build_model` for the board too** — rejected: it
  would put a table read on every hydration tick to feed a surface that is shut
  99% of the time. The sidebar's tag _does_ ride hydration (it must be honest
  with the board shut) but it is a fold over one small table, beside the
  merge-queue read already there.
- **An age-cutoff-only adopt drain (leave rows in place)** — rejected: it keeps
  the unbounded-growth bug the drain exists to fix. Claim-and-delete is the
  mailbox's contract; the cutoff decides _apply vs discard_, not _delete_.
- **Adopting into a fresh tab rather than a split** — deferred: a split lands
  the agent beside the work, and a per-stage tab policy is a preference this
  change has no evidence to set.
