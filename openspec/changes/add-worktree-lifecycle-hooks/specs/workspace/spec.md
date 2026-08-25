# Workspace

## ADDED Requirements

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
