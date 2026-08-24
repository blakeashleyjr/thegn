# Sandbox

## MODIFIED Requirements

### Requirement: Worktree stays on the host and is bind-mounted

A sandboxed worktree SHALL remain on the host filesystem and MUST be bind-mounted
into the container at a **deterministic** destination, so host-side git reads and
the compositor continue to operate on the same files the sandboxed process edits.

Where the sandbox's path namespace can represent the host path — every unix host
— that destination MUST be the real host path, and the contract is unchanged.
Where it cannot (a Linux container on native Windows), thegn MUST map the host
path deterministically **and** MUST make the worktree's git metadata resolve
under that mapping, so that `git` inside the sandbox and `git` on the host
address the same repository. A sandbox in which in-worktree `git` cannot resolve
its own gitdir SHALL NOT be selected.

The metadata mapping MUST NOT mutate the host's own pointer files, and MUST NOT
be attempted through `GIT_DIR`/`GIT_WORK_TREE`, which thegn deliberately scrubs
from every pane environment.

#### Scenario: Host git reads remain coherent

- **WHEN** a worktree process runs inside a container backend
- **THEN** the worktree is bind-mounted at its deterministic destination and git
  status/diff read from the host see the same working tree the sandboxed process
  edits

#### Scenario: Linked worktree under a mapped destination

- **WHEN** a linked worktree — whose `.git` is a pointer file carrying an
  absolute host path — runs under a sandbox whose mount destination differs from
  its host path
- **THEN** the sandbox sees a `.git` pointer and `gitdir` back-pointer that
  resolve to the mapped gitdir, `git status` and `git commit` inside the sandbox
  operate on the repository the host sees, and the host's own pointer files are
  left byte-identical

## ADDED Requirements

### Requirement: Sibling worktree metadata is read-only inside a sandbox

A sandboxed process SHALL NOT be able to modify or delete the git metadata of any
worktree other than its own. `<git-common>/worktrees` MUST be mounted read-only
with the pane's own `<git-common>/worktrees/<id>` overmounted read-write, so the
pane retains full function (commit, rebase, index writes) while sibling metadata
is unreachable.

This prevents a sandboxed `git worktree prune` or `git gc` from deleting host
worktree metadata whose recorded path the sandbox cannot see — a hazard that is
universal on native Windows and latent on unix wherever a sibling worktree is not
otherwise visible inside the container.

A consequence, and accepted: `git worktree add` from inside a sandboxed pane
fails, because it must write a new entry under `<git-common>/worktrees`.

#### Scenario: In-sandbox prune cannot reach siblings

- **WHEN** a sandboxed process runs `git worktree prune`
- **THEN** no sibling worktree's metadata is removed, and the host's view of
  every other worktree is unchanged

#### Scenario: The pane's own worktree stays writable

- **WHEN** a sandboxed process commits in its own worktree
- **THEN** the write to its own `<git-common>/worktrees/<id>` succeeds
