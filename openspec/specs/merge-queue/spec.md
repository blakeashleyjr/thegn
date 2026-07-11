# Merge Queue

## Purpose

The merge queue lets a user assign worktree branches to a per-repo queue that
drains serially, folding each branch onto the target tip in the object database,
test-gating it, and atomically CAS-advancing the target ref (never a working-tree
merge) for clean branches. Conflicts and gate failures can be handed to a
headless CLI agent that fixes the branch in its own worktree, while thegn
always performs the land itself so object-DB coherence and the merge guard hold.
The whole queue is drivable from a `merge` CLI namespace.

## Requirements

### Requirement: Worktree branches can be assigned to the merge queue

thegn SHALL let a user assign worktree branches to a per-repo merge queue,
both explicitly (one or more named worktrees) and in bulk (every eligible
worktree branch), and SHALL let them list, remove, and clear queue entries. An
assigned branch MUST be recorded with a `queued` status keyed by its worktree
path, so the queue survives across invocations and is visible in the panel.

#### Scenario: Explicitly assigning a worktree queues its branch

- **WHEN** a user runs `merge add <worktree>`
- **THEN** that worktree's current branch is recorded in the queue as `queued`
  against the repo's target branch

#### Scenario: Assigning all eligible branches

- **WHEN** a user runs `merge add --all` in a repo
- **THEN** every eligible worktree branch (excluding the target branch and, absent
  `snapshot_dirty`, dirty worktrees) is queued

#### Scenario: Removing and clearing entries

- **WHEN** a user runs `merge rm <worktree>` or `merge clear`
- **THEN** the named entry (or every entry for the repo) is removed from the queue

### Requirement: The queue drains branches one at a time and auto-lands the clean ones

thegn SHALL drain the queue serially, one branch at a time, oldest-queued
first. For each branch it SHALL fold the branch onto the repo's current target
tip in the object database and test-gate the result; a branch that merges clean
and passes the gate SHALL be landed by an atomic compare-and-swap of the target
ref when `auto_land` is on, or held at a `ready` status when it is off. The
target ref MUST only advance through the object-DB fold + CAS (never a
working-tree merge), and the main checkout SHALL be fast-forwarded without
clobbering uncommitted work.

#### Scenario: A clean branch lands automatically

- **WHEN** the driver drains a queued branch that folds clean and gates green
  with `auto_land = true`
- **THEN** the target ref is CAS-advanced to include the branch and the row is
  marked `landed`

#### Scenario: auto_land off holds at ready

- **WHEN** the same branch is drained with `auto_land = false`
- **THEN** the target ref is not advanced and the row is marked `ready` for a
  later explicit land

#### Scenario: An already-merged branch is a no-op

- **WHEN** a queued branch's tip is already an ancestor of the target
- **THEN** the driver records it as landed without creating a redundant merge

### Requirement: Conflicts and gate failures are handed to a headless agent

When `conflict_handoff` is `"agent"` and `agent_command` is set, the driver SHALL
dispatch a headless CLI agent to fix a branch that has a textual merge conflict
or fails the test gate, running the agent in that branch's own worktree with a
task prompt describing the conflict paths or the gate output. After the agent
finishes, the driver SHALL re-attempt the fold; it SHALL retry up to
`agent_max_attempts` and mark the branch `needs_human` if it still cannot land.
The agent MUST NOT be relied on to merge into the target — thegn performs the
land itself, so the object-DB coherence guarantee and the merge guard hold. Each
agent invocation SHALL be bounded by `agent_timeout_secs`.

#### Scenario: The agent resolves a conflict and the branch lands

- **WHEN** a queued branch conflicts with the target and the agent resolves it in
  the worktree
- **THEN** the driver's re-attempt folds the branch clean and lands it

#### Scenario: The agent cannot fix it within the attempt budget

- **WHEN** the agent fails to make the branch landable within `agent_max_attempts`
- **THEN** the branch is marked `needs_human` and the target is left unchanged

#### Scenario: Agent handoff disabled defers instead

- **WHEN** `conflict_handoff` is not `"agent"` or `agent_command` is empty and a
  branch conflicts or fails the gate
- **THEN** the branch is left `deferred` / `gate_failed` with its reason recorded,
  and no agent is run

### Requirement: The merge queue is driven from the CLI

thegn SHALL expose a `merge` command namespace (`add`, `list`, `rm`, `clear`,
`drain`, `land`) that assigns and drains the queue programmatically, honoring the
`--json` output convention. The batch fold-everything path SHALL remain available
as the `integrate` command.

#### Scenario: Draining from the CLI reports outcomes

- **WHEN** a user runs `merge drain`
- **THEN** each branch's outcome (landed / ready / deferred / needs a human) is
  reported, and `--json` emits a machine-readable summary

#### Scenario: Landing a ready branch

- **WHEN** a user runs `merge land <worktree>` for a branch held at `ready`
- **THEN** the branch is folded and CAS-advanced into the target
