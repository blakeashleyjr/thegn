# Tasks — durable review tasks on watched PRs

Depends on the implemented PR queue and THE-27.

## 1. Satisfied review substrate (THE-27)

- [x] 1.1 Reuse THE-27's complete `PrReviewSnapshot`, provider thread ids,
      cached conversation/diff, anchor model, and bounded single-thread prompt
      formatter. Do not repeat those types or migrations.
- [x] 1.2 Preserve THE-27's complete-success cache rule so transient fetch
      failure leaves the last snapshot and durable tasks intact.

## 2. Pure per-thread derivation (thegn-core)

- [x] 2.1 Derive a canonical source key per forge/repository/PR/thread and a
      bounded deterministic source revision from the current thread snapshot.
- [x] 2.2 Reconcile one task per unresolved thread: insert once, revise the
      same row when comments change, preserve running admission, and transition
      resolved sources without a per-PR blocker or fingerprint.
- [x] 2.3 Render the current queue role/review prompt through `PrReview`, bound
      and sanitize prompt/event fields, and produce `pr.thread_unresolved` only
      for durable create/revision work.
- [x] 2.4 Cover dedupe, revision, resolution, PR-level fallback, identity,
      hostile/bounded input, and deterministic derivation with core unit tests.

## 3. Durable roster, audit, and forge seam (thegn-core / thegn-svc)

- [x] 3.1 Add nullable review metadata and forge retry bookkeeping to the
      shared `agent_dispatches` roster in additive schema v64, following
      THE-27's v63 cache schema; preserve ordinary dispatch rows.
- [x] 3.2 Add atomic partial-index upsert/dedupe, typed task CRUD, resolved
      transition, and durable forge-attempt cooldown operations with migration
      and CRUD coverage.
- [x] 3.3 Add once-keyed queued/revised/resolved notification audit and the
      optional object-safe `resolve_review_thread` provider operation,
      capability forwarding, unsupported default, and GitHub implementation.

## 4. Off-loop refresh and explicit handling (thegn-host)

- [x] 4.1 On the existing PR-queue cadence, deep-fetch and reconcile only
      explicit queue rows whose resolved `watch` contains `review`; update the
      THE-27 cache and emit each event only after durable upsert.
- [x] 4.2 Suppress the competing aggregate review agent when per-thread work
      owns the feedback; keep stale/empty-provider cases visible for a human.
- [x] 4.3 Hydrate and render per-thread rows beneath their PR with anchor, role,
      status, and revision; add TUI-only panel/palette action
      `pr-review-task-handle` (`h`).
- [x] 4.4 Run handle work off-loop with exact role/command resolution, saved
      prompt, sandbox floor, unchanged revision, verified local/remote push,
      fresh unresolved-thread recheck, optional provider reply/resolve, durable
      backoff, and fail-closed human fallback.

## 5. Contract, help, and scoped validation

- [x] 5.1 Align proposal/design/delta specs to explicit watched rows,
      per-thread lifecycle, verified-push resolution, notification audit, and
      no new external control/catalog/config surface.
- [x] 5.2 Document the existing cadence, current role/prompt, revision-in-place,
      `h`/palette handle action, bounded prompt, notification/event audit, and
      unsupported/rate-limited human fallback in the config example and PR
      queue help; keep unrelated snapshots unchanged.
- [x] 5.3 Run `just quick thegn-host`, the targeted host help tests, targeted
      core config tests, and strict validation of this OpenSpec change.
