# Turn unresolved threads on watched PRs into durable tasks

Linear: THE-22

## Why

The PR queue already polls explicitly queued pull requests, and THE-27 already
stores each successful review conversation and diff as one complete
`PrReviewSnapshot`. Unresolved review threads therefore have stable provider
identities, anchors, comments, and head revisions, but a watched PR still
presents them only as review context. There is no durable, independently
handleable task for each thread.

A PR-wide blocker is the wrong unit. Several threads can be open at once, a new
reply should revise only its thread's work, and transient provider failures must
not erase known tasks. Completion must also be stronger than an agent process
exiting: thegn must verify the pushed head and recheck the provider before it
can claim a thread was resolved.

## What Changes

- On the existing off-loop PR-queue cadence, an explicitly queued row whose
  `watch` includes `review` deep-fetches a complete THE-27 snapshot and passes
  it to a substrate-free core reconciler. There is no all-PR watcher, second
  timer, auto-watch default, or `review_trigger` key.
- The reconciler produces one durable roster task per unresolved provider
  thread. A canonical thread source key deduplicates the row; a bounded
  revision over that thread's current snapshot revises the same row when new
  comments arrive. It does not add a per-PR unresolved-comments blocker or
  fingerprint.
- Each task captures the current `[pr_queue] agent` role and resolved
  `[pr_queue.prompts].review` prompt, bounded and sanitized through the shared
  `PrReview` formatter. A successful durable create/revision emits the bounded
  `pr.thread_unresolved` event and a once-keyed notification audit.
- The PR-queue panel renders each task beneath its PR. Panel key `h` and the
  palette action `pr-review-task-handle` explicitly run the selected queued
  task; polling never starts a competing implicit review agent.
- After the agent run, thegn resolves a provider thread only if the original PR
  head was unchanged before launch, the remote head moved and exactly matches
  the task worktree's local head, the task revision stayed current, and the
  thread is still unresolved. It then posts a bounded audit reply and resolves
  through the optional forge `resolve_review_thread` operation.
- Unsupported, unauthenticated, stale, concurrent, offline, or rate-limited
  resolution remains unresolved and is durably parked for a human. Task
  creation/revision, successful resolution, and needs-human outcomes remain in
  the notification audit.

## Impact

- Roadmap: extends **Z 759** (PR queue, team mode), advances **Z 338** (PR
  notifications), and automates the per-thread handoff introduced by **T 262 /
  THE-27** without duplicating its review model or formatter.
- Specs: adds final `pr-queue` lifecycle requirements and additive `state-db`
  roster metadata. THE-27's snapshot/cache substrate is already satisfied and
  is consumed as-is.
- Core: pure `pr_review_tasks` derivation; nullable review-task metadata on the
  shared `agent_dispatches` roster; notification audit; optional object-safe
  forge resolution operation. Schema v64 follows THE-27's v63 cache schema.
- Host: the existing PR-queue worker reconciles snapshots off-loop; the panel
  and palette expose one TUI-only handle action; verified-push resolution runs
  on the blocking handoff worker.
- Config and external surfaces: no new config key, CLI verb, completion slot,
  control route/schema, gRPC/MCP/plugin call, or capability-catalog entry. The
  forge seam capability is internal provider capability discovery.

## Non-goals

- Watching every forge PR or enabling review handling by default.
- A PR-wide `UnresolvedComments` blocker, `review_trigger`, per-PR thread
  fingerprint, or attempt-budget refill based on that fingerprint.
- Automatically launching agents during polling. Review tasks require the
  panel/palette `handle` gesture.
- Acting on PR-level conversation comments or task-list checkboxes. A
  changes-requested review body can be represented as a human-visible fallback,
  but it has no provider thread to auto-resolve.
- Choosing an agent per PR or per thread. Reconciliation uses the current queue
  role/prompt; changing public configuration or CLI surface is outside this
  change.
