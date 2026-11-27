# Bound idle worktree refresh cost

Idle hydration currently examines and periodically scans every worktree in the
session, and CI refreshes also walk background worktrees. With large registries,
this creates avoidable status and provider work even when the user is focused on
one checkout.

## Change

Keep live git glyph refreshes focused on the active or recently active worktrees,
bound concurrent glyph scans, and scope periodic CI refresh to the focused
worktree. Add a high-cardinality idle CPU benchmark using the existing harness.

## Impact

Old glyph and CI cache entries remain available for background views and refresh
when their worktree becomes active. Dormant worktrees may show older cached
status until selected.
