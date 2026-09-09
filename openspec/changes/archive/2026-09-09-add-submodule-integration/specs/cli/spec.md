# CLI

## ADDED Requirements

### Requirement: CLI worktree creation uses the shared submodule initializer

After `wt new` creates a checkout, it SHALL invoke the same `[git].submodules`
post-checkout initializer as UI and remote creation. In `auto`, initialization
SHALL run only for valid metadata and after the repository's current submodule
request is approved; in `off`, it SHALL be skipped. Failure SHALL be surfaced
without deleting the successfully-created worktree.

#### Scenario: Initialization is pending approval

- **WHEN** `wt new` creates a worktree whose repo declares submodule URLs not
  yet approved
- **THEN** the worktree remains created, init does not run, and the CLI reports
  the pending trust action
