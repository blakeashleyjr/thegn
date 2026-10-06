# agent-orchestration Specification

## Purpose

TBD - created by archiving change harden-pipeline-slot-accounting. Update Purpose after archive.

## Requirements

### Requirement: A roster row distinguishes a live worker from an exited one

thegn SHALL record a dispatch row's worker exit — the exit code when it was
reaped, and when the exit was observed — and SHALL derive from it a liveness
that separates a row whose worker is still running from a row whose worker has
finished but which no supervisor has closed. Absence of an exit stamp MUST mean
_unknown_, never _exited_: a row from before the column existed, or one whose
daemon died before it could stamp, MUST NOT be reported as finished. A row
whose worker has exited MUST still count as occupying its stage slot until a
supervisor closes it — exiting is not the same as being reconciled.

#### Scenario: An exited-but-unclosed row is not free capacity

- **WHEN** a worker exits and its row keeps a non-terminal status because
  closing it is the supervisor's verified decision
- **THEN** the row reports as exited-unverified rather than as a live worker,
  it still occupies its stage slot, and the operator-facing listing says so
  rather than showing a bare `running`

#### Scenario: A row with no exit stamp is treated as live

- **WHEN** a roster row predates the exit columns, or its daemon went away
  before stamping
- **THEN** the row reports as live, so nothing closes work that may still be
  running

### Requirement: Creating a dispatch is one atomic, checkable step

thegn SHALL offer a claim operation that decides whether a dispatch may be
created and creates it inside a single transaction, so that the decision and
the insert cannot be interleaved with another claimant. The claim MUST refuse
when an equivalent row already occupies a slot, and MUST refuse when the
stage's configured concurrency is already fully occupied. Occupancy MUST be
counted from roster rows — including exited-but-unclosed ones — never from live
process or session liveness alone.

Equivalence MUST be keyed on the issue, the stage, the worktree AND the handoff
artifact, so that several workers of one stage running in one worktree on
different artifacts are recognised as distinct work rather than duplicates.
When either claim lacks an assigned artifact, the chunk path MAY distinguish
provisional work and MUST match a retry to the same chunk even if the existing
row has since been assigned its row-derived artifact. When both claims carry an
artifact, it MUST dominate the chunk path so different chunk descriptions
cannot concurrently own the same handoff.

A deliberate duplicate MUST remain possible through an explicit override that
requires a reason, and that reason MUST be recorded on the created row in the
same transaction, so an authorized duplicate is always distinguishable from a
runaway one. This override MUST NOT bypass the configured concurrency budget.
The public claim command MUST refuse a stage absent from the loaded pipeline
instead of treating it as unbounded.

Every pipeline helper that creates a new worker, including a finisher created
by `session open --resume-work`, MUST use the same atomic admission policy. A
spawning or running source with no exit stamp MUST NOT be resumed because its
worker may still be live. An eligible queued, parked, or positively exited
source SHALL be deliberately reconciled before its finisher competes for the
freed slot. Reconciliation and admission MUST commit together: a duplicate or
capacity refusal rolls the source back unchanged, and a concurrent supervisor
verdict MUST be preserved. The finisher's rendered prompt MUST bind its own
row id and row-derived artifact (with the source artifact only as parent/recovery
context), so the path the worker is told to commit is the path its roster row
publishes and the completion gate verifies.

#### Scenario: Parallel chunks are not duplicates

- **WHEN** a supervisor claims a third coder in a worktree that already has two
  open coder rows, each producing a different chunk artifact
- **THEN** the claim is granted, because identity includes the artifact

#### Scenario: Simultaneous claimants receive one authoritative answer

- **WHEN** two independent database connections claim the final stage slot at
  the same time, or claim the same artifact while more than one slot is free
- **THEN** exactly one row is created, and the other claimant is refused as at
  capacity or as a duplicate based on the winner committed in the transaction

#### Scenario: Duplicate override preserves the stage budget

- **WHEN** a supervisor supplies a non-empty duplicate override while the
  stage is already at capacity
- **THEN** the claim is refused at capacity and no row or audit note is created

#### Scenario: Different chunks cannot share an assigned artifact

- **WHEN** two otherwise equivalent claims name different chunk files but the
  same assigned handoff artifact
