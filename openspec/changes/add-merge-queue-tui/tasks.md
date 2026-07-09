# Tasks — merge-queue TUI management

## 1. Backend prep

- [x] 1.1 `DriveStep.worktree` (the panel-row patch key) passed at every
      `progress` site; CLI printer unchanged.
- [x] 1.2 Shared `merge_driver::rows_for_repo` (one repo-membership rule for
      CLI + host).

## 2. Loop wiring

- [x] 2.1 `handlers/merge_queue.rs`: `DriveMsg`, `spawn_drive`, `spawn_fold`
      (moved from `run.rs`), `drain_fold_results`/`drain_drive_msgs` +
      `DrainCtx`, `apply_step` (pure in-place panel patch).
- [x] 2.2 `drive_tx`/`drive_rx` beside the fold channel; drains called from the
      loop's refresh block.
- [x] 2.3 `Action::Integrate` body → `dispatch_integrate`; new
      `Action::DrainMergeQueue` → `dispatch_drain`; shared `arm_fold` guard on
      the single `fold_inflight` flag.

## 3. Panel section

- [x] 3.1 Queue rows carry `PanelHit::Row(MergeQueue, i)`; hint row lists the
      action keys.
- [x] 3.2 Section keys `a/A/x/l/r/c/D` → `section_key` (pure `row_action_for`
      status×key matrix + off-loop executors, optimistic panel patches).
- [x] 3.3 `chrome.rs` context hints updated (held at the ratchet ceiling).

## 4. Keybind + palette

- [x] 4.1 `open-merge-queue` default chord `Ctrl Alt q`.
- [x] 4.2 New `merge-drain` spec (palette, gated on `[merge_queue].enabled`).

## 5. Visibility

- [x] 5.1 Statusbar badge: red += `needs_human`, amber += `agent_running`,
      quiet dim chip for an idle-but-populated queue.
- [x] 5.2 Badge overlay: Enter focuses the row's worktree; `m` opens the
      section (`DetailAction::OpenMergeQueueSection`, intercepted by the loop).
- [x] 5.3 Sidebar detail-line MQ chip (`SidebarStatus.mq` filled by
      `collect_attention`; denormalized onto rows with attention).

## 6. Notifications

- [x] 6.1 Core kinds `queue_landed`/`queue_ready`/`queue_needs_human`
      (priorities info/notice/alert; exhaustive tests extended).
- [x] 6.2 Settled drive steps toast + route via `NotifyState::decide` →
      sound/desktop/inbox record.
- [x] 6.3 Documented in `config/config.toml.example` ([merge_queue] +
      [notifications.priority]).

## 7. Tests + gates

- [x] 7.1 Unit: `row_action_for` matrix, `apply_step` patch/materialize,
      `push_mq_badge` severity matrix.
- [x] 7.2 Full core+host suites green.
- [ ] 7.3 `just ci` (pre-PR gate, run once at the end).
- [ ] 7.4 Live TUI verification (enqueue, drain with fake agent, land/retry,
      badge/chip/inbox).
