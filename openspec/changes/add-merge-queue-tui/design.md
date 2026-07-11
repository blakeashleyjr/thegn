# Design — merge-queue TUI management

## Channel shape

`handlers/merge_queue.rs` owns a dedicated `DriveMsg` channel (not `fold_tx` —
the fold reports one `FoldReport`; the drive streams many steps + a summary):

- `Step { worktree, branch, status, detail }` — one driver transition (the DB
  row is already written when it fires). The loop patches
  `model.panel.merge_queue` in place (`apply_step`, pure + unit-tested) so the
  frame after the wake shows the new status; settled transitions additionally
  set `want_model_refresh` so attention/sidebar/badge recompute from the DB.
- `Done(DriveOutcome)` — clears the inflight flag, toasts the summary.
- `Note(String)` — one-line outcome of an off-loop queue mutation (add/rm/
  retry/clear), toasted; also used for failure messages of those mutations.
- `Failed(String)` — a drive (or land) that died before/at the driver level.

`drive_queue`'s sync `progress` callback bridges trivially: it runs on the
`spawn_blocking` thread and `UnboundedSender::send` + `TerminalWaker::wake`
are both sync and non-blocking — the same shape as `spawn_fold`.

## Mutual exclusion

The batch fold (`integrate`) and the queue drain (`merge-drain`, section `D`)
reuse the single `fold_inflight` flag, set by a shared `arm_fold` guard. Both
mutate the same target ref, so one flag makes the exclusion structural rather
than conventional. A section `l` (land) also arms the flag: it is a
fold+gate+CAS.

## Threading contract

Everything the section keys do (git symbolic-ref, candidate scan, DB upserts,
`land_branch`) runs on `spawn_blocking`; outcomes ride the drive channel (or
`RefreshKind::Model`) back with a waker pulse. The `drain_*` functions run on
the loop and are I/O-free; inbox records are themselves written on
`spawn_blocking` (best-effort — the queue row is the durable record).

## Quit-mid-drain

The fixing agent runs in its own process group under a plain-thread watchdog;
if thegn exits, the agent is orphaned (keeps running, unsupervised) and the
queue row is left at `folding`/`agent_running`. Accepted for this change:
retry/re-add resets the row via the enqueue upsert, and hydration must NOT
auto-reset transient rows (a concurrent CLI drain owns them). Documented in
the handler module doc.

## Notification mapping

`queue_landed` → Info (history), `queue_ready` → Notice (unread count),
`queue_needs_human` → Alert (red flag + desktop toast via the generic
`Event::NotificationReceived` path). Routed through `NotifyState::decide` like
`test_failed`, so rules/DND/modes apply uniformly.

## Phase 2 (out of scope, sketched)

A pr_view.rs-style full-screen `MergeQueueView` (Queue / Branch / Logs tabs)
entered by Enter on a section row. Prerequisite: capture capped gate/agent log
tails to `$XDG_STATE_HOME/thegn/logs/merge/<hash>-{agent,gate}.log` from
`run_agent` (deterministic path ⇒ no DB schema bump); its action keys reuse
this change's executors so it adds no new mutation paths.
