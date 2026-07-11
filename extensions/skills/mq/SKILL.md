---
name: mq
description: Manage thegn's local merge queue from inside a worktree — add the current branch to the queue, clear it, list it, or drain it. Use when the user asks to queue a branch for merge, empty/clear the merge queue, or check what's queued.
---

# thegn merge queue (`/mq`)

You are inside a **thegn** worktree. thegn has a local, test-gated merge
queue that folds worktree branches onto the repo's target branch. Drive it with
the `thegn` CLI (already on your PATH; `tg` is an alias). All commands
operate on the **current** worktree/repo — run them from within the worktree.

## Actions

- **`/mq add`** — add this worktree's current branch to the queue:

  ```bash
  thegn merge add
  ```

  Add every eligible branch in the repo with `thegn merge add --all`.

- **`/mq clear`** — empty the queue for this repo:

  ```bash
  thegn merge clear
  ```

- **`/mq list`** — show what's queued:

  ```bash
  thegn merge list
  ```

  Add `--json` for machine-readable output.

- **`/mq drain`** — process the queue one branch at a time (fold + gate; the
  autopilot). This can land branches, so confirm with the user first:
  ```bash
  thegn merge drain
  ```

## Notes

- These are no-ops if the merge queue is disabled (`[merge_queue] enabled = false`);
  the command will say so.
- Adding a branch that _is_ the target branch is skipped automatically.
- `add`/`clear`/`list` are safe and reversible (`thegn merge rm <worktree>` removes
  a single entry). `drain` mutates the target branch — treat it as a landing action.
