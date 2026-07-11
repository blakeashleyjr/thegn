# Git Backend

## Purpose

thegn reads and writes git state through a service seam that prefers fast,
native (gix) reads but always degrades to the git CLI for writes and for any
operation the native path does not cover. git remains the source of truth for
worktrees.

## Requirements

### Requirement: GitBackend trait with native-first reads

Git operations SHALL go through the `GitBackend` trait; reads MUST prefer the gix-native provider for speed and MUST fall back to the git CLI when the native path is missing or errors.

#### Scenario: Native read succeeds

- **WHEN** a supported read (e.g. ahead/behind, status) is requested and the gix
  provider can serve it
- **THEN** the native provider answers without spawning the git CLI

#### Scenario: Native gap falls back to CLI

- **WHEN** a requested operation is not implemented natively or the native call
  fails
- **THEN** the backend transparently falls back to the git CLI subprocess

### Requirement: Writes go through the CLI

Mutating git operations SHALL be performed via the git CLI to match git's exact write semantics.

#### Scenario: Write operation

- **WHEN** a write operation (e.g. commit, branch creation) is invoked
- **THEN** it is executed through the git CLI

### Requirement: git is the source of truth for worktrees

The set of worktrees SHALL be derived from git, and the SQLite DB MUST act only as a cache/resurrection layer that never overrides what git reports.

#### Scenario: DB disagrees with git

- **WHEN** the DB's cached worktree list differs from git's actual worktrees
- **THEN** git's view wins and the cache is reconciled to match

### Requirement: All git invocations run with a scrubbed git environment

Every git invocation SHALL go through the `util::git_cmd`/`GitLoc` builders that `env_remove` the `GIT_ENV_VARS` (e.g. `GIT_DIR`/`GIT_WORK_TREE`) on each call, and raw `Command::new("git")` MUST NOT be used outside that single builder (enforced by `just lint`).

#### Scenario: Inherited GIT_DIR does not leak

- **WHEN** a git command runs in a process whose environment carries
  `GIT_DIR`/`GIT_WORK_TREE`
- **THEN** the builder strips them so the command operates on its `-C <dir>`
  target rather than the inherited shared `.git`

#### Scenario: Raw git is rejected

- **WHEN** code invokes git outside the `git_cmd`/`GitLoc` builder
- **THEN** `just lint` fails on the grep guardrail

### Requirement: Self-heal shared .git/config pollution

thegn SHALL surgically strip a stray `core.worktree` from a main checkout's shared `.git/config` at startup and again on each worktree switch, so a poisoned shared config never retargets repo-wide git operations at the wrong worktree.

#### Scenario: Stray core.worktree at startup

- **WHEN** a main checkout's `.git/config` contains an invalid `core.worktree`
- **THEN** thegn removes it (text edit) across cwd, session worktrees, and the
  `--git-common-dir` parent before serving git reads

#### Scenario: Mid-session pollution heals

- **WHEN** another process writes a stray `core.worktree` while thegn runs
- **THEN** the next worktree switch heals it off-thread

### Requirement: Serialize concurrent git mutations with a cross-process lock

Mutating git operations SHALL take an advisory `flock` on `<git-common>/thegn-git.lock` so concurrent thegn/agent processes on the same repo serialize their writes, while reads remain lock-free.

#### Scenario: Two writers serialize

- **WHEN** two processes attempt git mutations on the same repo at once
- **THEN** they acquire the lock in turn rather than clobbering the shared
  index/refs

#### Scenario: Reads are not blocked

- **WHEN** a git read runs concurrently with a mutation
- **THEN** the read proceeds without taking the lock
