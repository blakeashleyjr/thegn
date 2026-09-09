# PR Queue

## ADDED Requirements

### Requirement: Explicit review watches derive durable per-thread tasks

On the existing off-loop PR-queue refresh cadence, thegn SHALL derive review
tasks only for a durable, explicitly queued pull-request row whose resolved
`watch` contains `review`. It SHALL consume THE-27's last complete
`PrReviewSnapshot` model and SHALL perform source-key, revision, prompt, and
transition derivation in substrate-free core code. The feature MUST NOT add an
all-PR watcher, automatic watch default, second timer, `review_trigger`,
PR-wide unresolved-comments blocker, or per-PR thread fingerprint. A transient
conversation or diff fetch failure MUST preserve the previous cache and roster
rather than treating missing data as resolution.

#### Scenario: An explicitly watched row produces thread tasks

- **WHEN** an explicitly queued pull request has `review` in its resolved watch
  list and a successful complete snapshot contains two unresolved provider
  threads
- **THEN** thegn reconciles two durable review tasks on the existing queue
  worker and schedules no additional timer

#### Scenario: Review data is not fetched for an unwatched row

- **WHEN** a queued pull request's resolved watch list does not contain
  `review`
- **THEN** thegn does not deep-fetch review data or derive review tasks for that
  row

#### Scenario: A transient fetch cannot erase work

- **WHEN** a watched row already has a durable review task and the next review
  fetch fails
- **THEN** the prior snapshot and task remain unchanged and unresolved

### Requirement: Thread identity deduplicates and revisions update in place

thegn SHALL create at most one active roster task for each canonical
forge/repository/pull-request/provider-thread identity. It SHALL compute a
bounded deterministic revision from that thread's current snapshot, SHALL make
an unchanged revision a no-op, and SHALL revise the same durable row when its
anchor or comments change. A new revision SHALL requeue terminal or
human-parked work while retaining the admission state of a running task. A
provider thread observed resolved SHALL transition its existing task to done.

Each create/revision SHALL capture the current configured `[pr_queue] agent`
role and resolved review prompt, using `TaskKind::PrReview` validation and
THE-27's bounded thread formatter. Only after the upsert is durable, thegn SHALL
emit a bounded event named `pr.thread_unresolved` containing the source key,
source revision, PR/thread identity, anchor, head, role, prompt, and worktree.

#### Scenario: A reply revises one task rather than duplicating it

- **WHEN** another comment is added to an unresolved provider thread that
  already has a roster task
- **THEN** its source revision and bounded prompt are updated on the same task
  id, the task becomes queued if it was parked, and no second source row is
  created

#### Scenario: An unchanged snapshot is idempotent

- **WHEN** two successful refreshes contain the same thread snapshot
- **THEN** the second reconciliation writes no revision and emits no duplicate
  `pr.thread_unresolved` event

#### Scenario: Current role and prompt are captured

- **WHEN** a new or revised thread is reconciled after the repository's queue
  role or review prompt changes
- **THEN** that task and event carry the currently resolved role and bounded
  rendered prompt

### Requirement: Handling a review task is explicit and push-verified

thegn SHALL expose the same TUI-only `pr-review-task-handle` behavior through
panel key `h` on a queued thread row and the command palette. Refresh SHALL only
create or revise durable tasks and MUST NOT automatically launch their agents.
Handling SHALL run off the event loop using the saved prompt, exact configured
role/command with no default-role fallback, and the existing PR-queue sandbox
floor and timeout.

Agent exit MUST be advisory. Before resolving a thread, thegn SHALL establish
all of the following: the provider head matched the task baseline before launch;
the task revision remained unchanged; the provider head moved from that
baseline; the moved provider head exactly equals the task worktree's local
HEAD; and a fresh provider conversation still reports the same thread
unresolved. A failed condition SHALL leave the provider thread unresolved and
park the task for a human.

#### Scenario: A verified push can advance to provider resolution

- **WHEN** a user handles a queued task, its agent pushes a new head that
  exactly matches the task worktree, the task revision is unchanged, and the
  provider still reports the thread unresolved
- **THEN** thegn may invoke the provider's review-thread reply/resolve operation
  and records the task done only after resolution succeeds

#### Scenario: Agent exit without a verified push is not completion

- **WHEN** the selected agent exits but the provider head did not move or does
  not match the task worktree's local HEAD
- **THEN** thegn leaves the review thread unresolved and parks the task for a
  human

#### Scenario: A concurrent revision is requeued

- **WHEN** polling revises the same task while its agent is running
- **THEN** the old invocation cannot resolve the newer feedback and the latest
  revision is queued for another explicit handle

### Requirement: Review-thread resolution is capability-gated and audited

The forge seam SHALL advertise an optional object-safe
`resolve_review_thread` operation that posts a bounded audit reply and resolves
the identified thread as one semantic provider action. Providers that do not
implement it SHALL report unsupported. Unsupported, unauthenticated,
not-configured, stale, offline, or rate-limited outcomes MUST NOT be represented
as resolved; the task SHALL wait for a human, with durable retry cooldown where
the failure is transient.

thegn SHALL record once-keyed notification audit for task creation/revision,
successful resolution, and needs-human outcomes. This feature SHALL NOT add a
CLI verb or completion slot, control schema/route, capability-catalog entry,
gRPC/MCP operation, or plugin call.

#### Scenario: Unsupported resolution remains human work

- **WHEN** a verified agent push reaches a forge provider that does not support
  `resolve_review_thread`
- **THEN** the provider thread remains unresolved, the task is parked for a
  human, and the failure is recorded without an automatic retry loop

#### Scenario: Rate limiting preserves remote truth

- **WHEN** the provider rate-limits the post-agent thread recheck or resolution
- **THEN** the thread remains unresolved, the task records a durable cooldown
  and needs-human audit, and polling does not claim completion

#### Scenario: Successful resolution is auditable

- **WHEN** the provider confirms reply/resolution after every verified-push
  condition passes
- **THEN** the roster task becomes done and a once-keyed resolved notification
  records the thread, source revision, and verified head
