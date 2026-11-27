# Bound idle worktree refresh cost

Idle hydration currently examines and periodically scans every worktree in the
session, and CI refreshes also walk background worktrees. With large registries,
this creates avoidable status and provider work even when the user is focused on
one checkout.

## Change

Keep the busy glyph cadence focused on visible, recently touched, pane-bearing,
or agent-bearing worktrees; all other registered worktrees use the quiet safety
TTL. Raise the default background busy refresh interval to 30 seconds while
retaining its environment override. Bound concurrent glyph scans, scope periodic
CI refresh to the focused worktree, and add a high-cardinality idle CPU benchmark
using the existing harness.

## Impact

Old glyph and CI cache entries remain available for background views. Quiet
worktrees can show older cached status until the safety interval expires or they
become visible, active, pane-bearing, or agent-bearing.
