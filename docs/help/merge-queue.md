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

## Watching

The masthead shows queue depth ([[bars]]); the _merge_ section lists
entries with their gate state. Failures surface as notifications.
