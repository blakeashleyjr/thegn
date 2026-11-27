# Workspace status refresh

## Requirements

### Requirement: Idle worktree status refresh is bounded

The host SHALL perform live git glyph refreshes only for the focused worktree
and worktrees with activity in the previous fifteen minutes. Other worktrees
SHALL retain their last-known cached glyphs until they become focused or active.
The process-wide glyph scan pool SHALL run at most two concurrent scans.

#### Scenario: Large dormant worktree registry

- **WHEN** the host is idle with a registry containing 1,000 worktrees
- **THEN** status scans SHALL be limited to focused or recently active worktrees
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
