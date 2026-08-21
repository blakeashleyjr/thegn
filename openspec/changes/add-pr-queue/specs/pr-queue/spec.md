# PR Queue

## ADDED Requirements

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
