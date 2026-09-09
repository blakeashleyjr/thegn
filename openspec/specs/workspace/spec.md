# workspace Specification

## Purpose

Adding, discovering, and removing workspaces (git repos and plain directories): discovery-first creation, the fuzzy new-workspace picker, and orphan-aware non-destructive deletion.

## Requirements

### Requirement: Create a workspace from a path or git URL

The native host SHALL let the user create a workspace by entering a local path or a git URL, cloning a URL into the workspaces directory and validating a path as an existing git repository before registering the workspace and switching to it, and MUST reject invalid input (empty, non-existent path, or failed clone) with a clear error rather than registering a broken workspace.

#### Scenario: Create from an existing repo path

- **WHEN** the user enters a path to an existing git repository
- **THEN** the workspace is registered and the session switches to it

#### Scenario: Create from a git URL

- **WHEN** the user enters a git URL
- **THEN** the repository is cloned into the workspaces directory, registered, and switched to

#### Scenario: Directory that is not a git repository

- **WHEN** the user enters a path to a directory that is not a git repository
- **THEN** the host offers to initialize a git repository there before registering it

#### Scenario: Invalid path is rejected

- **WHEN** the user enters a path that does not exist
- **THEN** an error is shown and no workspace is registered

### Requirement: Create a workspace from an auto-discovered repo

When repositories exist under the configured root directories, the native create action SHALL present them for selection so the user can register one without typing its path, while still offering a way to type an arbitrary path or URL.

#### Scenario: Pick a discovered repo

- **WHEN** the user triggers the create action and repositories are discovered under the configured roots
- **THEN** a picker lists those repositories and selecting one registers and switches to it

#### Scenario: No repos discovered

- **WHEN** the user triggers the create action and no repositories are discovered
- **THEN** the host opens the path-or-URL entry prompt directly

### Requirement: Delete a workspace with confirmation, keeping files optional

Deleting a workspace SHALL require confirmation and MUST remove the database registration while leaving the workspace's worktree files on disk when the user chooses to keep them, reporting how many worktrees remain, and MUST switch to the next available workspace or fall back to an empty home when none remain; a destructive option that also deletes the branch worktree directories from disk MAY be offered as an explicit confirmed choice.

#### Scenario: Keep files reports orphaned worktrees

- **WHEN** the user confirms deletion of a workspace with the keep-files option and the workspace still has registered worktrees
- **THEN** the registration is removed, the worktree files on disk are untouched, and the status reports how many worktrees remain on disk

#### Scenario: Delete the last workspace

- **WHEN** the user deletes the only remaining workspace
- **THEN** the session falls back to an empty home rather than leaving no context

### Requirement: A workspace may belong to a zone

A workspace SHALL optionally belong to exactly one zone within its profile,
recorded as membership in the state database and never inferred from a filesystem
path. The `thegn zone` command SHALL create, rename, list, delete, and assign
zones; assigning ensures the workspace is registered, and deleting a zone with
members is refused unless forced (which unassigns its members first).

#### Scenario: Assigning a repo records membership

- **WHEN** `thegn zone assign clientA <repo>` is run
- **THEN** the repo's workspace belongs to zone `clientA` and the zone's member
  count includes it

#### Scenario: Deleting a non-empty zone is refused

- **WHEN** `thegn zone rm clientA` is run while `clientA` has members
- **THEN** the deletion is refused with a message, unless `--force` is given

#### Scenario: Membership is not path-inferred

- **WHEN** a worktree's filesystem path resembles a zone name
- **THEN** its zone is determined solely by recorded membership, not the path

### Requirement: Workspace clone initializes submodules through the shared seam

Workspace creation SHALL perform its ordinary superproject clone and then
invoke the shared strict, configuration-controlled, trust-gated recursive
submodule initializer. It MUST NOT bypass that trust/failure contract by
implicitly recursing repo-controlled submodule URLs during the clone itself.
Remote/provider creation SHALL provide the same accepted/pending/degraded
outcome semantics.

#### Scenario: Clone succeeds while init degrades

- **WHEN** the superproject clone succeeds but an approved recursive submodule
  update fails
- **THEN** the workspace remains registered and the initialization failure is
  surfaced for recovery

### Requirement: Worktree lifecycle hooks run at defined points

thegn SHALL run configured `[hooks]` command lists at worktree lifecycle
events: `pre_create` before `git worktree add` (cwd = repo root),
`post_create` after the worktree is registered and after built-in env
provisioning (the legacy `[sandbox] prepare` list executes as the head of
`post_create`, preserved as a documented alias), `pre_destroy` before
`git worktree remove` (cwd = worktree), and `post_destroy` after removal
(cwd = repo root). Hooks SHALL be configurable at global,
`[workspace.<slug>]`, and repo `.thegn.toml` scopes; scopes accumulate and
execute sequentially in global → workspace → repo order, declaration order
within a scope. `post_create` runs in parallel with the first pane by
default; an entry with `wait = true` MUST delay the first pane until it
completes.

#### Scenario: Setup runs after provisioning, before the hook's dependents

- **WHEN** a worktree is created with a global
  `post_create = ["pnpm install"]`
- **THEN** the command runs in the new worktree after the built-in
  prepare/direnv/devshell provisioning, and the first pane does not wait for
  it

#### Scenario: Teardown accumulates across scopes

