---
id: workflows
title: Workflows
order: 20
---

# Workflows

Task-oriented guides — the shortest path through a whole job, not a key
reference. Every guide assumes the [[getting-started]] mental model.

## The guides

- [[review-a-pr]] — from notification to approved, without leaving thegn
- [[merge-queue]] — queue branches locally and let the fold-actor land them
- [[pr-queue]] — shepherd pull requests on the forge, on a team
- [[sandboxing]] — run each worktree's process in a container

## The shape of a day

Most work in thegn follows the same arc, and each step has a page:

1. **Start something.** `Alt-w` opens a worktree on a new branch — a tab
   of its own, with its own terminals. See
   [[workspaces-and-worktrees]].
2. **Work in it.** Split panes, run tools, watch the diff build up in the
   [[panel]]. See [[terminal-and-panes]] and [[git-and-diffs]].
3. **Leave it running.** Detach, close the laptop, come back. The pane
   daemon keeps the work alive — see [[daemon-and-sessions]].
4. **Land it.** Queue the branch and let the fold-actor merge it, or
   `thegn land` it directly. See [[merge-queue]].
5. **Clean up.** Delete the worktree from the [[sidebar]]; the branch and
   the DB rows go with it.

You can run many of these at once — that is the point of worktrees. The
[[sidebar]]'s `attention` sort bubbles whichever one needs you.

## When something needs you

The status bar's `✋` chip and the notifications section surface failing
CI, PR conflicts, and review requests across every worktree, not just the
focused one. [[bars]] explains the chips; [[panel]] covers the
notification list.

## Automating it

Everything above has a CLI verb, so a script or an agent can drive it
headlessly — see [[cli]] and [[best-practices]].

### Issue autopilot

The opt-in `[autopilot]` policy turns a provider refresh result into one
durable claim, one existing agent-dispatch roster row, and one linked worktree.
Matching is limited to issues returned by the authenticated “assigned to me”
filter with the configured label and Todo status. The worker uses the existing
`TaskKind::Issue` prompt, commits in its worktree, and never pushes or opens a
PR. The host validates the clean, on-branch, commit-ahead result before the
host pushes a new branch and opens the PR.

`thegn autopilot status [--repo PATH] [--json]` is a read-only, bounded audit
view. Once opened, the existing PR queue owns review, CI, conflict, approval,
and merge policy. Only an observed queue transition to `merged` closes the
autopilot run; optional Done synchronization uses the configured tracker seam.
Failures remain parked for a human with a bounded reason and do not trigger a
second attempt automatically.
