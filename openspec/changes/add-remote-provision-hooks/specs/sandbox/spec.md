# Sandbox

## ADDED Requirements

### Requirement: A remote workspace can run user-authored per-task provision and teardown hooks

thegn SHALL run an ordered set of user-authored provision hooks when bringing
up a remote (sprite) workspace for a task, and matching teardown hooks when it is
torn down, passing each hook the task context (task id, repo, branch, worktree
path) in its environment. Hooks MUST run off the event loop with a per-hook
timeout, and a hook exiting non-zero MUST fail bring-up under the existing
non-local-env failover/halt policy. An empty hook list MUST reproduce the prior
provider-only behavior.

#### Scenario: Provision hooks run on bring-up with task context

- **WHEN** a remote workspace is brought up for a task with provision hooks
  configured
- **THEN** each hook runs in order with the task id, repo, branch, and worktree
  path in its environment before the task starts

#### Scenario: A failing provision hook halts bring-up

- **WHEN** a provision hook exits non-zero
- **THEN** bring-up fails under the configured failover/halt policy rather than
  starting the task on a half-provisioned host

#### Scenario: Teardown hooks run on teardown

- **WHEN** the task's remote workspace is torn down
- **THEN** the configured teardown hooks run in order

#### Scenario: No hooks reproduces provider-only behavior

- **WHEN** no provision or teardown hooks are configured
- **THEN** the workspace uses the provider's static bring-up/teardown unchanged

### Requirement: Provision hooks compose with the warm sandbox pool

thegn SHALL skip provision hooks for a task that claims a warm pool spare
(already provisioned) and run them for a task that starts fresh, so hooks do not
re-provision an already-live spare.

#### Scenario: A claimed spare skips provision

- **WHEN** a task claims a warm pool spare
- **THEN** provision hooks are not re-run for that task

#### Scenario: A fresh task runs provision

- **WHEN** a task starts without claiming a spare
- **THEN** its provision hooks run