- **THEN** the second claim is refused as a duplicate

#### Scenario: A re-dispatch of finished-but-unclosed work is refused

- **WHEN** a supervisor claims work whose row is already open and whose worker
  has exited
- **THEN** the claim is refused, naming the row and directing the supervisor to
  verify and close it rather than dispatch again

#### Scenario: Resume cannot duplicate a possibly-live worker

- **WHEN** a supervisor asks to resume a spawning or running row whose worker
  has no recorded exit
- **THEN** the finisher is refused without creating a row or a session

#### Scenario: A finisher obeys the same stage ceiling

- **WHEN** a supervisor resumes eligible queued, parked, failed, or positively
  exited work
- **THEN** the prior row is terminal or deliberately reconciled, and the new
  finisher is inserted only if the atomic duplicate and capacity claim grants it

#### Scenario: A refused finisher does not consume its source

- **WHEN** an eligible parked source is resumed but another row owns the last
  stage slot, or the source receives a newer supervisor verdict concurrently
- **THEN** no finisher is inserted and the source retains its pre-resume or
  newer status rather than being left failed by a partial reconciliation

#### Scenario: A finisher writes the artifact recorded on its own row

- **WHEN** a new finisher row is admitted for a source attempt with an older
  artifact path
- **THEN** the rendered stage task and finisher instruction name the new row's
  exact id and row-derived artifact, while the older path appears only as
  source/parent recovery context

#### Scenario: A restarted monitor cannot refill an occupied stage

- **WHEN** a monitor dies without closing its rows and a new monitor starts
  with no memory of them
- **THEN** claims against that stage are refused while the rows remain open,
  and the refusal reports how many of the occupants have already exited

### Requirement: One monitor owns a pipeline at a time

thegn SHALL provide a durable, expiring lease that a supervising process takes
before driving a pipeline. A second process MUST be refused while the lease is
live and MUST be told who holds it. The holder MUST be able to renew its own
lease, and a lease whose holder has crashed MUST become available again without
human intervention once it expires. Releasing MUST be owner-scoped.

#### Scenario: A second Lead is refused

- **WHEN** a monitor holds the pipeline lease and another monitor starts
- **THEN** the second monitor is refused and told which owner holds the lease

#### Scenario: A crashed monitor's lease lapses

- **WHEN** the lease holder dies without releasing
- **THEN** the lease expires on its own and the next monitor may take it

### Requirement: A stage prompt must teach the handoff contract it is gated on

Because run-completion is gated on a worker-filed report, thegn SHALL reject a
configured stage whose prompt does not give the worker its roster row id and
does not instruct it to file a report. The check MUST run at explicit config
validation and MUST also be surfaced at config load, because the operator who
most needs it is the one whose pipeline is already running against prompts that
cannot close a row. Detection of the row placeholder MUST use the same template
parser the renderer uses, so an escaped brace pair does not count.

#### Scenario: A prompt that cannot close its row is rejected

- **WHEN** a stage prompt never references the row placeholder, or never names
  the report command, while the completion gate requires a report
- **THEN** validation reports each missing half separately, naming the remedy,
  even though the org chart is otherwise well formed

### Requirement: Reclaiming build output never fights a running build

thegn SHALL NOT reclaim a worktree's build output when that worktree still
carries an unclosed pipeline dispatch — such work is mid-flight even though no
process is running in it. thegn SHALL additionally apply hysteresis: a worktree
whose output was reclaimed recently MUST NOT be reclaimed again within a
cooldown window, and pressure-driven eviction MUST free past the warning
threshold rather than stopping exactly on it, so that reclaiming and rebuilding
cannot oscillate against each other on a disk that sits near the line.

#### Scenario: Work awaiting verification keeps its build output

- **WHEN** a worktree's worker has exited and committed, but its row is not yet
  closed, and the disk is under pressure
- **THEN** its build output is preserved, so the reviewing stage does not pay
  for a cold rebuild of work already done

#### Scenario: Reclaim does not oscillate with rebuild

- **WHEN** a worktree's output was reclaimed and a later build repopulates it
  while the disk remains near the critical line
- **THEN** the worktree is not reclaimed again until the cooldown has elapsed,
  and the earlier eviction freed enough headroom that the rule is not
  immediately re-triggered

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
