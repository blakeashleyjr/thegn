# Agent

## ADDED Requirements

### Requirement: Operating agents are declarative and tool-restricted

thegn SHALL support named operating-agent definitions declared as markdown
files with front-matter (id, title, allowed tool set, optional model, system
prompt), resolved from a repo `.thegn/agents/` directory layered over a global
agents directory (repo winning by id). The declared tool set MUST be the upper
bound on what that agent may call; a tool call outside the set MUST be refused.

#### Scenario: Researcher cannot mutate

- **WHEN** the active operating agent declares a tool set of read/search/fetch and
  it attempts a file-write tool call
- **THEN** the write is refused because it is outside the agent's bounded tool set

#### Scenario: Repo definition overrides global

- **WHEN** both the global and the repo agents directory define an agent with the
  same id
- **THEN** the repo definition is used

### Requirement: Built-in operating agents ship by default

thegn SHALL ship three built-in operating agents — an executor
(read/write/patch/shell), a read-only researcher (read/search/fetch), and a
planner (read/search plus plan-writing) — and a user file with the same id MUST
override the built-in.

#### Scenario: Defaults are available with no config

- **WHEN** no user agent files exist
- **THEN** the executor, researcher, and planner agents are available with their
  specified tool sets

### Requirement: Skills are reusable workflows orthogonal to agents

thegn SHALL support named skill definitions (a `SKILL.md` with name,
description, and optional parameters) resolved from a repo `.thegn/skills/`
directory and a global directory, and a skill MUST be invocable by any operating
agent rather than being bound to one. Skills are surfaced to harnesses through the
gateway's capability injection so a single definition is available across
harnesses.

#### Scenario: Skill invocable across agents

- **WHEN** a skill is defined and two different operating agents are active in
  turn
- **THEN** both agents can invoke the skill

#### Scenario: Invalid skill is rejected

- **WHEN** a `SKILL.md` is missing its name or description
- **THEN** it fails validation and is not offered
