# THE-363 — primary instructions (batch 10, codex restart)

Coalesce, cancel and bound file-preview jobs and renderer process trees. A previous agent committed its
plan (.thegn/maintenance/THE-363/agent-plan.md — read it) and died mid-implementation; its UNCOMMITTED
changes may be partial — `git diff` first, keep what is right, then complete.

Issue (data):

- crates/thegn-host/src/preview_pane.rs:29-51: unconditional detached spawn_blocking per fetch,
  unbounded result senders. run.rs ~6060-6066 unbounded channels; ~18192-18215 every Enter spawns a new
  fetch without cancelling the prior one. crates/thegn-host/src/rasterize.rs:57-101: pdftoppm/pdftotext/
  mmdc launched with no deadline, cancellation or process-tree containment.
  Required (smallest complete version):
- At most ONE active + ONE latest-pending preview job per view; older queued jobs dropped before they
  start; results generation-tagged so stale completions never mutate a newer preview.
- External renderers: own process group, total deadline, output cap, kill the whole group and REAP on
  timeout/cancel (follow the leader-pinning pattern in crates/thegn-host/src/platform/sound_process.rs —
  keep the leader unreaped with WNOWAIT until the group is drained/killed; never killpg after reaping).
- Bounded result channels; closing/replacing a preview cancels its owned work.
- No new timers or wake sources on the event loop; preview work stays off-loop.
  Out of scope: THE-362 per-job memory limits, THE-251 shared primitive (a shared runner is tracked as
  THE-729 — do not create a new crate). Tests: rapid preview changes keep at most the bounded job count and
  only the latest generation delivers; a never-exiting fake renderer (fake `sh` script) is killed and
  reaped by the deadline with no surviving descendant.
