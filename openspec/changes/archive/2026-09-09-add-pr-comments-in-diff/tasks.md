# Tasks — PR review projection and thread handoff

## 1. Complete review snapshot and cache

- [x] 1.1 Add `PrReviewSnapshot` with worktree, branch, PR, head, timestamp,
      full conversation, and PR diff identity.
- [x] 1.2 Add the additive v63 `pr_review_cache` table plus typed cache-store
      reads/writes, identity rejection, migration preservation, shape, and
      idempotence tests.
- [x] 1.3 Extend off-loop refresh to write only complete snapshots, preserve the
      last complete cache on partial/transient failure, and deliver generation-safe
      status to views.

## 2. Pure review projection

- [x] 2.1 Add exact new-side `(path, line)` anchoring with per-file outdated and
      general buckets; cover hits, misses, deleted/missing anchors, multiple
      threads, and resolved filtering.
- [x] 2.2 Add bounded, data-delimited selected/all-unresolved feedback formatting
      with PR context, locations, diff hunks, comments, control stripping, and no
      final newline.
- [x] 2.3 Keep the projection free of renderer, database, async runtime, and
      vendor-client dependencies.

## 3. Presentation and navigation

- [x] 3.1 Interleave selectable thread rows in the PR Files tab, preserve
      outdated/general feedback, reuse the reply composer, and clamp cursor state.
- [x] 3.2 Add a full-screen **PR review** diff source backed by the PR-head diff
      while preserving **Worktree** as the default local/staging source.
- [x] 3.3 Add unresolved counts, `n`/`N` navigation, `v` resolved-history toggle,
      top-level comments/reviews, and honest loading/stale/unsupported labels.

## 4. Agent handoff

- [x] 4.1 Add `p` selected-thread and `P` all-unresolved handoffs to a recognized
      live agent pane scoped to the active worktree, with a bounded sanitized
      non-submitting paste and no trailing newline.
- [x] 4.2 Add repository-aware, confirm-gated headless `PrReview` dispatch through
      the existing off-loop runner, sandbox, and isolation-floor policy.
- [x] 4.3 Add explicit no-target/floor-miss/failure status and hostile paste,
      target-scope, config-overlay, confirmation, and dispatch regression tests.

## 5. Help and evidence

- [x] 5.1 Document PR-review source semantics, thread navigation/toggle,
      handoff keys, human-submit boundary, headless confirmation, and IDE fallback
      in `docs/help/review-a-pr.md`; pass the focused help/ratchet checks.
- [x] 5.2 Record focused core cache/migration/review and host PR-view/diff-view/
      handoff/actions/pane-writer test evidence plus clippy, formatting, and
      repository ratchets in the THE-27 pipeline artifacts.
- [x] 5.3 Reconcile the final delivered proposal/design/delta specs and pass
      strict OpenSpec validation without claiming an unrun live forge/model or full
      workspace/e2e exercise.
