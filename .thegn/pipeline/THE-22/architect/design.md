# THE-22 — Auto-handle unresolved review threads on watched PRs

## Decision

Implement review work as a durable, per-thread agent roster fed by the existing
explicit PR-watch rows. A watched PR is an existing `pr_queue` row whose
configured watch set includes `review`; it is never inferred from all visible
PRs and is never created by this change. Each unresolved review thread produces
one queued dispatch row, with the configured PR-queue agent role and a rendered
`agent_task` prompt containing bounded thread text, PR identity, and
`path:line` context. A new comment updates that row; it does not create a
second task.

The implementation depends on THE-27 landing first:

```
11f1f6a7 feat(the-27): add review row and handoff seams
90187ba1 feat(the-27): add review projection types
3c51f299 feat(the-27): add cached review anchoring substrate
d17e80a9 docs(the-27): architect design + chunk specs
```

In particular, THE-22 consumes THE-27's `ReviewThread`, `PrConversation`,
`PrReviewSnapshot`, anchoring rules, bounded formatter, and cached off-loop
refresh. The Lead must merge `tg/the-27-pr-comments-in-diff` before coding
these chunks. No THE-22 chunk may recreate those types or fetch review data
through a vendor SDK.

## Current-code and draft verification

The current queue is already an explicit opt-in registry. `pr_queue` rows are
persisted by the queue-add path (`crates/thegn-host/src/cmd/pr_queue.rs:127-151`)
and the queue's configured watch kinds are documented in
`config/config.toml.example:3990-4040`. The current queue classifier only
receives PR status facts and maps `ChangesRequested` to the existing aggregate
`PrReview` task (`crates/thegn-core/src/pr_queue.rs:39-73,
184-223`). Its driver consequently renders one generic PR-wide prompt
(`crates/thegn-host/src/pr_driver.rs:553-633`), which would duplicate the
required thread tasks. THE-22 must route the review portion through the
reconciler while preserving CI/conflict/merge handling.

THE-27 has already supplied the missing review substrate on its branch: the
pure anchoring and bounded feedback formatter are in
`crates/thegn-core/src/review.rs:54-209`, and the full conversation/diff model
is in `crates/thegn-core/src/forge/model.rs:469-553`. It also keeps provider
operations behind the forge seam (`crates/thegn-core/src/forge/mod.rs:285-370`)
and fetches review data off-loop as described by its architect design. Those
claims are therefore satisfied by THE-27 after it lands; this change must
reuse them.

The openspec draft was read in full. Its useful parts are the explicit watch
source, off-loop refresh, bounded review prompt, and dedupe intent. The
following draft claims are pruned because they do not satisfy this issue or
the current architecture:

| Draft claim                                                                                    | Final decision                                                                                                                                                                                                                                                  |
| ---------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Add `Blocker::UnresolvedComments`, `review_trigger`, and a PR-level fingerprint to `pr_queue`. | Cut. The queue row is the watch registry; task identity is `(forge, repo, PR, thread id)`, and revision is the thread content revision. Adding a blocker would duplicate the existing aggregate `ChangesRequested` path and cannot express one task per thread. |
| Add per-entry agent/command and task data to `pr_queue`.                                       | Cut. Reuse the existing configured `[pr_queue].agent`/`agent_command`, and persist task prompt/revision/source on the dispatch roster. Do not make the watch row a task table.                                                                                  |
| Dispatch one generic `PrReview` task per PR.                                                   | Cut. Existing `TaskKind::PrReview` remains the prompt renderer, but its generic direct dispatch is suppressed when review threads are reconciled.                                                                                                               |
| Reply but never resolve a thread.                                                              | Cut. The issue requires a forge seam operation that replies and resolves after a verified agent push. Unsupported providers remain reserved/human-visible.                                                                                                      |
| No per-thread lifecycle or notification audit.                                                 | Cut. The roster needs durable source identity, revision, lifecycle, and rate-limit state; queue and resolution events go to the notifications feed.                                                                                                             |

The draft's state-db claims are consequently replaced by the additive roster
schema below. Existing draft prose/specs must be aligned by the documentation
chunk before implementation is considered complete.

## Core contract

