---
name: mq
description: Manage superzej's local merge queue from inside a worktree — add the current branch to the queue, clear it, list it, or drain it. Use when the user asks to queue a branch for merge, empty/clear the merge queue, or check what's queued.
---

# superzej merge queue (`/mq`)

You are inside a **superzej** worktree. superzej has a local, test-gated merge
queue that folds worktree branches onto the repo's target branch. Drive it with
the `szhost` CLI (already on your PATH; `sj`/`superzej` are aliases). All commands
operate on the **current** worktree/repo — run them from within the worktree.

## Actions

- **`/mq add`** — add this worktree's current branch to the queue:

  ```bash
  szhost merge add
  ```

  Add every eligible branch in the repo with `szhost merge add --all`.

- **`/mq clear`** — empty the queue for this repo:

  ```bash
  szhost merge clear
  ```

- **`/mq list`** — show what's queued:

  ```bash
  szhost merge list
  ```

  Add `--json` for machine-readable output.

- **`/mq drain`** — process the queue one branch at a time (fold + gate; the
  autopilot). This can land branches, so confirm with the user first:
  ```bash
  szhost merge drain
  ```

## Notes

- These are no-ops if the merge queue is disabled (`[merge_queue] enabled = false`);
  the command will say so.
- Adding a branch that _is_ the target branch is skipped automatically.
- `add`/`clear`/`list` are safe and reversible (`szhost merge rm <worktree>` removes
  a single entry). `drain` mutates the target branch — treat it as a landing action.
