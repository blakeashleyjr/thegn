# Change Intent

## ADDED Requirements

### Requirement: Intent is derived from the session-to-worktree binding

Change intent SHALL be taken from the agent session bound to the worktree that produced the change, MUST NOT be reconstructed by scraping agent transcripts or by file-overlap heuristics, and SHALL be treated as absent when no bound agent session exists.

#### Scenario: Worktree produced by a bound agent session

- **WHEN** a change originates from a worktree with a bound agent session
- **THEN** the intent is the task/prompt of that session, available directly

#### Scenario: Hand-edited worktree with no agent session

- **WHEN** a change has no bound agent session
- **THEN** intent is absent and the review and PR proceed without it rather than failing

### Requirement: Intent informs review and the generated PR body

When intent is present, the review stage SHALL judge findings against it and the PR stage SHALL generate the PR body's intent/changes/risk/evidence sections from it, reusing the existing PR-creation path.

#### Scenario: Review with intent present

- **WHEN** the review stage runs with an available intent
- **THEN** findings are evaluated against the stated intent

#### Scenario: PR creation with intent present

- **WHEN** the PR stage opens or updates a pull request with an available intent
- **THEN** the PR body's intent/changes/risk/evidence sections are generated from that intent via the existing `pr create` path
