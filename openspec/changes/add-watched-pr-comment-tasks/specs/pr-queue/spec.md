# PR Queue

## ADDED Requirements

### Requirement: Unresolved review comments are a classified, optionally actionable blocker

thegn SHALL classify a queued pull request that has unresolved review threads
and no stronger blocker as blocked on unresolved comments, reporting the
count, and SHALL always display that classification on the entry. Dispatching
an agent for it MUST additionally require the review watch class to be
enabled and the review trigger to be configured to act on any unresolved
feedback; the default trigger MUST remain the forge's changes-requested
review decision, so existing configurations gain no new autonomous behavior.
The dispatched task MUST be the existing review task, whose prompt lists the
unresolved threads, and every existing team-safety rule (own-PRs-only,
foreign-push pause, force-with-lease, never merge/approve/resolve, attempt
budget) MUST apply unchanged. A thread fetch failure MUST leave the entry's
previous comment state intact rather than fabricating a change.

#### Scenario: Comments without a formal review are seen

- **WHEN** a queued pull request has two unresolved review threads and no
  changes-requested review decision
- **THEN** the entry reports itself blocked on unresolved comments with the
  count, ranked below a red check or conflict when those are also present

#### Scenario: Default trigger keeps today's behavior

- **WHEN** the review trigger is at its default and a queued pull request has
  unresolved threads but no changes-requested decision
- **THEN** the blocker is displayed but no agent is dispatched

#### Scenario: Opting up makes comments actionable

- **WHEN** the review trigger is set to act on any unresolved feedback, the
  review watch class is enabled, an agent resolves, and a queued own pull
  request has unresolved threads
- **THEN** the review task is dispatched in that pull request's worktree with
  the unresolved threads in its prompt

### Requirement: New feedback re-arms and notifies a watched entry

thegn SHALL record a fingerprint of each queued pull request's unresolved
review thread identities. When the fingerprint changes by gaining a thread
identity thegn has not seen for that entry, thegn SHALL refill the entry's
agent attempt budget — so a long-lived pull request recovers when a human
adds feedback — and SHALL raise a notification for the new feedback,
including on entries the agent will never write to. The agent's own replies
MUST NOT count as new feedback, since replying does not change the unresolved
set, so an agent cannot refill its own budget.

#### Scenario: A stuck entry recovers on a new comment

- **WHEN** an entry marked needing a human after exhausting its attempt
  budget gains a new unresolved review thread
- **THEN** its attempt budget is refilled and the entry is re-evaluated on the
  next pass

#### Scenario: Watching a teammate's pull request stays useful

- **WHEN** a queued pull request authored by someone else gains a new
  unresolved thread while `own_prs_only` is enabled
- **THEN** a notification reports the new feedback and no agent is dispatched

#### Scenario: The agent's replies are not new feedback

- **WHEN** the only change to an entry's threads since the last pass is
  replies posted by the dispatched agent
- **THEN** the fingerprint is unchanged, no budget refill occurs, and no
  notification is raised

### Requirement: The dispatched agent can be chosen per queue entry

thegn SHALL let a user attach an agent override to an individual queue entry —
naming a configured agents/tools entry or supplying a full command template —
when adding the entry from the CLI and from the queue's panel section, and
SHALL let them clear it. At dispatch, the entry's override MUST take
precedence over the repo-level queue agent configuration, and MUST pass the
same template validation; an entry whose override fails to resolve MUST be
marked as needing a human with that reason rather than silently falling back.
The override MUST survive restarts and MUST be reported by the queue's list
output.

#### Scenario: A heavyweight PR gets a heavyweight agent

- **WHEN** a user runs `pr queue add --agent <name>` naming a configured
  agents entry, and the entry later blocks on CI
- **THEN** the dispatched task runs that agent rather than the repo-level
  `[pr_queue]` agent

#### Scenario: A broken override is surfaced, not papered over

- **WHEN** an entry's agent override no longer resolves against configuration
  at dispatch time
- **THEN** the entry is marked as needing a human naming the unresolvable
  override, and no fallback agent runs

#### Scenario: The override is visible and durable

- **WHEN** a user lists the queue with `--json` after a restart
- **THEN** entries report their agent override, or its absence
