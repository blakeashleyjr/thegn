---
description: Drain the thegn merge queue — land clean branches, resolve conflicts yourself, then move to the next
allowed-tools: Bash, Read, Edit, Write, Grep
---

You are the **merge-queue drainer** for this thegn repo. Your job: land every queued branch onto its target branch (usually `main`), **resolving conflicts and build breakages yourself**, one branch at a time, until nothing drainable remains.

thegn owns the actual fold + ref advance. You never check out, merge into, or push the target branch — you only make each blocked branch _clean in its own worktree_, then let thegn land it.

(The wide `allowed-tools` above is deliberate: fixing a red gate means running
_this project's_ test command and editing whatever it points at, which cannot be
enumerated ahead of time.)

## Loop

Repeat these steps until every remaining branch is `landed`, `ready`, or `needs_human`:

1. **Sweep the clean ones.** Run `thegn merge drain --json`. This folds every branch that merges clean and defers the rest. Read the JSON: `landed`, `ready`, `deferred`, `needs_human`.

2. **List the queue with worktrees.** Run `thegn merge list --json` to get each branch's `worktree` path, `branch`, `target_branch`, `status`, and conflict/error detail.

3. **Stop condition.** If no branch has status `deferred` or `gate_failed`, you are done — go to _Report_.

4. **Fix one blocked branch.** Pick a `deferred`/`gate_failed` branch you have **not already attempted this run**. Let `WT` = its worktree, `TARGET` = its `target_branch`. All git runs use `-C "$WT"` (a fresh shell each call — do not rely on `cd` persisting) and a non-interactive editor: prefix rebase/commit git calls with `GIT_EDITOR=true` (or `-c core.editor=true`).
   - **Conflict (`deferred` with conflict paths):**
     - `git -C "$WT" fetch --all --quiet`
     - `GIT_EDITOR=true git -C "$WT" rebase "$TARGET"`
     - For each conflicted file (`git -C "$WT" diff --name-only --diff-filter=U`): open it, resolve every conflict marker preserving the intent of **both** sides, then `git -C "$WT" add <file>`.
     - `GIT_EDITOR=true git -C "$WT" rebase --continue`. Repeat resolve → add → continue until the rebase finishes clean. If it becomes unrecoverable, `git -C "$WT" rebase --abort` and mark the branch needs-human (see below).
   - **Build breakage (`gate_failed`):** the merge is clean but the test gate is red. In `WT`, run the project's gate/tests, read the failures, fix the code, and commit the fix on this branch (`GIT_EDITOR=true git -C "$WT" commit -am "fix: <what>"`).
   - Ensure `git -C "$WT" status --short` is clean (everything committed) when done.

5. **Mark it attempted** (track branch names you've tried this run so you never loop forever on the same one), then go back to step 1 — the next `thegn merge drain` will land the now-clean branch.

6. **Give-up guard.** If a branch is still `deferred`/`gate_failed` after you attempted it, do not retry it again — leave it for a human and continue with the others. If every remaining blocked branch has already been attempted, stop.

## Report

When the loop ends, print a summary:

- ✓ landed: `<branch> → <sha>` for each.
- ⚑ needs a human: `<branch>` + one-line reason (unresolved conflict / gate still red).
- Confirm with a final `thegn merge list`.

Rules (repeat to yourself): work only inside each branch's own worktree; commit your fixes on that branch; **never push**; **never check out, merge into, or reset the target branch** — thegn folds and advances it once the branch is clean.
