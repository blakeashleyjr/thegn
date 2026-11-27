# Workspace status refresh

## Requirements

### Requirement: Idle worktree status refresh is bounded

The host SHALL use the busy background glyph refresh interval only for visible,
recently touched, pane-bearing, or agent-bearing worktrees. Other registered
worktrees SHALL use the quiet safety TTL and retain their last-known cached
glyphs between scans. The default busy interval SHALL be 30 seconds and remain
overridable with `THEGN_BG_MODEL_REFRESH_MS`. The process-wide glyph scan pool
SHALL run at most two concurrent scans.

#### Scenario: Large dormant worktree registry

- **WHEN** the host is idle with a registry containing 1,000 worktrees
- **THEN** scans on the busy interval SHALL be limited to visible, recently
  touched, pane-bearing, or agent-bearing worktrees
- **AND** other registered worktrees SHALL wait for the quiet safety TTL
- **AND** concurrent status scans SHALL not exceed two

### Requirement: Periodic CI refresh is focused

The host SHALL refresh CI run history only for the focused worktree during
periodic refresh. Background worktrees SHALL continue to use their cached CI
history until focused.

#### Scenario: Focused CI refresh

- **WHEN** a periodic CI refresh is requested
- **THEN** only the focused worktree SHALL trigger a provider refresh

### Requirement: Forge refreshes honor configured cache TTLs

Periodic UI refreshes for pull requests and issue trackers SHALL skip provider
calls while the focused worktree's cache is younger than the configured
`[pr] ttl_secs` or `[issues] ttl_secs`. Explicit issue refreshes SHALL bypass
the cache TTL.

#### Scenario: Focused provider cache remains fresh

- **WHEN** a periodic pull request or issue refresh is requested before its
  configured cache TTL expires
- **THEN** the cached result SHALL be reused without starting another provider
  call
