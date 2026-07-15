---
id: merge-queue
title: Merge queue
parent: workflows
order: 2
contexts: [panel:merge]
actions: [integrate, merge-drain, open-merge-queue]
---

# Merge queue

A **local** merge queue: finished branches queue up, and a fold-actor
lands them on `main` one by one — fold, gate, advance the ref — without
ever checking `main` out. Enable it with `[merge_queue] enabled = true`.

## Queueing

Add the current worktree's branch from the [[panel]]'s _merge_ section,
the sidebar row menu, or `thegn mq add` in the pane. The queue is
persisted; entries survive restarts.

## Landing

- **Integrate** (palette, or the section's key) drains the queue once:
  each clean branch is folded into `main` and gated before the ref
  advances; conflicted branches stay queued.
- **Drain (agent autopilot)** hands conflicts to a coding agent to
  resolve, then continues.
- `thegn integrate` does the same from any shell.

A running instance sitting on `main` fast-forwards its own tree when the
ref moves — no stale checkout.

## Across hosts

The queue is **anchored to the target repo** — the host where `main`
lives — because the fold happens inside that repo's object store. You can
still queue branches whose worktrees live on **other machines**: each
row's host is shown as an `@host` chip in the _merge_ section, and at
drain time the branch's tip is bundle-fetched into the target store
before it folds. A branch whose host is unreachable is **deferred** (with
the reason), never silently dropped, and retried on the next drain.

Run the drain **where the target repo lives**. If you invoke it from a
machine other than the target's host, thegn tells you which host to run
it on — the fold, gate, and ref-advance must be co-located with `main`.

## Watching

The masthead shows queue depth ([[bars]]); the _merge_ section lists
entries with their gate state. Failures surface as notifications.
