# Cross-worktree aggregation

## ADDED Requirements

### Requirement: The aggregation model accepts a project scope

The pure aggregation model SHALL accept a project scope: excerpts collected
from the worktrees of a project feature set across the project's member
repos, not just from one workspace's worktrees. In project scope each
excerpt's display label MUST be repo-qualified so rows from different repos
remain identifiable, ordering MUST remain deterministic, and jump targets
MUST resolve to the owning worktree regardless of which member repo it
belongs to. Population MUST follow the existing rule: computed off the event
loop from the caches and delivered over a channel with a waker pulse.

#### Scenario: A feature set aggregates across repos

- **WHEN** the `tg/payments-retry` feature set spans worktrees in member
  repos `api` and `web` and both have aggregable items
- **THEN** one aggregation lists both worktrees' excerpts, grouped and
  deterministically ordered, with each row's label naming its repo

#### Scenario: Jump crosses workspaces

- **WHEN** the user activates an excerpt row owned by a worktree in a
  different member repo than the active one
- **THEN** the jump target resolves to that worktree's path and the session
  switches to its tab