Add a substrate-free `pr_review_tasks` module. It accepts a review snapshot,
the PR context, configured role/template inputs, and existing task records, and
returns pure upsert/transition decisions plus an optional automation event.
It performs no SQLite, Tokio, terminal, forge, or process work.

For every unresolved `ReviewThread`:

1. Canonical identity is `forge + repository + pull-request number + thread id`.
   A provider thread id is opaque data and is never parsed as a GitHub id.
2. Canonical revision is a digest of the thread id, resolved state, anchor,
   diff hunk, comment ids/authors/timestamps/bodies, and the PR head OID. The
   canonical serialization is deterministic and length-bounded before hashing.
3. The task prompt is rendered with the existing `TaskKind::PrReview` variable
   validation and template machinery (`crates/thegn-core/src/agent_task.rs:60-113,
288-347`). The thread-specific feedback comes from THE-27's bounded,
   sanitized formatter; it includes PR URL/title/branch/head, thread text, and
   exact `path:line` when the anchor is current. Outdated/general comments say
   so rather than guessing a nearby line.
4. An unchanged revision is a no-op. A changed revision updates the same
   roster row and prompt. If it is queued, it remains queued; if running, the
   new revision is recorded as pending and is not dispatched concurrently.
5. A resolved thread is not dispatched. Its existing task is transitioned to a
   terminal done/resolved state with an audit notification. A missing or
   provider-unsupported thread id is not guessed or merged with another task.

`PrReview` remains the task kind to avoid a parallel prompt system. The
configured `[pr_queue].agent` is the role stored on the dispatch row, and
`agent_task::resolve_agent` retains its existing precedence for an explicit
command versus configured agents (`crates/thegn-core/src/agent_task.rs:573-593`).

Requested changes with actual unresolved threads follow the per-thread path.
If a provider reports `CHANGES_REQUESTED` with no thread but does provide a
non-empty review body, create one deterministic PR-level review-decision task
whose source kind is `review_decision`; its prompt says `PR-level` for the
location and includes the bounded review body. If neither thread text nor a
review body exists, retain the existing aggregate blocker and notify a human—
inventing a task with no actionable content would be misleading. This keeps
the requested-changes signal actionable without pretending it is a thread.

The core event shape is:

```
event = "pr.thread_unresolved"
source_key, source_revision, forge, repository, pr_number, pr_url,
pr_title, branch, base, head_oid, thread_id, path, line, role, prompt
```

It is emitted only for a newly created or newly revised unresolved-thread task,
with bounded remote text and no credentials. THE-21 may consume this event for
automation when it lands; THE-22 does not implement an automation engine,
second timer, or new control protocol. A future consumer must still honor the
durable roster dedupe and rate-limit state.

## Persistence and roster

Extend the existing `agent_dispatches` roster with nullable review-task
metadata rather than adding columns to `pr_queue`:

- `task_kind` (nullable wire value; existing pipeline rows remain null),
- `source_key` (nullable canonical thread/decision identity),
- `source_revision` (nullable digest),
- `prompt` (nullable rendered prompt),
- `forge_action_attempts`, `next_forge_action_at_ms`, and
  `expected_head_oid` for durable resolve throttling and push verification.

Add a unique partial index over `(task_kind, source_key)` for non-null review
rows. Add schema version 63 after THE-27's version 62, with fresh-schema DDL,
an additive migration, and migration tests. Do not rewrite or invalidate
existing dispatch rows. Keep the new CRUD in a sibling
`db_review_tasks.rs`; only schema and dispatch mapping belong in the existing
DB modules. SQLite remains cache/coordination state, while the forge remains
the source of truth for review resolution.

The pure reconciler decides `queued`, `running`, `done`, `waiting_human`, and
`failed` transitions using existing `AgentDispatchStatus` values
(`crates/thegn-core/src/issue.rs:363-391`). A transient fetch failure leaves
the prior roster untouched. A new revision never erases a prompt that is
currently running; it creates a pending revision for the next safe attempt.

## Refresh, dispatch, and resolve lifecycle

Use the existing PR-queue refresh cadence and remote-ref wakeups:
`hydrate.rs:640-705` emits the configured queue refresh, and
`run.rs:10653-10701`/`handlers/pr_queue.rs:98-149` already execute queue work
off the terminal loop. Extend that worker to fetch THE-27's review snapshot
only for explicit watched rows, reconcile pure deltas, persist the roster, and
publish notifications/events. There is no idle timer. The loop remains
nonblocking; all forge, DB, hashing, template rendering, and agent work stays
off-loop, with `TerminalWaker` used for UI changes.

