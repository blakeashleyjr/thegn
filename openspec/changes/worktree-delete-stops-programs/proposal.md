# Stop a worktree's programs before deleting it, and refuse unsafe targets

Linear: THE-2736

## Why

Deleting a worktree from disk removed the folder while its pane programs (dev
servers, agents) were still running. They were reaped only after the folder
was gone, so a server could keep writing into a half-deleted checkout. A
worktree locked with `git worktree lock` was refused only late, by Git, after
hooks and sandbox teardown had already run.

## What Changes

- A delete-from-disk hands the worktree's pane processes to the destroy
  worker. After the vetoing `pre_destroy` hook passes, the worker stops them
  (terminate, grace, kill) and, on Linux, waits up to 5 seconds for any process
  using the folder. Only then does teardown delete the folder. Anything still
  there is named in the status line.
- The main checkout and a Git-locked worktree are refused with a plain reason
  before anything is stopped or closed. This applies to every removal path that
  goes through the shared destroy step, including `thegn wt rm`.
- The branch is kept, as before.
