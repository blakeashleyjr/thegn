# Autopilot

## ADDED Requirements

### Requirement: Marked issues are picked up autonomously, under explicit consent

When autopilot is enabled, thegn SHALL claim tracker issues that carry the
configured trigger label, satisfy the assignee policy (default: assigned to
the session's own tracker identity), and are in a configured pickup status —
and only such issues; there MUST be no heuristic pickup. Claims MUST be
evaluated off the event loop when the existing issue-tracker refresh
completes, adding no new wake source and nothing to the idle path, and MUST
be bounded by a configured concurrency cap. A claim MUST be durably recorded
before any work starts, so a re-poll or restart never dispatches the same
issue twice; after a crash mid-run the recorded run MUST resurface as needing
a human rather than silently re-dispatching. The feature MUST default to
disabled and, while disabled, MUST have no polling hook, no surfaces, and no
writes.

#### Scenario: A labeled, assigned issue is claimed once

- **WHEN** an issue gains the trigger label and is assigned per the assignee
  policy, and two consecutive issue refreshes complete
- **THEN** exactly one run is claimed for it, recorded durably before the
  worktree is created

#### Scenario: An unlabeled issue is never touched

- **WHEN** an issue is assigned to the user but lacks the trigger label
- **THEN** no run is claimed for it

#### Scenario: The concurrency cap holds

- **WHEN** more matching issues exist than the configured cap allows
- **THEN** only up to the cap run concurrently and the remainder wait for a
  slot, oldest first

#### Scenario: Disabled means inert

- **WHEN** `[autopilot] enabled` is false
- **THEN** issue refreshes trigger no pickup evaluation, the CLI verbs refuse
  with guidance, and no badges or notifications appear

### Requirement: A claimed issue becomes a worktree and a headless agent run

For each claimed issue, thegn SHALL create a worktree and branch derived from
the issue (linked through the existing issue↔worktree binding) and dispatch a
headless agent through the shared agent-task engine with an issue-implement
task kind carrying the issue's identifier, title, body, and URL, plus the
branch, base, and worktree. The prompt for this stage MUST carry the
merge-queue family's rules: work only in the worktree, commit on the branch,
do not push, never merge — the agent is given no forge credential and
performs no network write. The run MUST be subject to the configured watchdog
timeout and attempt budget, and an agent failure, timeout, or dirty result
MUST mark the run as needing a human with a notification, leaving the
worktree in place, and MUST NOT write to the tracker. On claim, thegn SHALL
set the issue's tracker status to in-progress (and MAY post a linking comment
when configured), and a tracker write failure MUST be surfaced as a run note,
never fatal to the run.

#### Scenario: Claim to running agent

- **WHEN** an issue is claimed
- **THEN** a worktree is created on an issue-derived branch, the issue is
  linked to it, its tracker status is set to in-progress, and a headless
  agent runs in the worktree with the issue's context in its prompt

#### Scenario: The implement-stage agent never pushes

- **WHEN** the issue-implement task's prompt is rendered
- **THEN** it instructs the agent to commit on the branch and not to push or
  merge, and the run performs no forge write until thegn validates the result

#### Scenario: A failed run asks for a human and leaves the tracker alone

- **WHEN** the agent times out or exits with the worktree dirty or without
  commits
- **THEN** the run is marked as needing a human with the reason, a
  notification fires, the worktree is preserved, and no tracker status is
  written

### Requirement: thegn opens the pull request and hands it to the PR queue

When an implement run succeeds — the worktree is clean and the branch is
ahead of its base — thegn itself SHALL push the branch (a plain push to a new
remote branch, never any force variant) and create the pull request through
the forge seam, deriving title and body from the issue with a
provider-appropriate reference, as ready-for-review or draft per
configuration. thegn SHALL then enqueue the pull request into the PR queue,
whose existing rules govern everything after — fixing CI, resolving
conflicts, addressing review feedback, and merging under the forge's own
protection. When the PR queue is disabled, the run SHALL stop at
pull-request-opened and report that shepherding is off rather than
reimplementing it. Autopilot MUST NOT merge, approve, or mark ready any pull
request itself, and MUST NOT widen any PR-queue safety default.

#### Scenario: Success becomes a queued pull request

- **WHEN** an implement run completes with commits on a clean worktree
- **THEN** thegn pushes the branch, opens a pull request referencing the
  issue, enqueues it into the PR queue, and records the run as shepherding

#### Scenario: CI feedback on the opened PR is handled by the queue

- **WHEN** the opened pull request's checks go red
- **THEN** the PR queue's existing classification and dispatch handle it,
  under the PR family's rules (push with force-with-lease, never merge)

#### Scenario: Queue disabled stops the loop honestly

- **WHEN** `[pr_queue] enabled` is false and an implement run succeeds
- **THEN** the pull request is opened, the run reports that shepherding is
  disabled, and nothing polls it

### Requirement: The tracker reflects the run's outcome

When the PR queue observes that a pull request autopilot opened has merged,
thegn SHALL set the linked issue's tracker status to done, when configured
(default on, autopilot-started runs only). Tracker writes from autopilot MUST
be limited to the in-progress and done status transitions and the optional
pickup comment — never closing, re-scoping, or otherwise editing items — and
a run needing a human MUST write no status.

#### Scenario: Merge closes the loop

- **WHEN** a shepherded pull request merges and `done_on_merge` is on
- **THEN** the linked issue's status is set to done and the run is recorded
  as done

#### Scenario: A stuck run changes nothing in the tracker

- **WHEN** a run is marked as needing a human at any stage
- **THEN** the issue's tracker status is left as it was

### Requirement: Autopilot is operable and observable from the CLI and chrome

thegn SHALL expose an `autopilot` CLI namespace — `status` (all runs, honoring
`--json`), `stop <issue>` (halt a run, preserving its worktree), and
`retry <issue>` (re-dispatch a stopped or needs-human run, consuming an
attempt) — each projected as a capability-catalog row gated by its required
scope, never a second policy table. Run-state transitions SHALL surface as
notifications (picked up, PR opened, needs human, done) and as a badge on the
linked issue's panel rows. Every surface MUST be inert while the feature is
disabled.

#### Scenario: Status reports the fleet of runs

- **WHEN** a user runs `thegn autopilot status --json`
- **THEN** every recorded run is reported with its issue, state, worktree,
  branch, attempt count, and pull request number when one exists

#### Scenario: Stop halts, retry resumes deliberately

- **WHEN** a user stops a working run and later retries it
- **THEN** the stop halts the agent and preserves the worktree, and the retry
  re-dispatches only from the stopped or needs-human state, consuming an
  attempt
