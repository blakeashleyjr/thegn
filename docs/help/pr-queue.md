---
id: pr-queue
title: PR queue
parent: workflows
order: 3
contexts: [panel:prq]
actions: [open-pr-queue, pr-queue-add, pr-queue-refresh]
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

Queueing is explicit by default. Set `auto_enqueue = "worktree"` if you
would rather every PR you open be watched.

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
`require_approval` is set.

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
close, or approve — and, for review feedback, never resolve the threads,
because that is the reviewer's call. Override them per kind under
`[pr_queue.prompts]`; keep those rules if you do.

Before waking an agent on a red build thegn re-runs the failed checks
once, since plenty of red CI is a flake.

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
  create counts, so an agent cannot top up its own budget.

## Watching

The status bar shows a `PR` chip: red when a pull request needs you, amber
while thegn is working on one, dim when the queue is merely populated,
silent when empty. Merged / ready / needs-you transitions toast and land
in the notifications inbox.

In the section: `a` add · `x` remove · `r` re-watch a settled row ·
`c` clear · `D` refresh now · `o` open in a browser. CLI equivalents are
`thegn pr queue add|rm|clear|drain|list|status`, all with `--json`.