- **WHEN** both the global config and the workspace declare `pre_destroy`
  entries and the user deletes the worktree
- **THEN** the global entries run first, then the workspace entries, before
  `git worktree remove`

### Requirement: Hook failure semantics are per-event with safe defaults

A failing `pre_create` hook SHALL block creation (nothing exists yet). A
failing `pre_destroy` hook SHALL block a user-invoked destroy and offer an
explicit force override, but MUST only warn and continue for unattended
removal (merge-lifecycle reclaim) — a dead teardown script must not wedge the
queue. `post_create`, `post_destroy`, `session_start`, and `session_end`
failures warn. A per-entry `on_failure = "block" | "warn"` MAY adjust within
these bounds, except that repo-sourced entries are always warn-only.

#### Scenario: A failing pre_create aborts cleanly

- **WHEN** a global `pre_create` hook exits non-zero
- **THEN** no worktree is created and the failure (with output tail) is
  surfaced

#### Scenario: A failing pre_destroy blocks with a force path

- **WHEN** a user deletes a worktree and a workspace `pre_destroy` hook fails
- **THEN** the worktree is not removed, the failure is surfaced, and the user
  can force the removal, which skips the failed hook

#### Scenario: Unattended reclaim is never wedged

- **WHEN** merge-lifecycle cleanup removes a merged worktree and its
  `pre_destroy` hook fails
- **THEN** the removal proceeds and the failure is surfaced as a warning

### Requirement: Hooks execute off-loop under one contract

Every hook SHALL run `sh -lc` off the event loop with the curated base
environment plus `THEGN_EVENT`, `THEGN_REPO_ROOT`, `THEGN_WORKTREE`,
`THEGN_BRANCH`, and `THEGN_WORKSPACE` — never thegn's full process
environment. Each hook has a timeout (default 120 s, `timeout_secs` per
entry); on timeout the process group is killed and the hook counts as failed.
Output MUST be captured to a per-worktree state-dir log and a failure
notification MUST include its tail. Completion is delivered over the existing
refresh channel with a `TerminalWaker` pulse — no polling, preserving the
idle invariant — and hook processes join the shared resource slice so they
cannot escape `[sandbox.limits]`.

#### Scenario: A hung hook is bounded

- **WHEN** a `post_create` hook exceeds its timeout
- **THEN** its process group is killed, the hook is reported failed with its
  captured output, and the worktree remains usable

#### Scenario: Hooks do not inherit the user's shell secrets

- **WHEN** thegn was launched from a shell exporting `GH_TOKEN` and a hook
  runs
- **THEN** the hook's environment contains the curated base and `THEGN_*`
  context, not `GH_TOKEN`

### Requirement: Session hooks bracket a worktree's live session

thegn SHALL run `session_start` when a worktree's first pane of a UI session
spawns and `session_end` when its last pane exits or the tab closes. Both are
warn-only and MUST never delay or block pane spawn or close; per-pane setup
remains `init_script`'s job.

#### Scenario: One session_start per session, not per pane

- **WHEN** a worktree opens three panes in one session
- **THEN** `session_start` runs once, before or alongside the first pane, and
  not for the second or third

### Requirement: Repo-authored hooks are trust-gated and can never block

Repo `.thegn.toml [hooks]` entries SHALL be trust-on-first-use gated per
event category (`hooks.<event>`), matched by canonical form so an edit
re-prompts. An unapproved repo hook MUST NOT run and MUST NOT block the
operation (which proceeds, with the request surfaced as pending). An approved
repo hook remains warn-only: blocking semantics are reserved for global and
workspace scopes, so a cloned repository can never hold worktree creation or
removal hostage.

#### Scenario: A cloned repo's hook does not run on first open

- **WHEN** a worktree is created from a repo whose `.thegn.toml` declares
  `post_create` hooks with no recorded approval
- **THEN** the hooks do not run, the request is surfaced as pending, and the
  worktree opens

#### Scenario: An approved repo pre_destroy cannot veto removal

- **WHEN** an approved repo `pre_destroy` hook exits non-zero during a
  user-invoked delete
- **THEN** the removal proceeds and the failure is surfaced as a warning

### Requirement: One-repository workspace presentation uses project vocabulary

Sidebar chrome, palette/action labels, prompts, menus, help, configuration
prose, and human CLI help SHALL call thegn's one-repository object a project.
Canonical action ids SHALL use project spellings, while old workspace action ids
remain accepted compatibility aliases and workspace terms remain searchable.
Machine JSON/state identifiers remain stable.

#### Scenario: Project action uses an old keybinding

- **WHEN** existing configuration binds the legacy workspace action id
- **THEN** the canonical project action dispatches with the same chord and no
  duplicate palette row

### Requirement: Multi-repository groups use program vocabulary

The existing multi-repository CLI namespace and capability ids SHALL use
`program`. Exact legacy `project` command/flag/capability forms SHALL retain
their behavior as deprecated compatibility aliases during the bounded window,
with diagnostics excluded from machine-readable output.

#### Scenario: Legacy multi-repo command runs

- **WHEN** a user invokes the former `thegn project` command
- **THEN** the corresponding `thegn program` behavior runs and a human warning
  identifies the canonical spelling

#### Scenario: JSON remains stable

- **WHEN** a compatibility alias is used with machine-readable output
- **THEN** the documented JSON schema/fields are unchanged and no warning is
  mixed into the JSON stream
