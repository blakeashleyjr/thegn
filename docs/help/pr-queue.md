---
id: pr-queue
title: PR queue
parent: workflows
order: 3
contexts: [panel:prq#watching]
actions: [open-pr-queue, pr-queue-add, pr-queue-refresh, pr-review-task-handle]
---

# PR queue

The merge queue's counterpart for a **shared** repo. Where the merge
queue folds local branches onto a local target, the PR queue watches
**pull requests** on the forge: it polls each queued PR, works out what
is blocking it, optionally hands that blocker to an agent, and lets the
PR merge once it is green.

Off by default — this is the one part of the shell that makes network
_writes_. Turn it on with `[pr_queue] enabled = true`.

## Queueing

`a` in the _prq_ section queues the current worktree's pull request;
`thegn pr queue add` does the same from a shell, and
`thegn pr queue add --pr 42` queues one by number. A PR queued by number
has no worktree — it is watched and displayed, but an agent has nowhere
to work, so the row says `(no worktree)` and asks for a human instead of
going quiet.

Queueing is explicit by default: thegn does not watch every PR on the forge.
Set `auto_enqueue = "worktree"` if you would rather every PR you open from a
worktree be queued.

The add command stores the forge PR URL and checks that later fetches
still identify the same repository and PR number before refreshing reviews,
merging, rerunning checks, or launching an agent. On upgrade, older active rows
have no recorded identity and are held for a human; re-add the PR with
`thegn pr queue add --pr 42` to verify and bind it.

Press `r` on a settled row to re-verify it with the forge selected from the
repository root. For a row without a recorded identity, this binds the verified
PR URL and starts watching again. If the fetched PR does not match a bound
identity or forge, re-watch is refused. Use `thegn pr queue rebind --pr 42` to
verify and rebind it, with
`--worktree <path>` or `--no-worktree` to set its checkout. Adding a row whose
checkout changed is refused and names this rebind command.

## What blocks a PR

Each pass classifies one blocker, most actionable first:

- **draft** — yours to mark ready; thegn won't touch it.
- **CI failing** — the named checks went red.
- **conflict** — it conflicts with, or has fallen behind, its base.
- **changes requested** — a reviewer wants something.
- **awaiting review** — nobody has looked yet. This is the normal
  resting state of a healthy PR, so it is amber, never red.

Checks still running are just "not yet", and never mask a failure that
is already visible.

## Merging

When a PR passes every gate, the default hands the merge to the **forge**:

```toml
[pr_queue]
merge_mode = "auto_merge"   # the forge merges it, under ITS rules
merge_method = "squash"
require_approval = true
require_checks = true
```

That default is the point. Branch protection, required reviews, and any
server-side merge queue stay authoritative, so thegn's view of "ready"
can never race a rule it cannot see. `merge_mode = "thegn"` merges
directly when thegn's own gates pass; `"ready"` never merges and just
tells you.

A draft is never merged, and neither is a PR without its approval when
`require_approval` is set. Before merging, rerunning checks, or waking an agent,
thegn re-attests the row identity, PR head, and bound checkout. A merge is pinned
to the head it evaluated; if the head moved, the row returns to watching with
`head moved from <old> to <new> before the merge; refreshing`. A forge that
cannot pin the head refuses the merge. Drain counts merged, merge requested
(waiting for forge confirmation), and auto-merge armed separately.
If a later poll confirms a requested merge at the same head, the row records
`merged (confirmed on a later poll)` and the drain counts it as merged. A merge
at a different head, or one not requested by thegn, is recorded as merged on
the forge but is not counted as a thegn merge.

## The agent

Point it at an agent the same way the [[merge-queue]] does — an
`[[agents]]` name, or a full command:

```toml
[pr_queue]
agent = "claude"
watch = ["ci", "conflict", "review"]   # which blockers may wake it
agent_max_attempts = 2
```

A blocker class left out of `watch` is still tracked and displayed; it
just never gets an agent. With no agent configured, everything is
reported and nothing is written.

The built-in prompts are the **inverse** of the merge queue's: the agent
_must_ push (that is the only way a PR advances) but must never merge,
close, approve, or call the forge to resolve a review thread. The verified
review-task lifecycle below owns any reply and resolution. Override prompts
per kind under `[pr_queue.prompts]`; keep those rules if you do.

Before waking an agent on a red build thegn re-runs the failed checks
once, since plenty of red CI is a flake.

When the CI cache has failed-job logs captured for the PR's current head
commit, the CI-failure prompt includes a redacted excerpt for each job,
windowed from 20 lines before its first recognised failure (or its last lines
when no failure is recognised), newest run first, with 16 KiB total. If no
cached log matches that head, the prompt lists the failing-check links as
before.

Each PR-agent run holds a durable per-PR lease, so the PR queue and CI autofix cannot run two agents on the same PR at once; each admitted run spends exactly one attempt. A second dispatcher is refused while the lease is live. If thegn exits mid-run, the lease lapses within five minutes; the next PR-queue or CI-autofix pass clears the stale claim, notes that the previous agent's outcome is unknown, and may dispatch again. The attempt that run consumed stays spent.

While a PR agent's lease is live, other refresh passes skip that PR entirely: no merge, re-run or status change happens under a running agent.

If thegn cannot confirm a running PR agent's lease before it expires, it stops
the agent and reports that the next refresh re-evaluates the row.
CI autofix reports the same lost-lease condition when it stops its PR agent.
CI autofix acts on the one active queue row for the failing worktree: merged or
closed rows for the same checkout are ignored, and when two or more active rows
share the worktree it holds with a notification naming them until the stale
ones are removed with `thegn pr queue rm`.
After an own-PR agent in the PR queue or CI autofix returns, thegn re-reads the
PR and checks its author, forge account, repository and head before recording a
push or updating the row. If the author, account or repository changed, the
head moved to anything other than the agent's own push, or any of them cannot
be verified, the row is held as `needs_human`.

## Review comment tasks

An explicitly queued row whose `watch` includes `"review"` gets one durable
task for each unresolved review thread. The task is derived from THE-27's
complete cached review snapshot, carries the thread anchor and comments in a
bounded prompt, and uses the currently configured `[pr_queue] agent` role and
`[pr_queue.prompts].review`. A later comment changes the revision and updates
the same thread-keyed task instead of creating a duplicate.

Tasks appear under their PR with role, status, and revision. Select a queued
task and press `h` (**handle**), or choose `pr-review-task-handle` / “PR review
task: handle queued” in the palette. Refresh only creates or revises roster
tasks; it never launches their agents automatically.

Handling runs the saved prompt off the event loop with the configured role.
An agent exit alone is not success: thegn verifies that the PR head moved from
the task's baseline, that the remote head exactly matches the task worktree's
local head, that the task revision did not change, and that the provider still
reports the thread unresolved. Only then may the forge provider post a bounded
audit reply and resolve the thread.

If the provider cannot resolve review threads, authentication is missing, the
request is stale or rate-limited, the push cannot be verified, or the task has
no worktree, the thread stays unresolved and the task waits for a human.
Queued/revised tasks, successful resolutions, and needs-human outcomes are
recorded in notifications; durable task creation/revision also emits the
bounded `pr.thread_unresolved` event. Review refresh shares the existing
`poll_interval_secs` cadence (60 seconds by default, floored at 15), so this
feature adds no second timer.

## Working on a team

These are on by default, and they are why this is safe to point at a
repo other people push to:

- **`pause_on_foreign_push`** — if the branch moved and thegn didn't
  move it, someone else is working there. The agent stops and asks for
  you rather than racing them. Its prompts require
  `git push --force-with-lease`, never a plain `--force`.
- **`own_prs_only`** — a PR you didn't author is watched and displayed
  but never written to. If thegn cannot tell who you are, it assumes the
  PR is not yours.
- **`reset_attempts_on_push`** — a new commit refills the attempt
  budget, because a pull request lives for days and a one-shot budget
  would exhaust once and stick forever. Only a commit thegn did _not_
  create counts: after an agent run, thegn records the run's own push when the
  worktree's HEAD descends from the head the agent started on and matches the PR
  head on the forge, so an agent cannot top up its own budget or trip
  `pause_on_foreign_push`. Anything it cannot verify (a remote worktree, a
  teammate's push) still counts as someone else's.

## Watching

The status bar shows a `PR` chip: red when a pull request needs you, amber
while thegn is working on one, dim when the queue is merely populated,
silent when empty. Merged / ready / needs-you transitions toast and land
in the notifications inbox.

In the section: `a` add · `x` remove · `r` re-watch a settled row after forge
verification ·
`h` handle a queued review task · `c` clear after a second press · `D` refresh
now · `o` open in a browser. CLI equivalents for queue rows are
`thegn pr queue add|rm|clear|rebind|drain|list|status`; `list`, `drain`, and
`status` take `--json`.
