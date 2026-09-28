# agent-orchestration

## ADDED Requirements

### Requirement: A validation run is classified by what it established, not by its exit code

thegn SHALL classify the result of running a configured validation command
against a pipeline lane into a closed set that separates a verdict about the
**code** (it did not build, a test failed, a linter objected) from a fact about
the **environment** (the command could not run, was not found, or was killed by
a signal) and from an **unrecognised** outcome. An environment failure MUST NOT
be reported as a verdict about the branch, and an unrecognised failure MUST NOT
be coerced into a recognised one. Classification MUST be usable for any
toolchain, keyed by the validation task's configured output matcher with a
generic fallback, and MUST NOT name any coding-agent harness. The stored
evidence for a run MUST be bounded in both line count and line length.

#### Scenario: A test failure is not reported as a build break

- **WHEN** a test runner exits non-zero having printed both its own failure
  lines and a trailing generic error line
- **THEN** the run is classified as a test failure, so the lane is returned for
  the behaviour that actually failed rather than for a compile error that did
  not occur

#### Scenario: A build break outranks the noise it causes

- **WHEN** a run prints a compiler error and, because of it, a test runner also
  reports that the run failed
- **THEN** the run is classified as a build break, because no later signal in
  that output says anything until the code compiles

#### Scenario: A command that never ran blames nothing

- **WHEN** a validation command cannot be launched, is not executable, is not
  found, or is killed by a signal
- **THEN** the result is classified as an environment fact, is not treated as a
  passing result either, and is eligible to be retried before anybody is told

#### Scenario: An unfamiliar failure is reported as unknown

- **WHEN** a validation command exits non-zero and its output matches no known
  shape
- **THEN** the result is recorded as inconclusive rather than being attributed
  to the build, the tests or the linter

### Requirement: An approval is bound to the commit it was granted against

thegn SHALL record an approval of a pipeline stage's output against a specific
commit, and SHALL report that approval as no longer applying once the lane's tip
differs from it. An approval MUST additionally be invalidatable by explicit
revocation and by age, where an explicitly recorded deadline and a configured
maximum age both apply and the earlier of them wins. A recorded commit too short
to identify a tree MUST be refused rather than matched permissively. Abbreviated
and full commit identifiers MUST compare equal when one is a prefix of the
other.

#### Scenario: A new commit withdraws the approval

- **WHEN** a lane is approved and a further commit is then made on it
- **THEN** the approval reports as superseded, naming both the commit that was
  approved and the current tip, and authorizes nothing

#### Scenario: A refusal says what is actually on record

- **WHEN** an approval exists but does not apply to the current tip
- **THEN** the refusal names the approved commit rather than reporting that no
  approval exists, so the reviewer is not sent to redo work they already did

#### Scenario: An empty approved commit matches nothing

- **WHEN** an approval record carries an empty or too-short commit identifier
- **THEN** it is reported as malformed and authorizes nothing, rather than
  matching every tree by prefix

### Requirement: A stage transition is authorized by recorded facts the operator named in advance

thegn SHALL let each stage declare, from a closed vocabulary, what must already
be recorded about the output it consumes before that stage may be dispatched —
the parent's artifact being committed, the parent's report being filed, the
parent's validation having recorded a passing result, and an applying approval.
Landing SHALL carry its own such gate, evaluated against the terminal row, and
SHALL default to requiring an approval. thegn SHALL decide a transition only by
checking those recorded facts, SHALL carry the satisfied requirements on the
decision so that the reason is inspectable, and SHALL NOT offer any operation
that creates an approval as part of deciding or applying a transition. A
validation result that establishes nothing MUST NOT satisfy a passing-result
requirement.

#### Scenario: An approved lane advances and says what authorized it

- **WHEN** every requirement a stage declares is satisfied by a recorded fact
- **THEN** the transition is proposed together with the list of requirements
  that were checked and met

#### Scenario: A lane that is mechanically finished asks for a person

- **WHEN** a lane's validation has recorded a passing result and the next stage
  requires an approval that does not yet apply
- **THEN** the lane is reported as awaiting a decision, naming the exact commit
  to review, and is distinguished from lanes whose handoff is merely incomplete

#### Scenario: A command that never ran does not count as a pass

- **WHEN** a stage requires a passing validation result and the only recorded
  result is an environment failure
- **THEN** the transition is refused, because "not a failure" is not "passed"

### Requirement: The supervisor's decisions are inspectable before anything is switched on

thegn SHALL provide a read-only surface that reports, for every pipeline lane,
what the supervisor would do and which recorded facts would authorize it, and
that works regardless of whether the supervisor is enabled. Exactly one decision
MUST be reported per lane, including for lanes it would leave alone, together
with the reason — a lane omitted from the report is indistinguishable from a
lane that was never examined. A lane already merged into the target branch MUST
be reported as finished rather than examined for content, because a merged lane
presents no difference from the target and would otherwise read as an empty
change.

#### Scenario: A landed lane is not re-examined

- **WHEN** a lane's commits are already an ancestor of the target branch
- **THEN** it is reported as finished, and no validation, transition or finding
  is proposed for it

#### Scenario: Deciding twice over unchanged facts decides the same thing

- **WHEN** the supervisor's decision is computed twice over the same recorded
  facts
- **THEN** the same decisions result, and in particular a validation already
  recorded against the lane's current tip is not proposed again

#### Scenario: Work that may still be running is left alone

- **WHEN** a roster row carries no record of its worker having exited
- **THEN** the lane is left alone, because absence of an exit record means
  unknown rather than finished

### Requirement: A pipeline configuration that could never be satisfied is refused

thegn SHALL reject, at explicit config validation, a validation step that does
not name a configured task, a requirement outside the closed vocabulary, a
requirement on a stage that nothing advances into, and a passing-result
requirement whose parent stage declares no validation. Each refusal MUST name
the offending entry and the remedy. A validation step MUST name a configured
task rather than carrying a command, so that a stage cannot introduce arbitrary
command execution from configuration.

#### Scenario: An unmeetable requirement fails validation rather than stalling a lane

- **WHEN** a stage requires a passing validation result but the stage that feeds
  it declares nothing to validate
- **THEN** config validation fails, naming both stages, rather than the
  configuration being accepted and its lanes silently never moving

#### Scenario: A misspelled requirement is refused with the known values

- **WHEN** a requirement is spelled in a way the closed vocabulary does not
  contain
- **THEN** validation fails listing the known values, rather than accepting a
  gate that is present in the file and absent in effect

#### Scenario: A validation step cannot smuggle in a command

- **WHEN** a stage's validation step is written as a shell command line rather
  than the name of a configured task
- **THEN** validation fails, naming the task that would have to be defined
