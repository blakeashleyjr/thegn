# config

## ADDED Requirements

### Requirement: The pipeline stage chart is declarative, validated configuration

Configuration SHALL carry an optional multi-stage agent pipeline as an ordered
list of stages, each declaring a name, the agent that runs it, a prompt
template, a concurrency budget, an advisory timeout, an optional next stage, and
what to do when a stage worker blocks. The table SHALL default to empty, and an
absent table MUST behave exactly as an empty one.

The chart is **structure, not judgment**: thegn SHALL validate it and MAY
display it, and **no thegn code path may advance the next stage, enforce the
concurrency budget, or fire the timeout** — those fields are read by a
supervising agent, which resolves the whole table as one machine-readable
document. Removing the table MUST NOT change any behaviour other than what is
validated and displayed.

#### Scenario: An absent chart is inert

- **WHEN** a config file declares no pipeline stages
- **THEN** it validates clean, emits no warning, and every surface behaves as
  before

#### Scenario: The chart resolves as one document

- **WHEN** `thegn config get pipeline --json` runs against a configured chart
- **THEN** it emits the stage list as structured JSON — each stage's name,
  agent, prompt, concurrency, timeout, next and blocked-handling — including the
  defaults for keys the file omitted

### Requirement: A stage chart is strictly validated

Strict validation SHALL reject a chart that cannot be executed, reporting each
problem against the offending stage's index and name so the message points at a
line in the file. A stage MUST have a non-empty, unique name; MUST name an agent
that resolves either to a configured agent/tool entry or to a known coding-agent
harness id, on the same terms the agent-launch path accepts; MUST declare a
concurrency of at least one; and MUST NOT declare a next stage that names no
configured stage or that closes a cycle. Each cycle SHALL be reported once
rather than once per member.

A stage that no other stage's next edge reaches, and that is not the first
stage, SHALL raise a soft warning on the config-warning channel rather than an
error — it is reachable by explicit dispatch.

#### Scenario: An agent that names nothing launchable

- **WHEN** a stage's agent is a shell command line rather than a configured
  agent/tool name or a known harness id
- **THEN** validation fails naming that stage's index, name and the offending
  value

#### Scenario: A concurrency budget of zero

- **WHEN** a stage declares a concurrency of `0`
- **THEN** validation fails, because a stage that can never run is a typo rather
  than a way to disable one

#### Scenario: A cycle in the next edges

- **WHEN** three stages form a loop through their next edges
- **THEN** validation reports exactly one cycle error, naming the path, from the
  earliest-declared member

#### Scenario: An unreachable stage

- **WHEN** a stage is neither the first stage nor named by any other stage's next
  edge
- **THEN** the config load warns about it and nothing is blocked

### Requirement: Stage prompt templates are checked against a fixed variable set

Every stage's prompt template SHALL be checked at validation time against the
variables a stage worker's prompt may reference: everything an issue worker's
prompt may reference, plus the stage's own name, the artifact it writes, and the
artifact its parent stage wrote. An unknown placeholder MUST be a validation
error rather than an empty expansion at dispatch time.

thegn SHALL NOT render a stage prompt itself — the variable set exists to
validate the template, and substitution is the supervising agent's job.

#### Scenario: A typo in a stage prompt

- **WHEN** a stage's prompt references a placeholder that is not in the stage
  variable set
- **THEN** validation fails naming that stage's prompt key and the unknown
  placeholder

#### Scenario: Issue vocabulary stays valid for a stage

- **WHEN** a stage's prompt references the variables an issue worker's prompt
  uses
- **THEN** validation accepts them, because the stage variable set is a superset
