# Merge Queue

## MODIFIED Requirements

### Requirement: Conflicts and gate failures are handed to a headless agent

When `conflict_handoff` is `"agent"` and an agent is configured — either as an
`agent_command` template or as the name of a configured `[[agents]]` entry — the
driver SHALL dispatch a headless CLI agent to fix a branch that has a textual
merge conflict or fails the test gate, running the agent in that branch's own
worktree with a task prompt describing the conflict paths or the gate output.
The prompt SHALL be rendered from a configurable template for that failure kind,
defaulting to thegn's built-in instructions when the user has not supplied one.
After the agent finishes, the driver SHALL re-attempt the fold; it SHALL retry up
to `agent_max_attempts` and mark the branch `needs_human` if it still cannot
land. The agent MUST NOT be relied on to merge into the target — thegn performs
the land itself, so the object-DB coherence guarantee and the merge guard hold.
Each agent invocation SHALL be bounded by `agent_timeout_secs`.

#### Scenario: The agent resolves a conflict and the branch lands

- **WHEN** a queued branch conflicts with the target and the agent resolves it in
  the worktree
- **THEN** the driver's re-attempt folds the branch clean and lands it

#### Scenario: The agent cannot fix it within the attempt budget

- **WHEN** the agent fails to make the branch landable within `agent_max_attempts`
- **THEN** the branch is marked `needs_human` and the target is left unchanged

#### Scenario: Agent handoff disabled defers instead

- **WHEN** `conflict_handoff` is not `"agent"` or no agent is configured and a
  branch conflicts or fails the gate
- **THEN** the branch is left `deferred` / `gate_failed` with its reason recorded,
  and no agent is run

#### Scenario: A custom prompt template replaces the built-in instructions

- **WHEN** a user configures a prompt template for the conflict or gate-failure
  kind and the driver dispatches the agent for that kind
- **THEN** the agent receives the user's rendered template instead of thegn's
  built-in prompt

#### Scenario: No configured template preserves today's prompt

- **WHEN** no prompt template is configured for a failure kind
- **THEN** the agent receives thegn's built-in prompt for that kind, unchanged

## ADDED Requirements

### Requirement: Prompt and command templates are validated before use

thegn SHALL validate agent prompt and command templates against the variables
available for their task kind, and SHALL report an unknown placeholder as a
configuration error rather than expanding it to nothing. Command templates MUST
use bare placeholders, because every substituted value is shell-quoted when the
command line is composed; a placeholder written inside quotes in a command
template SHALL be reported as a configuration error, since quoting it a second
time would deliver the value with literal quote characters attached.

#### Scenario: An unknown placeholder is rejected

- **WHEN** a configured prompt template references a variable that its task kind
  does not provide
- **THEN** configuration validation reports the unknown placeholder and names the
  variables that kind does provide

#### Scenario: A double-quoted placeholder is rejected

- **WHEN** a configured command template wraps a placeholder in quotes
- **THEN** configuration validation reports it, because the value is already
  shell-quoted during substitution