The existing direct `ChangesRequested -> run_agent(PrReview)` route must be
gated so it cannot create a duplicate PR-wide review run. CI/conflict and other
non-review blockers retain their current decisions. If the provider only
offers the aggregate requested-change case described above, the reconciler
owns that single decision task.

The panel shows the queued review tasks alongside the watched PR row, including
PR/thread location, role, revision/update state, and a human-needed reason.
The `handle` action is a TUI-only panel/palette action. It loads one queued
task, marks it running in an off-loop worker, and invokes the existing generic
agent runner with the durable rendered prompt. It must not add a public CLI,
MCP, gRPC, or control route. If no role/command resolves, mark
`waiting_human` and notify rather than silently using another agent.

After the agent exits, its exit status is advisory, as documented by
`crates/thegn-core/src/agent_run.rs:97-101`. Refresh the PR and compare the
observed head with the task's `expected_head_oid`. Only an exact head change
from the task's baseline is eligible for automatic resolution; an unrelated or
concurrent push pauses the task for a human. This check must not weaken the
existing foreign-push safety behavior.

Add one object-safe forge operation:

```
resolve_review_thread(loc, thread_id, bounded_reply) -> Result<(), ForgeError>
```

The operation is the provider seam's single semantic action: provider
implementations may use a vendor-specific combined mutation, but the host
never calls a vendor or sequences reply/resolve itself. Add the capability bit,
service-ladder forwarding, and GitHub implementation in the forge/vendor
files only. Providers without it retain the default unsupported/reserved
behavior and advertise no capability. Resolve only when the thread is still
unresolved, the verified head moved, and the durable per-thread/PR cooldown
allows it. The bounded reply identifies the task and head; it must not include
untrusted secrets or an unbounded agent transcript. On rate limit, auth
failure, conflict, or unsupported operation, preserve the unresolved thread,
record the attempt/backoff, move to `waiting_human`, and wake the UI.

Use dedicated notification kinds for `pr_review_task_queued` (including a
revision update) and `pr_review_thread_resolved`. Persist them with the
existing notification store's once semantics (`db_notification.rs:13-60`),
including PR/thread/path:line/head context but not remote credentials. The
resolution notification is the audit record even when the provider reports
success asynchronously.

## Configuration, help, catalog, and ratchets

No new runtime config key is needed. Reuse and clarify the existing
`[pr_queue]` keys: `enabled`, explicit queue rows, `watch` containing
`review`, `agent`/`agent_command`, and the existing prompt setting. Update
`config/config.toml.example:3990-4074` to describe per-thread prompts,
dedupe, the verified-push resolve behavior, and unsupported-provider fallback.
Do not add `review_trigger`, a second interval, or an auto-watch default;
therefore the env-overlay ratchet needs no new key. Any implementation that
adds a key must update the example, config overlay, env-overlay ratchet, and
help in the same chunk.

Add the TUI handle action to the existing keymap/action registry and
`docs/help/pr-queue.md`; update only the corresponding help prose/context
ratchets. Because handle is an internal TUI action, do not add a capability
catalog row, control-schema entry, completion entry, or snapshot. If a coder
chooses to expose it externally, that is a design violation unless the full
catalog and all control/completion snapshots are updated in that same chunk.

The implementation must preserve the architecture ratchets: no forge imports
in core task derivation, no vendor calls outside forge implementation files,
no new timer, no blocking event-loop work, no new god module, and no ignored
results. Use new sibling modules for task derivation, persistence, and the
host lifecycle. Tests must include pure dedupe/revision/resolution decisions,
prompt bounds, migration compatibility, unsupported forge behavior, and
rate-limit/push-verification transitions.

## Chunk order and integration

Chunks are intentionally serial at their dependency boundaries and touch
disjoint implementation surfaces. The Lead merges THE-27 first, then runs
chunk 1, chunk 2, and chunk 3 in order. Exact file ownership, scoped tests,
and commit subjects are in the chunk files. No chunk runs a full workspace
build, e2e test, migration against the live state DB, or the worktree's built
binary.
