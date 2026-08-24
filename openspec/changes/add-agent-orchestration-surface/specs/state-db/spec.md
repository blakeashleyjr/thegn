# State DB

## ADDED Requirements

### Requirement: The agent-dispatch roster is durable and parseable

The state database's `agent_dispatches` table SHALL serve as the durable
orchestration roster — issue, worktree, agent, dispatch time, status — that a
restarted supervisor reads back to resume without re-dispatching. Statuses
SHALL be a closed, parseable set including terminal outcomes (done, failed)
alongside the lifecycle states, every writer MUST go through the typed status
(never a free string), and reads MUST tolerate legacy or unknown stored
strings by presenting them visibly rather than erroring — the
never-reset-user-data contract applies.

#### Scenario: A finished worker's row is parseable

- **WHEN** a dispatched agent's pane exits and the exit handler records the
  outcome
- **THEN** the stored status is a member of the closed set (done or failed) and
  round-trips through the status parser

#### Scenario: A supervisor resumes from the roster

- **WHEN** a supervisor restarts and lists dispatches
- **THEN** rows still running are distinguishable from finished and abandoned
  ones, so no running row is dispatched twice

#### Scenario: A legacy status string does not break the roster

- **WHEN** a row written before the closed set existed carries an unrecognized
  status string
- **THEN** listing still succeeds, presenting the raw value as unknown rather
  than failing the read
