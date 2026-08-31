# THE-22 architect revision 1 — make watched-review handling race-safe

## Why this revision exists

The implementation has the right overall architecture: pure derivation in
core, additive roster state, an object-safe forge operation, off-loop refresh
and handling, explicit TUI admission, bounded audit text, and no new public
control/config surface. Three correctness gaps remain in the end-to-end
lifecycle, however, and two of them can make the advertised safe path fail or
run in the wrong checkout.

## Required corrections

### 1. Never turn a by-number queue row into an executable worktree task

`refresh_review_tasks` currently falls back from `item.worktree == None` to
`repo_root` and persists that path as the task's `worktree_path`. The handle
worker therefore treats the canonical checkout as the PR's worktree and may
run the configured agent there. This contradicts the existing PR-queue safety
contract and the new help text: a PR queued by number has no worktree and must
remain human-only.

- Separate the read-only forge/cache location from the durable execution
  worktree identity. Using the repo root to query a by-number PR is acceptable;
  recording it as the agent worktree is not.
- Preserve a missing queue-row worktree as missing/empty in the review task,
  render it as human-needed, and guarantee that handle performs no sandbox,
  git, forge-write, or agent operation for it.
- Do not let a synthetic/empty cache key overwrite THE-27 review data for a
  real worktree or collide across multiple by-number PRs. It is acceptable to
  skip that cache write and hydrate identity from durable task/audit metadata,
  or to use another non-executable cache identity.
- Add a driver/handle regression test that queues a PR by number, derives its
  review task, and proves the repo root is never passed to the agent runner.

### 2. Freeze active task inputs and make refresh/handle interleavings safe

A changed snapshot for a `running` row currently retains the `running` status
but overwrites `source_revision`, `prompt`, `expected_head_oid`, role, and
cooldown state in place. Because the revision includes the PR head, the
agent's intended push can be observed by the normal refresh wakeup before the
handle worker finishes. That refresh changes the durable revision/baseline;
the worker then sees a revision mismatch and requeues instead of resolving the
thread. A real new review comment during the run has the same overwrite path,
so the active durable prompt is also lost.

- Do not overwrite the active revision, prompt, role, or expected-head
  baseline while a task is `spawning`/`running`.
- Preserve a newly observed revision durably as pending work (or use an
  equivalently crash-safe state model) without creating a second task or
  re-emitting it on every unchanged poll.
- On completion, distinguish the task's verified push from genuinely revised
  review feedback. The normal interleaving where refresh observes the exact
  agent-pushed head before post-run verification must still be eligible for
  resolution; a new comment/anchor change during the run must prevent stale
  resolution and safely promote/requeue the latest prompt.
- Keep the unique `(task_kind, source_key)` invariant, durable cooldown, and
  advisory agent-exit semantics intact.
- Add tests for both interleavings: (a) refresh sees the intended verified
  push before handle completes and resolution succeeds once, and (b) a new
  comment arrives while running, the active inputs remain intact, no stale
  resolve occurs, and exactly one latest revision becomes queued.

### 3. Publish the specified structured event, not only its notification

After a successful upsert, the worker logs the event and sends its payload to
the TUI, but the drain converts it to
`Event::NotificationReceived`. Subscribers never receive an event named
`pr.thread_unresolved` with the documented source/PR/thread/prompt payload.
The notification audit is useful but is a different contract.

- Add an internal typed/custom event-bus projection (or an equivalent existing
  internal event channel) whose wire name is exactly
  `pr.thread_unresolved` and whose payload is the bounded
  `ReviewTaskEvent`.
- Publish it only after the roster upsert succeeds. Keep the once-keyed
  `pr_review_task_queued` notification as a separate audit/attention event.
- Do not add a CLI, MCP, gRPC, control-schema, completion, or capability-catalog
  surface.
- Test that a create and a changed revision each publish once, an unchanged
  snapshot publishes nothing, and a failed durable upsert publishes nothing.

## Expected files

At minimum, revisit:

- `crates/thegn-core/src/pr_review_tasks.rs`
- `crates/thegn-core/src/db_review_tasks.rs`
- `crates/thegn-core/src/issue.rs`
- `crates/thegn-core/src/event_bus.rs`
- `crates/thegn-host/src/pr_driver.rs`
- `crates/thegn-host/src/review_task_handoff.rs`
- `crates/thegn-host/src/handlers/pr_queue.rs`

If the race-safe pending model needs additive roster columns, also update
`db.rs`/`db_migrate.rs`, fresh DDL, the migration ladder, and mapping tests in
the same commit. Update config/help/OpenSpec only if the repaired behavior
changes a user-visible claim; do not churn them otherwise.

## Verification

Run the scoped gates for the files actually changed:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core pr_review_tasks`
- `cargo nextest run -p thegn-core db_review_tasks`
- `cargo nextest run -p thegn-core db_migrate`
- `cargo nextest run -p thegn-core event_bus`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host pr_driver`
- `cargo nextest run -p thegn-host review_task_handoff`
- `cargo nextest run -p thegn-host pr_queue`
- `git diff --check`

Do not run a live-state migration, built binary, e2e suite, or full-workspace
gate for this revision.

## Done criteria

- A PR with no queue worktree cannot execute an agent in the repository root.
- An active task's durable inputs remain stable while later review state is
  retained without duplication or loss.
- The verified agent-push refresh race resolves normally, while a concurrent
  new review comment blocks stale resolution and queues one current task.
- Internal subscribers receive the bounded `pr.thread_unresolved` event after
  durability, independently of the notification audit.
- Scoped tests and architecture ratchets pass.
