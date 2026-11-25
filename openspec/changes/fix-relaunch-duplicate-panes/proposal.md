# Fix relaunch duplicate panes

## Why

Quitting thegn detaches daemon-backed panes — the pane daemon keeps their
sessions — and relaunching warm-reattaches each one from its persisted
`pane_sessions` record. But the relaunch's worktree probe ALSO returns that
same live session (it is alive and carries the worktree), and the drain-side
attach plan de-duplicated probe targets only against live panes: no pane
shows the session yet, so it slipped through as a "headless" target and
`graft_surplus` split it in beside the leaf that had just reattached it. A
restored single-pane tab came back with a second pane and a different split
— the `sessions.py` demo clip's exact flow: type `thegn`, run the tick loop,
quit (`Ctrl+Q`, `y`), relaunch `thegn`, snap.

## What Changes

- The drain drops attach targets a persisted daemon record names: such a
  session reattaches onto its own recorded leaf through the existing
  warm-reattach branch, so it is not headless and must not enter the attach
  pool. Records keyed by a leaf that is no longer in the tree do not filter:
  their sessions are headless from the compositor's view and stay adoptable.
- Both drain paths share the filter: the materialize batch's target pool and
  the prewarm-skip adopt path.
- No probe, worker, or materialize change: the fix is the loop-side plan,
  where the `shown` re-dedup already lives. Every probe site funnels through
  `drain_specs`, so one filter covers every path.

## Impact

Affected specs: `control-plane` (relaunch-restore requirement added).

Affected code: `thegn-host` `handlers::worktree_attach` (pure helper + unit
tests), `handlers::provision::drain_specs` and its prewarm-skip attach
(filter + regression tests).

Roadmap: I 111–113 (detach/attach, resurrection after reboot, restore tree +
layouts).
