# Add session snapshot hardening (scrollback + stale-state guard)

## Summary

Harden session restore with two details borrowed from
[`jmux`](https://github.com/jarredkenny/jmux)'s snapshot design:

1. **Per-pane scrollback capture** — persist a bounded tail of each pane's
   scrollback on snapshot and repaint it on restore, so a resurrected pane shows
   its recent history instead of an empty screen.
2. **Stale agent-state coercion** — on restore, downgrade a persisted "running"
   agent/activity state that is older than a grace threshold to a settled state,
   so a session that was killed mid-run doesn't come back showing a phantom
   forever-running agent. This generalizes the existing `RESUME_GRACE_SECS`
   sticky-state logic into an age-based guard applied at resurrection.

## Impact

- **I 114** (per-session snapshots) / **I 116** (restore agent state) /
  **I 117** (restore to exact position) — makes restore faithful (history shown)
  and self-correcting (no phantom running state).
- Relates to the activity-dot sticky-state work (`activity.rs`,
  `RESUME_GRACE_SECS`): the same age reasoning, applied once at restore.
- Extends the `state-db` capability. **DB schema change: `user_version` bump**
  (a scrollback-snapshot column on the tab-group table + a dispatch timestamp/TTL
  for stale coercion).

## Rationale

superzej already persists worktrees, tab layouts, pane cwds/cmds, and provider
sessions, and restores to the exact position. Two gaps remain versus jmux, which
captures per-pane scrollback and runs `coerceStaleAgentState` (a "running" agent
with no lifecycle signal past ~10 min is treated as complete on restore). Without
scrollback, a restored pane is blank until it produces new output; without a
stale guard, a crash mid-run resurrects a dot that says an agent is working when
nothing is. Both are small, deterministic, unit-testable, and shell-level — they
harden the restore path the one-session model depends on.

## Non-goals

- **Full unbounded scrollback persistence** — only a bounded tail (configurable
  cap) is captured; superzej is not a session recorder (that is the separate
  time-travel replay feature).
- **Reviving the process itself** — scrollback is repainted for context; whether a
  pane re-spawns its command follows the existing restore rules, unchanged.
- **Changing the live sticky-state machine** — the stale guard runs only at
  resurrection; the live `RESUME_GRACE_SECS` behavior is untouched.
- **Any AI dependency** — scrollback is shell content; the stale guard degrades an
  agent indicator but never requires the AI layer to be present.
