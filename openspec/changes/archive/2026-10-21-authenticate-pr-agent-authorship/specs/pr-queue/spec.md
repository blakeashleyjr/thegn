## ADDED Requirements

### Requirement: Authenticate own-PR handoff authorship

When own-only PR policy is enabled, automatic agent handoffs SHALL require current provider-authenticated author and viewer stable identities for the selected location's repository and PR head. Missing or inconsistent proof SHALL hold without launching an agent or consuming an attempt or candidate claim. Repository namespaces and review-comment authors SHALL NOT substitute for PR authorship.

#### Scenario: Authored organization PR

- **WHEN** current author and viewer IDs match for the selected repository, PR and head
- **THEN** an organization-owned PR passes the ownership gate subject to existing policy gates

#### Scenario: Someone else authored a PR in the viewer's namespace

- **WHEN** the current author ID differs from the authenticated viewer ID
- **THEN** automatic handoff is held regardless of the repository URL owner

#### Scenario: Authority changes during preparation

- **WHEN** author, viewer, provider, repository, PR or head proof changes after preparation starts
- **THEN** revalidation holds before dispatch budget or claim consumption

#### Scenario: Legacy or unsupported ownership evidence

- **WHEN** author evidence is absent, malformed, bot/deleted, partial, or unsupported by the selected provider
- **THEN** own-only handoff remains held with an actionable diagnostic

#### Scenario: Explicit broader policy

- **WHEN** own-only policy is disabled
- **THEN** existing broader handoff policy remains effective without requiring a new GitHub identity proof

#### Scenario: Another owner changes a review task during preparation

- **WHEN** a preclaim identity or preparation gate holds
- **THEN** the stale invocation returns its diagnostic without updating or reparking the current durable task row

#### Scenario: Interactive review falls back to a headless agent

- **WHEN** a review handoff selects the headless fallback with own-only policy enabled
- **THEN** a blocking worker verifies the selected worktree, repository URL, PR number, branch and head through shared fresh authorship admission before sandbox preparation, and revalidates before launch
- **AND** unavailable or changed evidence holds without starting an agent or modifying a durable claim or attempt count

#### Scenario: Review feedback is pasted into an existing agent pane

- **WHEN** the handoff selects an existing live agent pane
- **THEN** feedback remains pasted without submission under the existing interactive behavior
