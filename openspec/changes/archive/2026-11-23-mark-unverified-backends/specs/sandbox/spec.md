# Sandbox

## ADDED Requirements

### Requirement: A backend whose runtime was never verified says so

thegn SHALL distinguish backends whose commands have been checked against the
real runtime from those that have not, and MUST report the difference wherever a
backend's availability is shown. A backend with no liveness check is reached by a
PATH probe alone, so its `ready` state means only that a binary exists — thegn
MUST NOT let that stand as a claim the sandbox will work.

An unverified backend SHALL remain selectable when a user names it explicitly,
and MUST NOT appear in the default backend chain. thegn MUST NOT guess a
runtime's verbs in order to clear the mark.

#### Scenario: Doctor marks an unverified backend

- **WHEN** `thegn doctor` reports a backend that has no liveness check and whose
  verbs were never tested against the real runtime
- **THEN** the row carries a caveat saying `ready` means the binary is on PATH
  rather than that panes will work, in addition to any state remedy

#### Scenario: The caveat does not displace the remedy

- **WHEN** an unverified backend is also not installed or not running
- **THEN** both the remedy for that state and the verification caveat are shown

#### Scenario: An unrunnable backend is not also called unverified

- **WHEN** an unverified backend cannot run on this OS at all
- **THEN** only its unsupported state is reported, because that decides the row
  regardless of verification

#### Scenario: Launching under one warns

- **WHEN** a worktree pane launches under an unverified backend
- **THEN** thegn warns that it was selected on PATH presence alone, and the
  launch proceeds

#### Scenario: A verified backend is unaffected

- **WHEN** a backend whose runtime has been verified is reported
- **THEN** no verification caveat appears and no warning is emitted
