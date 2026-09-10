# Agent orchestration

## ADDED Requirements

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
