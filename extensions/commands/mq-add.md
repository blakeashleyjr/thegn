---
description: Commit this worktree's branch and add it to the thegn merge queue
argument-hint: "[optional commit message]"
allowed-tools: Bash(git status:*), Bash(git symbolic-ref:*), Bash(git add:*), Bash(git commit:*), Bash(git diff:*), Bash(thegn merge add:*), Bash(thegn merge list:*)
---

You are adding the **current git worktree's branch** to the thegn merge queue so a drainer can land it onto the target branch. The queue only folds _committed_ branches, so make sure the work is committed first.

Do this:

1. Determine the branch and whether there's uncommitted work:
   - `git symbolic-ref --quiet --short HEAD` (must NOT be the target branch, usually `main`; abort with a clear message if it is, or if HEAD is detached).
   - `git status --short`
2. If there are uncommitted changes, commit them (do NOT push, do NOT switch branches):
   - Stage with `git add -A`.
   - Commit using conventional-commit style. If `$ARGUMENTS` is non-empty, use it as the commit message/subject; otherwise write a concise message summarizing the diff (`git diff --staged --stat`).
   - If the working tree is already clean, skip this step.
3. Enqueue the branch: `thegn merge add`
4. Confirm: `thegn merge list`

Report the branch name, the commit you made (if any), and the branch's line in `thegn merge list`. Never push and never check out or modify the target branch — the queue lands it for you.
