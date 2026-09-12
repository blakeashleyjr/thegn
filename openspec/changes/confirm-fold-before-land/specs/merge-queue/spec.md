## ADDED Requirements

### Requirement: Only target advancement authorizes a landed outcome

The system MUST distinguish prepared fold objects from successfully CAS-landed
commits. Failed or unadvanced folds MUST NOT trigger landed bookkeeping, lifecycle
cleanup, success messages, or automatic expiry sweeps.

#### Scenario: Gate fails before target advancement

- **WHEN** the union gate fails or cannot run
- **THEN** the target and candidate worktrees remain intact, the CLI fails, and
  bounded phase-tagged diagnostics are retained without unsupported branch blame.

#### Scenario: Base is already red

- **WHEN** a red union is bisected and the base gate fails
- **THEN** no candidate is classified as an offender.

#### Scenario: Manual integration versus automatic readiness

- **WHEN** automatic landing is disabled
- **THEN** automatic attempts remain Ready while explicit manual integration can
  land only after a successful gate and target CAS.
