# Make the merge queue manageable from the TUI

## Summary

The merge queue's backend is complete (fold engine, `attempt_land`, the
agent-driven `drive_queue`, the `merge` CLI namespace), but managing it means
shelling out per branch: the panel section is read-only with no default
keybind, the statusbar badge is silent unless something is active or failing,
and the in-app "Integrate" action runs only the batch fold — never the queue
drain, never the conflict-fixing agent. This change completes the follow-ups
deferred by `add-agent-merge-driver` (its tasks 7.1/7.2) now that the
`run.rs`/`keymap.rs` god-file extractions have landed:

1. **Interactive panel section.** Work ▸ Merge queue rows carry a cursor hit;
   `a`/`A` enqueue (current worktree / all eligible), `x` removes, `r` retries
   a blocked row (the enqueue upsert resets it to `queued`), `l` lands a
   `ready` row, `c` clears landed rows, `D` drains. All mutations run on
   `spawn_blocking` and report back as toasts over the drive channel.
2. **In-app drain with the full agent autopilot.** A new
   `Action::DrainMergeQueue` (`merge-drain`, palette + section `D`) runs
   `merge_driver::drive_queue` off-loop; every status transition streams back
   over a `DriveMsg` channel with a waker pulse and patches the panel row **in
   place**, so `folding → agent_running → landed/needs_human` paints live
   instead of waiting for the model tick. Batch fold and drain share one
   inflight flag (they mutate the same target ref — mutual exclusion is
   structural).
3. **Visibility.** `open-merge-queue` gets a default chord (`Ctrl Alt q`); the
   sidebar detail line gains a per-worktree MQ status chip; the statusbar badge
   adds `needs_human` (red) and `agent_running` (amber) and shows a quiet dim
   chip for an idle-but-populated queue; the badge overlay's rows focus the
   worktree on Enter and `m` opens the section. Settled transitions toast and
   route to the inbox as three new notification kinds (`queue_landed` = info,
   `queue_ready` = notice, `queue_needs_human` = alert).

## Impact

- tasks.md: the same **T/Q** merge-pipeline groups as `add-agent-merge-driver`
  — this is its deferred in-TUI surface.
- **superzej-core** — three new `NotificationKind`s (+ exhaustive-test updates).
- **superzej-host** — new `handlers/merge_queue.rs` (spawners, channel drains,
  section keys; `spawn_fold` moved out of the pinned `run.rs`);
  `merge_driver::DriveStep` gains `worktree` (the panel-row patch key) and the
  repo-membership filter is shared with the CLI; panel/statusbar/sidebar/
  detail-overlay/keymap/palette wiring as above.
- **No new idle wake path** — the drive channel is drained on waker pulses like
  every other off-thread producer; render-plan invariants untouched.

## Non-goals

- **A full-screen MergeQueueView** (pr_view.rs-style tabs with gate/agent log
  drill-in) — phase 2; needs agent/gate log capture first (`run_agent`
  currently discards output).
- **`auto_drain` on agent completion** — still deferred.
- **Recovering a drain across restart.** Quitting mid-drain orphans the running
  agent (own process group) and leaves the row at a transient status; retry
  (`r`) heals it. Hydration never auto-resets transient rows because a
  concurrent CLI `merge drain` legitimately owns them.
