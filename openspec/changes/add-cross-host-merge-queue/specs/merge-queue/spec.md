# Merge Queue

## MODIFIED Requirements

### Requirement: Worktree branches can be assigned to the merge queue

thegn SHALL let a user assign worktree branches to a per-repo merge queue,
both explicitly (one or more named worktrees) and in bulk (every eligible
worktree branch), and SHALL let them list, remove, and clear queue entries. An
assigned branch MUST be recorded with a `queued` status keyed by its worktree
path, so the queue survives across invocations and is visible in the panel. Each
row MUST also record the worktree's location descriptor (the host it lives on),
so the queue can attribute a row to a host without resolving it against the local
filesystem.

#### Scenario: Explicitly assigning a worktree queues its branch

- **WHEN** a user runs `merge add <worktree>`
- **THEN** that worktree's current branch is recorded in the queue as `queued`
  against the repo's target branch, together with the worktree's location

#### Scenario: Assigning all eligible branches

- **WHEN** a user runs `merge add --all` in a repo
- **THEN** every eligible worktree branch (excluding the target branch and, absent
  `snapshot_dirty`, dirty worktrees) is queued

#### Scenario: Removing and clearing entries

- **WHEN** a user runs `merge rm <worktree>` or `merge clear`
- **THEN** the named entry (or every entry for the repo) is removed from the queue

#### Scenario: A queued worktree on another host is still attributed to its repo

- **WHEN** the queue lists rows for a repo and a queued worktree lives on another
  host (so it cannot be resolved by a local `git worktree list`)
- **THEN** that row is still attributed to its repo by its recorded `repo_path`
  and included in the repo's queue, instead of being silently dropped

## ADDED Requirements

### Requirement: Cross-host branch tips are fetched into the target store before folding

The merge queue folds each branch into the target repo's object store. When a
queued branch's worktree lives on a different host from the target repo, its tip
commit exists only in that branch host's own object store and is therefore absent
from the target. thegn SHALL make the tip present in the target store before
folding, by creating a git bundle of the branch on its own host, transferring it
to the target host, and fetching it under a synthetic ref
(`refs/thegn/mq/<branch>`) that the fold then merges. A branch that already
shares the target store (local, or the same host as the target) SHALL be folded
directly from `refs/heads/<branch>` with no transfer. If the branch host is
unreachable or the transfer fails, the branch SHALL be deferred with the reason
recorded and MUST NOT be silently dropped, so a transient failure is retried on
the next drain.

#### Scenario: An off-host branch lands via a fetched tip

- **WHEN** the driver drains a queued branch whose worktree is on another host and
  the host is reachable
- **THEN** the branch's tip is bundle-fetched into the target store under
  `refs/thegn/mq/<branch>`, folded onto the target, and landed like a same-host
  branch

#### Scenario: A same-host branch needs no transfer

- **WHEN** the driver drains a queued branch whose worktree shares the target's
  object store
- **THEN** it is folded directly from `refs/heads/<branch>` with no bundle/fetch

#### Scenario: An unreachable branch host defers the row

- **WHEN** a queued branch's host is unreachable or its tip cannot be fetched in
- **THEN** the row is marked `deferred` with the reason, is not dropped, and is
  retried on a later drain

### Requirement: The drain is anchored to the target repo's host

Because the fold, test-gate, and CAS-advance all operate in the target repo's
object store and working tree, the drain SHALL run on the host where the target
repo lives. When a drain, land, or integrate command is invoked for a repo whose
target store lives on another host, thegn SHALL decline to fold in place and
SHALL tell the user which host to run the drain on, rather than attempting a
partial fold or failing obscurely.

#### Scenario: Draining a remote-target queue guides to the target host

- **WHEN** a user runs `merge drain`, `merge land`, `land`, or `integrate` for a
  repo whose target branch lives on another host
- **THEN** thegn declines to fold locally and reports the host on which the drain
  must be run (where off-host branch tips are fetched in automatically)
