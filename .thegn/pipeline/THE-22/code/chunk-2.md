# THE-22 chunk 2 — watched refresh, handle action, and safe resolution

## Dependency and ownership

Run after THE-27 and chunk 1 have landed. This chunk consumes the core
reconciler, roster CRUD, notification kinds, and `resolve_review_thread` seam.
It is file-disjoint from chunks 1 and 3. Chunk 3 must wait for this chunk's
final action names and user-facing behavior before updating docs/ratchets.

## Files touched

- `crates/thegn-host/src/pr_driver.rs`
- `crates/thegn-host/src/handlers/pr_queue.rs`
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/review_task_handoff.rs` (new)
- `crates/thegn-host/src/panel/mod.rs`
- `crates/thegn-host/src/panel/sections/pr_queue.rs`
- `crates/thegn-host/src/keymap.rs`
- `crates/thegn-host/src/keymap_specs.rs`

Do not touch core, service, config, help, openspec, or ratchet files in this
chunk.

## Approach

1. Extend the existing off-loop PR-queue worker and refresh signals. Only rows
   explicitly watching `review` and satisfying the current queue eligibility
   are fetched. Reconcile THE-27's cached `PrReviewSnapshot` into the chunk-1
   roster. Keep stale tasks unchanged on transient forge errors; wake the UI
   through the existing waker. Add no timer or event-loop blocking operation.
2. Prevent the current aggregate `ChangesRequested -> PrReview` path from
   launching a duplicate PR-wide run when review tasks are present. Preserve
   CI/conflict/merge decisions and preserve a human-visible aggregate state
   when the provider supplies no actionable body/thread.
3. Add a small host handoff module rather than growing `pr_driver.rs` into a
   god file. The panel/palette `handle` action selects one queued review task,
   marks it running durably, resolves the configured role using existing
   `agent_task::resolve_agent`, and runs the existing off-loop agent runner
   with the stored prompt. Missing configuration becomes `waiting_human` plus
   a notification; it must not silently fall back to another role.
4. After the agent exits, treat exit status as advisory. Refresh the PR and
   compare its head to the task's recorded `expected_head_oid`. Only an exact
   task-caused head movement may call the forge seam. Concurrent/foreign head
   changes pause for a human. Check unresolved state and durable cooldown
   before `resolve_review_thread`; on unsupported, auth, conflict, or rate
   limit, preserve unresolved state, record backoff, and notify.
5. Render task rows with PR/thread id, exact anchor or PR-level marker, role,
   queued/running/waiting state, and latest revision. Add a TUI-only palette and
   panel action. Do not add CLI/MCP/gRPC/control routes or a new catalog entry.

The refresh worker must emit the chunk-1 `pr.thread_unresolved` event after
durable upsert, so a future THE-21 consumer can observe the same deduped task.
It must not implement THE-21 itself or dispatch twice when an automation
consumer is present.

## Tests to run

Use only scoped checks:

- `just quick thegn-host`
- `cargo nextest run -p thegn-host pr_driver`
- `cargo nextest run -p thegn-host review_task_handoff`
- `cargo nextest run -p thegn-host pr_queue`
- `cargo nextest run -p thegn-host keymap`

Cover explicit-watch filtering, stale-on-error behavior, no duplicate generic
review run, handle-without-agent, queued-to-running transitions, foreign-push
pause, exact expected-head verification, cooldown/rate-limit behavior, and
unsupported resolve handling. Do not invoke the built binary, touch the live
state DB, run e2e, or run `just test`/`just ci`.

## Done criteria

- Review refresh piggybacks the existing PR queue cadence and only processes
  explicit watched PR rows; idle has no new timer and the terminal loop remains
  nonblocking.
- The generic aggregate PR review dispatch cannot duplicate a per-thread task.
- Panel and palette both expose the same TUI-only `handle` action, with durable
  status and clear human fallback.
- A thread is replied/resolved only through `resolve_review_thread` after a
  verified task head change, unresolved recheck, and durable cooldown check.
- Unsupported providers, foreign pushes, and transient failures degrade safely
  and remain auditable.
- All scoped tests pass with no new help/catalog/control/ignored-result ratchet
  failure introduced by the implementation wiring.
- Commit exactly as:

  `feat(the-22): reconcile and handle watched review tasks`
