# Design — durable review tasks on watched PRs

## Dependencies and ownership

THE-27 is a satisfied dependency. Its `PrReviewSnapshot`, provider
`ReviewThread` identities, complete-cache write rule, anchor data, and bounded
single-thread formatter are reused directly. THE-22 neither forks those types
nor performs a second cache migration.

The PR queue remains the owner of which PRs are watched. Only a durable queue
row in a repository whose resolved `[pr_queue].watch` contains `review` enters
this path. `auto_enqueue = "off"` remains the default, and review work rides the
existing `poll_interval_secs` worker/ticker slot. Disabled or unwatched queues
perform no deep review fetch.

## Snapshot-to-roster reconciliation

For each successful complete snapshot, the host supplies stable PR context and
the current queue role/review prompt to the substrate-free core reconciler:

1. Each unresolved thread with a non-empty opaque provider id receives a
   canonical source key over forge, repository, PR number, and thread id.
2. Its source revision is a bounded deterministic digest of the current head,
   anchor, resolved state, and bounded comment identities/content. New comments
   therefore change the revision without changing task identity.
3. An unseen source produces one queued roster upsert. A changed revision
   updates that same row and requeues terminal/human-parked work; a running row
   retains its running state so it cannot be launched twice.
4. An unchanged revision produces no write or event. A thread observed resolved
   transitions its existing row to done. A transient fetch failure never calls
   the reconciler, so absence cannot fabricate resolution.

When a provider supplies no thread objects, a non-empty latest
changes-requested review may create one PR-level fallback task. Real threads
supersede it. Because that fallback has no provider thread id, handling it ends
in human re-review rather than automatic resolution.

The prompt uses `TaskKind::PrReview` validation and THE-27's formatter, with the
current `[pr_queue].agent` role and resolved `[pr_queue.prompts].review` at
derivation time. Prompt and event fields are sanitized and bounded. The event
wire name is exactly `pr.thread_unresolved`; it is published only after the
atomic roster upsert, with durable once-keyed notification audit sharing the
same source identity and revision.

## Persistence

Review tasks are a nullable projection of the existing shared
`agent_dispatches` roster, not columns on `pr_queue`. Schema v64 (following
THE-27's v63) adds `task_kind`, `source_key`, `source_revision`, `prompt`,
`expected_head_oid`, `forge_action_attempts`, and
`next_forge_action_at_ms`. A partial unique index on `(task_kind, source_key)`
provides durable dedupe while ordinary pipeline dispatch rows keep NULL review
metadata and their existing projection.

The upsert is atomic and preserves one row/id across revisions. Forge action
attempts and the next retry time survive restarts. Successful create/revision,
successful thread resolution, and needs-human transitions use once-keyed inbox
notifications so retries do not flood the audit.

## Refresh and rendering

Conversation and diff I/O, cache writes, reconciliation persistence, and
notification writes stay on the existing blocking PR-queue worker. The worker
pulses the normal queue channel/waker once; there is no new wake source or idle
poll. Panel hydration joins roster tasks with THE-27's cached snapshot to render
thread id, anchor, role, status, and revision beneath the owning PR.

The aggregate `ChangesRequested -> PrReview` agent path is suppressed when
per-thread reconciliation owns review work. Polling creates/revises rows and
emits audit events, but never launches a per-thread agent. Empty provider
feedback and stale snapshots remain visible/parked rather than producing an
empty generic prompt.

## Explicit handle lifecycle

The TUI-only `pr-review-task-handle` action is available from the command
palette and as `h` on a queued task row. The event loop selects an id and spawns
the blocking worker; all DB, git, forge, sandbox, and agent work remains off the
loop.

The worker:

1. admits only a queued task, respects durable resolution cooldown, and changes
   it to running;
2. resolves exactly the task's configured role plus the current queue command,
   with no default-role fallback, then applies the existing agent sandbox floor;
3. verifies the provider head still equals the task's recorded baseline before
   launching the saved bounded prompt;
4. treats agent exit status as advisory, reloads the roster, and requeues rather
   than resolving if a refresh revised the task while it ran;
5. requires the provider head to have moved from the baseline and to exactly
   equal the task worktree's local HEAD, rejecting no-op or foreign/concurrent
   pushes;
6. refetches the conversation and confirms the same provider thread remains
   unresolved; and
7. calls the optional forge `resolve_review_thread` operation with a bounded
   audit reply, then marks the roster row done and records resolution.

If the thread was already resolved after the verified push, the roster can
finish without another mutation. Unsupported/not-configured/not-authenticated
providers, missing worktrees or roles, stale heads/revisions, and PR-level
fallbacks park for a human. Offline, rate-limited, and other transient forge
errors additionally persist bounded backoff; they never claim the provider
thread was resolved.

## Provider seam and external surfaces

`ForgeCaps::resolve_review_thread` advertises the optional internal provider
operation. The object-safe `Forge::resolve_review_thread` default returns
unsupported; GitHub implements a bounded reply plus resolve mutation, and the
service provider ladder forwards the capability and call. Capability discovery
therefore stays honest without exposing a new product capability.

The action is intentionally TUI-only. No CLI, completion, control schema,
capability catalog, gRPC, MCP, or plugin surface is added, and no config or
environment overlay key is introduced.

## Security and failure posture

Remote review text is untrusted input. It is formatted through THE-27's bounded
sanitizer, stored as a bounded prompt, and never executed as a command template.
The agent retains the queue's existing sandbox, timeout, own-PR, and
force-with-lease posture; it cannot satisfy the lifecycle merely by exiting.
Forge credentials remain in thegn's provider seam rather than the prompt.

Automatic reply/resolve is deliberately behind an explicit human gesture plus
verified local/remote head equality, unchanged source revision, a fresh
unresolved-thread recheck, provider capability, and durable cooldown. Every
ambiguous direction is fail-closed to `waiting_human` while leaving the remote
thread unresolved.

## Rejected alternatives

- **PR-wide `UnresolvedComments` blocker, `review_trigger`, and fingerprint.**
  These collapse independent threads, create a second attempt-budget mechanism,
  and make display classification control task identity. Thread-keyed roster
  reconciliation is the honest unit.
- **Automatically dispatch during polling.** It can race refresh revisions and
  a second aggregate review run. Polling only prepares durable work; `handle` is
  the single admission gesture.
- **Reply but never resolve.** That leaves completed durable tasks permanently
  actionable. Verified-push provider resolution supplies a real terminal state,
  with human fallback when it is unsafe or unsupported.
- **Per-entry agent override.** It would require new PR-queue schema, CLI/config
  semantics, and public projections. The final contract uses the queue's current
  configured role/prompt.
