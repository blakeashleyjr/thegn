# pr-queue Specification

## Purpose

TBD - created by archiving change add-watched-pr-comment-tasks. Update Purpose after archive.

## Requirements

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

### Requirement: Pull requests can be assigned to a per-repo PR queue

thegn SHALL let a user assign pull requests to a per-repo queue, both by naming a
worktree (resolving the PR opened from its branch) and by PR number for a pull
request with no local checkout, and SHALL let them list, remove, and clear
entries. An entry MUST be recorded against the repository and PR number rather
than a worktree path, so a queued PR without a checkout is representable, and the
queue MUST survive across invocations.

#### Scenario: Queueing the current worktree's pull request

- **WHEN** a user runs `pr queue add` inside a worktree whose branch has an open
  pull request
- **THEN** that pull request is recorded in the repo's queue with a `watching`
  status and the worktree it was queued from

#### Scenario: Queueing a pull request with no local checkout

- **WHEN** a user runs `pr queue add --pr <number>`
- **THEN** the pull request is queued with no worktree recorded

#### Scenario: Removing and clearing entries

- **WHEN** a user runs `pr queue rm <number>` or `pr queue clear`
- **THEN** the named entry (or every entry for the repo) is removed

### Requirement: Queued pull requests are polled and classified off the event loop

thegn SHALL refresh each queued pull request's remote state on a bounded
interval and whenever a push is observed, and SHALL classify what is blocking it
— draft, failing checks, conflict with the base, requested changes, or awaiting
review. Refresh MUST run off the event loop and pulse the terminal waker,
preserving the idle-CPU invariant, and MUST be skipped entirely while the feature
is disabled or the session is offline. A fetch failure MUST back off and leave
the last known classification intact rather than recording a false blocker.

#### Scenario: A red check run blocks the pull request

- **WHEN** a queued pull request's check rollup contains a failure
- **THEN** the entry is classified as blocked on CI with the failing check named

#### Scenario: A pull request behind its base is a conflict

- **WHEN** a queued pull request reports a dirty or behind merge state
- **THEN** the entry is classified as blocked on a conflict with the base

#### Scenario: A fetch failure does not fabricate a blocker

- **WHEN** the forge cannot be reached while refreshing a queued pull request
- **THEN** the entry keeps its previous status, the failure is recorded as a note,
  and the next attempt is backed off

### Requirement: Blockers can be handed to a configurable headless agent

When an agent is configured and the blocker's kind is one the user has enabled,
thegn SHALL dispatch a headless agent in that pull request's worktree with a
prompt describing the blocker, then re-evaluate. It SHALL retry up to the
configured attempt budget and mark the entry as needing a human beyond it. The
agent MUST NOT merge the pull request — thegn or the forge performs the merge —
and thegn MUST NOT dispatch an agent for a pull request the user did not author
while `own_prs_only` is set, nor for an entry with no worktree to work in.

#### Scenario: The agent fixes a red build and the pull request goes green

- **WHEN** a queued pull request is blocked on CI, an agent is configured, and the
  agent pushes a fix
- **THEN** the next refresh sees green checks and the entry leaves the blocked state

#### Scenario: An entry with no worktree cannot be fixed by an agent

- **WHEN** a blocked entry has no local worktree recorded
- **THEN** it is marked as needing a human, naming the missing checkout as the
  reason, and no agent is dispatched

#### Scenario: Someone else's pull request is watched but never written to

- **WHEN** a queued pull request was authored by another user and `own_prs_only`
  is enabled
- **THEN** its state is tracked and displayed but no agent is dispatched

#### Scenario: The attempt budget refills when the branch moves

- **WHEN** a new commit that thegn did not create appears on a queued pull
  request's head
- **THEN** its agent attempt budget is reset so a long-lived pull request is not
  permanently stuck

### Requirement: A green pull request is merged under the forge's own rules

thegn SHALL treat a queued pull request as ready only when it is not a draft, has
no failing checks, and — when approval is required — carries an approving review
decision. By default thegn SHALL delegate the merge to the forge's auto-merge so
branch protection, required reviews, and any server-side merge queue remain
authoritative; it SHALL merge directly only when explicitly configured to, and
SHALL never merge when configured to stop at ready.

#### Scenario: A ready pull request is handed to the forge's auto-merge

- **WHEN** a queued pull request becomes ready and `merge_mode` is `auto_merge`
- **THEN** thegn enables auto-merge on it and records the entry as ready, leaving
  the merge itself to the forge

#### Scenario: Direct merge only on request

- **WHEN** the same pull request becomes ready and `merge_mode` is `thegn`
- **THEN** thegn merges it with the configured method

#### Scenario: A draft is never merged

- **WHEN** a queued pull request is a draft, even with green checks and an approval
- **THEN** it is not merged and remains blocked as a draft

#### Scenario: Missing approval holds a pull request

- **WHEN** `require_approval` is set and a queued pull request has green checks but
  no approving review
- **THEN** it is not merged and is reported as awaiting review

### Requirement: The PR queue is driven from the CLI and visible in the UI

thegn SHALL expose a `pr queue` command namespace (`add`, `list`, `rm`, `clear`,
`status`, `drain`) honoring the `--json` output convention, and SHALL surface the
queue as a panel section with per-row actions, a statusbar badge, and
notifications for settled transitions. Every surface MUST be inert while the
feature is disabled.

#### Scenario: Draining from the CLI reports each outcome

- **WHEN** a user runs `pr queue drain`
- **THEN** each pull request's outcome is reported and `--json` emits a
  machine-readable summary

#### Scenario: Disabled leaves no surfaces behind

- **WHEN** `[pr_queue] enabled` is false
- **THEN** the command refuses with guidance, no polling occurs, and the panel
  section, badge, and palette actions are hidden
