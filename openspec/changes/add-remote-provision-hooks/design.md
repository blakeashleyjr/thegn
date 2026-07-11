# Design

## Hook contract (core)

A placement/env config gains ordered `provision` and `teardown` hook lists:

```
[env.<name>.provision]
hooks = ["./.thegn/provision.sh", "..."]
timeout_secs = 300
[env.<name>.teardown]
hooks = ["./.thegn/teardown.sh"]
```

Each hook is invoked with the task context in the environment
(`THEGN_TASK_ID`, `THEGN_REPO`, `THEGN_BRANCH`, worktree path), runs
sequentially, and fails the bring-up if any hook exits non-zero (respecting the
failover/halt policy already governing non-local envs). This composes with the
existing `Placement::ensure()`/`teardown()` — hooks run inside those lifecycle
points.

## Pool composition

A task that claims a warm spare (`claim_pool_spare`) skips provision (the spare is
already up); a fresh task runs the provision hooks. Teardown hooks run when the
task's placement is torn down (respecting pool return vs. destroy).

## Output + timeout

Hook stdout/stderr is captured off-loop and surfaced (a provisioning status line /
the notification path); each hook is bounded by `timeout_secs`. If per-task hook
results are shown in the UI beyond the live run, they persist in a small table
(`user_version` bump); otherwise they are transient.

## Invariants

- **Event loop**: hooks run off-loop (spawn_blocking / the existing placement
  bring-up path), status handed back over the channel + `TerminalWaker`; a timeout
  bounds each. No polling timer, no blocking exec on the loop.
- **Render**: provisioning status is a chrome `dirty` repaint (status line /
  notification). render_plan invariants unchanged.
- **State**: `user_version` bump only if hook results are persisted for the UI;
  otherwise none.
- **Additivity**: pure infra; no AI dependency. Failover/halt policy for non-local
  envs is reused, not re-invented.

## Alternatives considered

- **Only static provider up/down** — the status quo; insufficient for per-task,
  user-specific infra (the Emdash gap this closes).
- **Running hooks on the event loop** — rejected; violates the 0%-idle/no-blocking
  invariant. Hooks are off-loop with a timeout.
- **A bespoke provisioning DSL** — rejected; plain scripts with task context in the
  environment are the lowest-barrier, most portable contract.
