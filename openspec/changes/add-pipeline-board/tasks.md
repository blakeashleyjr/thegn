# Tasks — add-pipeline-board

## 1. The adopt drain (thegn-host)

- [x] 1.1 `handlers/adopt.rs`: a pure `plan(...) -> AdoptPlan` deciding
      graft-vs-drop (malformed / stale /
      already-shown / no-group) — **unit tests**: worktree intent lands in that
      group; worktree-less intent lands in the active group with `focus` false
      by default; stale row dropped (and the cutoff boundary + a future-stamped
      row are both fresh); a session already on screen is not adopted twice; an
      unknown worktree drops with its path; malformed/empty payloads drop; an
      out-of-range active index drops instead of panicking.
- [x] 1.2 `handlers::adopt::graft`: attach through the EXISTING door —
      `Panes::spawn_daemon_backed(argv, cwd, &[], center, Some(session))`, the
      warm-reattach call — and split it into the target group's active tab.
      Fallback argv is the pane shell, so a dead session degrades to a fresh
      daemon shell exactly like a stale warm-reattach.
- [x] 1.3 `hydrate::build_model` claims `take_intents("adopt_session")` into
      `FrameModel::adopt_intents`; `run.rs` collects **all** rows (not last-wins,
      unlike focus/preset) and applies them after the focus + preset intents.
- [x] 1.4 `[daemon] enabled = false`: report once and leave the sessions
      headless rather than dropping the rows silently.

## 2. Pure board rows (thegn-host)

- [x] 2.1 `monitor_pipeline.rs`: `ordered_rows(dispatches, stage_order, now_ms)`
      — stage grouping (config order → unknown by name → `unstaged` last),
      oldest-first within a stage, parent-indent for chunk rows, orphan/cycle
      degradation, capped depth — **unit tests**: empty roster; config order vs
      unknown vs unstaged; within-stage ordering + single group head; child
      indentation order; orphan parent (pruned / cross-stage / self) renders as
      a root; a 2-cycle terminates and lists every row once; deep chain caps its
      indent; blank/whitespace stages group as unstaged; no config order ⇒
      alphabetical; row fields (glyph, basename, session, age, issue); age
      formatting across units incl. negative.
- [x] 2.2 `stage_badges` / `stage_blocked` folds (newest **active** row per
      worktree; terminal rows never tag) — **unit tests** for both.
- [x] 2.3 `stage_order(cfg)` seam with `TODO(pipeline-config)` — the one line
      `add-pipeline-config-and-skill` flips when `Config.pipeline` exists.

## 3. The Pipeline tab (thegn-host)

- [x] 3.1 `MonitorTab::Pipeline`: enum + `ALL` (9→10) + `label` + stable `key`
      slug + `widget_id` + `present`/`visible` gated on roster-non-empty OR
      pipeline-configured + list-tab cursor (`row_len`, `clamp_sel`, `nav`) +
      rebuild-time `pipeline_rows` cache + footer legend. **Test**: the tab is
      hidden with no roster and no config, present with either.
- [x] 3.2 `monitor/build.rs::pipeline`: one table per stage with a group
      heading carrying its live count; status tone from the dispatch status;
      indent per chunk depth. No color/glyph literal (tones are `Tok::Slot` /
      `Tok::Hue`; the glyph comes from `AgentDispatchStatus::glyph`).
- [x] 3.3 `MonitorAction::Pipeline(PipelineJump)` + `pipeline_key` (`Enter`),
      dispatched in `run.rs` through
      `handlers::sidebar_activate::activate_row_target` —
      **unit tests** on `monitor_action::pipeline_target`: resolves a worktree
      row to its tab target; an unknown / targetless / wrong-kind row resolves
      to nothing (and the loop reports it rather than doing nothing).

## 4. Hydration + render invariant (thegn-host)

- [x] 4.1 `RefreshKind::Dispatches(Box<DispatchRoster>)` + the `run.rs` drain arm
      beside `RefreshKind::Model`; the model swap carries `dispatches` over
      (the `containers` precedent) so hydration never blanks the board.
- [x] 4.2 `monitor_action::spawn_dispatch_sample` — off-loop, Background QoS,
      one waker pulse, no timer/thread. Loop gate: seed once, on roster change,
      and on a 2s cadence only while `MonitorOverlay::wants_dispatches`.
- [x] 4.3 `pty_drain`'s dispatch stamp marks the roster dirty
      (`monitor_pipeline::mark_roster_dirty`) — a flag, not a channel, since the
      stamp runs in `spawn_blocking` with no sender and the exit already dirtied
      the frame. **Unit test**: the flag is claim-once.
- [x] 4.4 **Render invariant tests** in `render_plan.rs`: a roster update is a
      bounded diff (`Incremental { sidebar }` / `Incremental { panes }`) and an
      unchanged sample is `Skip` — never `Full`; plus the complementary pin that
      an OPEN board takes the pre-existing overlay rule like every other modal.

## 5. Sidebar stage evidence (thegn-host + thegn-core)

- [x] 5.1 `SidebarStatus::pipeline_stages` filled in
      `attention_status::collect_attention` (beside the merge-queue read, off
      the loop); `SidebarRow::pipeline_stage` denormalized in the same
      `build_rows` pass as `mq_status`.
- [x] 5.2 `sidebar_view::stage_tag` — faint, truncated, immediately after the
      activity dot (the `row_is_blocked` precedent). No new `ActivityState`.
- [x] 5.3 `AttentionInputs::stage_blocked_since` scoring at the EXISTING
      `(Blocked, 0, AgentNeedsInput)` coordinates so a `waiting_human` stage row
      feeds the existing blocked evidence — **unit test** (thegn-core, 95% gate):
      it scores blocked with an honest `since`, absent by default, and outranks
      a merely-finished agent. No new `AttentionReason`, no new
      `NotificationKind` (`StageBlocked` stays phase 2).

## 6. Validation

- [x] 6.1 Scoped tests: `cargo nextest run -p thegn-host monitor::`,
      `monitor_pipeline`, `monitor_action`, `adopt`, `render_plan`,
      `sidebar`, `attention`; `cargo nextest run -p thegn-core attention`.
- [x] 6.2 `just quick thegn-host`, `just quick thegn-core`, `treefmt`.
- [x] 6.3 `openspec validate --all --strict`.
- [ ] 6.4 Pre-PR gate, run **once** when the whole three-change plan is in:
      `just ci` (the lander's full-workspace nextest is the authoritative check
      per land).

## Notes

- **No `ACTION_SPECS` entry, no keybind, no `docs/help/` change.** The board is
  reached by the existing monitor-open action and then the existing overlay tab
  keys (`Tab`/`Shift-Tab`, `h`/`l`, `←`/`→`, and the digit keys, which index the
  _visible_ tab list). `Enter` on a row is handled inside the overlay's own key
  dispatch, exactly as the Containers tab handles its `Enter`. So the seven-gate
  action checklist and the `panel:*` help-context ratchet do not apply — see
  design.md, "Gates this change satisfies".
- **No schema change**, so no `SCHEMA_VERSION` bump: every column read here
  landed with `add-pipeline-roster-stages` (v56).
- `MonitorTab::ALL` grew 9→10, which auto-sizes the overlay's per-tab `scroll`
  array and `MonitorPrefs`; `key()` stays a stable slug so saved preferences
  survive a relabel.
