## ADDED Requirements

### Requirement: Repository-wide open-PR polling is shared across worktrees

The UI SHALL refresh the root checkout's repository-wide open-PR list no more
often than `[watch].pr_interval_secs` for one repository, regardless of how
many of its worktrees are focused during that interval. The cache key and poll
gate SHALL use the root checkout. A focused worktree MAY trigger the refresh;
inactive worktrees and panel redraws SHALL NOT create additional polls.
Branch-specific PR detail remains cached and refreshed per worktree.

#### Scenario: Several worktrees share the open-PR poll window

- **WHEN** several worktrees from one repository become focused within one
  configured PR interval
- **THEN** the repository-wide open-PR list is fetched once, and the next fetch
  becomes eligible when the interval expires
