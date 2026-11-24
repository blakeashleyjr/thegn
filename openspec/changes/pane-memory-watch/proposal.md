# Pane memory watch and restart

## Why

Two overnight pane scopes sat pinned at their memory cap for hours, thrashing
into swap, then lost a process to the kernel with no warning beforehand and no
recovery afterwards (THE-2600). The per-pane cgroup already records everything
needed to see it coming.

## What Changes

- The host samples each local pane scope's memory files about every five seconds,
  off the event loop.
- Crossing 85% of the cap, or a rising `high` event counter, shows a toast with
  the numbers; it re-arms after usage falls below 75%.
- An `oom_kill` increment shows an OOM notice.
- A pane whose shell exits together with an `oom_kill` is restarted in place,
  relaunching a recognised agent with its continue command, at most once per
  ten minutes.

## Impact

Pane exit handling, materialize, help page `sandbox-resources`. No new
configuration key.
