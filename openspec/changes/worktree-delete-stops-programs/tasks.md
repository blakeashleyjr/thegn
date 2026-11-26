# Tasks

## 1. Refuse unsafe targets

- [x] 1.1 Add `thegn_core::worktree::removal_refusal` for the main checkout and
      a Git-locked worktree, read from `.git` and Git's `locked` marker.
- [x] 1.2 Refuse in the shared destroy step before any hook, stop or teardown.

## 2. Stop programs before deleting

- [x] 2.1 Hand each deleted group's pane processes (PTY owner or daemon session
      child) to the destroy worker without loop-side I/O.
- [x] 2.2 Stop them after `pre_destroy` and wait, bounded, for the folder's
      processes to exit before teardown deletes it.

## 3. Tests and docs

- [x] 3.1 Test that a running `sleep` is stopped and the folder removed, branch
      kept; and that main and locked worktrees are refused with nothing stopped.
- [x] 3.2 Document the behavior in the worktree help and quick reference.
