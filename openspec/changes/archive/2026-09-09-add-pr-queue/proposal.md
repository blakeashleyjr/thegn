# PR queue — babysitting pull requests on the remote

## Summary

The merge queue is a **solo-dev** primitive: it folds local worktree branches
onto a local `main` in the object database and CAS-advances the ref. It has zero
forge awareness — no CI gate, no review gate, no mergeability check.

On a team, that model stops applying. `main` is owned by the remote, protected by
branch rules, and advanced by the forge. What a developer actually babysits is a
**pull request**: it goes red on CI, it falls behind the base, a reviewer asks
for changes, and it sits there. thegn already _displays_ all of that — `PrPanel`
carries `status_check_rollup`, `mergeable`, `merge_state_status`,
`review_decision`, and `threads` — but acts on none of it.

This adds a **PR queue**: enqueue a PR, thegn polls its remote state, classifies
what is blocking it, optionally dispatches a configurable agent in that PR's
worktree to unblock it, and lets the forge merge it once it goes green.

It is deliberately the mirror image of the merge queue's guarantee. There, the
agent never touches the target and thegn does the fold. Here, **the agent never
merges the PR** — thegn either hands merging to the forge's own auto-merge (the
default, so branch protection and required reviews stay authoritative) or calls
the merge itself only when explicitly configured to.

## Impact

- Roadmap: **Z 338** (PR event notifications) and **Z 340** (multi-repo PR
  dashboard) get their acting counterpart; **AT 638** (PR triage states) and
  **AT 646** (two-way comment/approval sync) are the multi-forge generalization
  this is shaped for. Adds a new item to group **Z**.
- Spec: new `pr-queue` capability. `state-db` — ADDED the `pr_queue` table.
- Code: new `thegn-core/src/{config_pr_queue,pr_queue}.rs` (config + pure
  classification), `thegn-svc/src/prq.rs` (the forge seam),
  `thegn-host/src/{pr_driver,pr_queue_refresh}.rs`, `handlers/pr_queue.rs`,
  `panel/sections/pr_queue.rs`, `cmd/pr_queue.rs`.
- **DB schema change: `user_version` bump** for the `pr_queue` table.
- Three new action ids (`pr-queue-open`, `pr-queue-add`, `pr-queue-drain`), a new
  panel section (`panel:prq`), and three `NotificationKind`s — so a new
  `docs/help/pr-queue.md` claims them (the help ratchet enforces this).

## Rationale

The fetching already exists. `thegn_core::github` ships `pr_status_full`,
`check_bucket`, `review_threads`, `reply_to_thread`, `merge_pr`,
`set_auto_merge`, and `rerun_failed_checks`; `ci.rs` ships a normalized
run/job/step/log model with `first_failure_line`. What is missing is a state
machine over that data and somewhere to hang an agent — both of which now have
established shapes in this codebase (the merge queue's driver, and the
`agent_task` engine extracted for exactly this).

Building it against a narrow `PrQueueForge` trait rather than calling `github::`
directly is a deliberate down-payment on **AT 631** (forge backend abstraction)
without building that whole group now: GitLab/Gitea implement six methods.

## Non-goals

- **Replacing the merge queue.** They coexist: the merge queue lands local
  branches onto a local target, the PR queue shepherds PRs on a remote. A repo
  can use either or both.
- **Building the forge abstraction (AT 631).** Only the six operations the queue
  needs are behind a trait; the rest of the GitHub surface is untouched.
- **Resolving review threads.** thegn can reply to a thread; marking it resolved
  is the reviewer's call, and there is deliberately no mutation for it.
- **Merging without the forge's consent by default.** `merge_mode = "auto_merge"`
  is the default precisely so branch protection stays in charge.
- **A cross-repo PR dashboard (Z 340 / AT 637).** The queue is per-repo; the
  existing `my_work` feed already aggregates across repos.
- **Non-GitHub providers in this change.** The trait exists; only `GithubPrq`
  implements it here.
