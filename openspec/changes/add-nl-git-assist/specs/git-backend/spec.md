# Git Backend

## ADDED Requirements

### Requirement: A natural-language intent is translated to a typed git operation with explanation and warnings

superzej SHALL translate a natural-language git intent into a concrete git
operation mapped onto its typed operation surface, together with an explanation
and any danger warnings, using a model response in a fixed
`<command>/<explanation>/<warning>` contract. A command that does not map to a
known operation MUST be surfaced as unrecognized and MUST NOT be executed, and a
malformed response MUST error rather than execute.

#### Scenario: Prose becomes a typed operation with explanation

- **WHEN** the user describes "squash the last three commits"
- **THEN** superzej produces a mapped git operation with an explanation and any
  warnings, without yet executing it

#### Scenario: An unrecognized command is not executed

- **WHEN** the model proposes a command that does not map to a known operation
- **THEN** it is surfaced as unrecognized and nothing is executed

### Requirement: A proposed git operation executes only after an explicit confirmation

superzej SHALL show the proposed command, its explanation, and warnings and
execute it only after an explicit confirmation; it MUST also offer an explain-only
mode that returns the command, explanation, and safety assessment and never
executes. The proposal MUST be pre-checked against the current repository state so
warnings reflect real danger (e.g. a rebase onto the current base branch, a squash
with too few commits, or a history-rewriting force push).

#### Scenario: Execution requires confirmation

- **WHEN** a git operation is proposed and the user cancels
- **THEN** nothing is executed

#### Scenario: Explain-only never executes

- **WHEN** the user requests an explanation of an operation in explain-only mode
- **THEN** the command, explanation, and safety are returned and nothing is
  executed

#### Scenario: An unsafe operation is warned before confirm

- **WHEN** a proposed operation would rewrite already-published history
- **THEN** the confirmation prompt includes a danger warning before the user can
  confirm
