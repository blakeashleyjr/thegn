# Review Gate

## ADDED Requirements

### Requirement: Findings carry a severity and an action

Every pipeline finding SHALL carry a `severity` in {info, warning, error} and an `action` in {auto-fix, ask-user}, and each node SHALL define an `auto_fix_limit` bounding how many auto-fix findings it may apply automatically. The `auto_fix_limit` is node-level policy attached to the graph node that produced the finding.

#### Scenario: Info-level mechanical finding

- **WHEN** a node produces an `info` finding with action `auto-fix` within its `auto_fix_limit`
- **THEN** the fix is applied automatically without parking

#### Scenario: Blocking finding parks for a decision

- **WHEN** a finding has action `ask-user`, or its node's `auto_fix_limit` is reached
- **THEN** the run parks at an approval-gate node and waits for an explicit resolution rather than self-applying

### Requirement: Review auto-fix is disabled by default

The review node SHALL default its `auto_fix_limit` to 0 so that review findings park for a decision, and MAY be re-enabled per repo or globally via configuration.

#### Scenario: Default review configuration

- **WHEN** a review finding is produced and review auto-fix has not been explicitly enabled
- **THEN** the finding parks for an approve/fix/skip decision instead of being applied

### Requirement: Findings resolve via approve, fix, or skip

A parked finding SHALL be resolvable by exactly one of approve (accept as-is), fix (apply the suggested change), or skip (drop it); the resolution surface SHALL be available both in the human review pane (including the `add-agent-steerable-review` panel, which MAY receive parked findings) and through a structured, non-interactive contract usable by the embedded agent.

#### Scenario: Human resolves in the review pane

- **WHEN** the user selects approve, fix, or skip on a parked finding
- **THEN** the finding is resolved accordingly and the run advances past the approval-gate

#### Scenario: Embedded agent resolves non-interactively

- **WHEN** the embedded agent submits a structured resolve action for a parked finding
- **THEN** the gate clears the same way a human resolution would, with no interactive prompt required

### Requirement: Gate state persists across restart

Pipeline runs and their findings SHALL be persisted in a cache table so a parked gate survives a restart, while git and the agent session remain authoritative; adding the table MUST bump the SQLite `user_version`.

#### Scenario: Restart with a parked gate

- **WHEN** superzej restarts while a run is parked at an approval-gate
- **THEN** the run and its findings rehydrate from the cache and the review pane shows the same parked state
