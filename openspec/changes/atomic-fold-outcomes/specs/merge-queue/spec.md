## ADDED Requirements

### Requirement: Publish only observed final queue outcomes

The system MUST capture queue and registry observations before folding and MUST
persist only final outcome states atomically per row. Observable reassignment,
duplicate contradictory projections, or failed writes MUST NOT trigger lifecycle.

#### Scenario: A failed fold becomes an infrastructure hold

- **WHEN** a prepared fold is held
- **THEN** another connection cannot observe a synthetic queued state during final
  persistence, and the final row clears stale result fields without resetting its
  nomination time or attempt budget.

#### Scenario: Another actor changes the queue during the gate

- **WHEN** the current row differs from the captured pre-fold observation
- **THEN** the delayed outcome refuses without overwriting that actor or applying
  lifecycle to the reassigned worktree.

#### Scenario: Pre-fold observation is unavailable

- **WHEN** observation fails before Git work begins
- **THEN** the Git outcome is reported separately from degraded bookkeeping and no
  fresh post-fold observation authorizes persistence or lifecycle.
