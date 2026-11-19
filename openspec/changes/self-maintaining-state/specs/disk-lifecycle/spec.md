# Disk Lifecycle

## MODIFIED Requirements

### Requirement: Reclaim runs off the event loop and explains itself

The size and line-count scanners SHALL go idle for any registry size: after a round each lane stores when its next row falls due and a pump arriving earlier returns before it opens the database or queues a job. The active worktree's `du` SHALL run at most once per 120 seconds; inside that window its row is re-stamped from cache.

#### Scenario: Large registry

- **WHEN** the registry holds 1,000 worktrees and nothing changed
- **THEN** the lanes reach an idle state after a bounded number of rounds and plan no further work until a row falls due
