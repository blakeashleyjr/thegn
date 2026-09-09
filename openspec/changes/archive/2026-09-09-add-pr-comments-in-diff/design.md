# Design — delivered PR review projection and handoff

## Complete identity-bearing snapshot

`thegn_core::review::PrReviewSnapshot` is the cache wire value for a worktree.
It carries the canonical worktree key, branch, PR number, head OID, fetch time,
full `PrConversation`, and `PrDiff`. Presentation accepts cached data only when
its worktree, branch, PR, and head identity match the active review.

The best-effort `pr_review_cache` table landed in additive schema v63. One
atomic row contains a complete snapshot. The off-loop refresh writes it only
after both conversation and PR diff succeed; a partial or transient forge
failure preserves the last complete row. The cache is not a source of truth and
an unsupported/unauthenticated provider is labeled rather than represented as
an authoritative empty review.

## Pure anchor and prompt projection

`thegn_core::review::anchor_threads` maps a thread only when its path and line
exactly match a rendered new-side diff line. Missing paths, deleted-side/missing
line anchors, renamed paths, and other misses remain visible in outdated or
general buckets; the projection never guesses a nearby line.

`visible_threads` supplies stable unresolved-first navigation with resolved
history included only when requested. `format_review_feedback` formats one
selected thread or all unresolved feedback, including PR identity, location,
diff hunk, and every comment. Remote bodies are bounded, delimited as data, and
stripped of C0/C1 controls except newline/tab. It returns no trailing newline.
The host paste builder independently neutralizes embedded bracketed-paste
markers.

## Two honest review surfaces

The PR view's Files tab and full-screen `DiffView` consume the same host
`review_rows` projection. Expanded files interleave exact threads beneath their
new-side lines and retain per-file outdated rows. General/missing-file feedback
stays visible without pretending to belong to an expanded file. Thread rows are
selectable for navigation, reply, IDE location, and handoff; selection and row
count use the same projection and clamp after refresh/toggle changes.

`DiffView` defaults to the local Worktree diff. `Tab` exposes an explicit
**PR review** source only when a matching snapshot exists. That source uses the
PR-head diff and includes top-level comments/reviews; stale, loading,
unsupported, and absent snapshots are labeled. This prevents remote anchors
from being attached to a drifting uncommitted patch.

The PR view uses `v` as a view-local resolved toggle and `n`/`N` for thread
navigation. File and panel review summaries expose unresolved counts before the
user expands feedback.

## Human-triggered handoff

`p` hands off the selected thread; `P` formats all unresolved feedback. Target
resolution is scoped to recognized live agent processes in the active
worktree's session group, never the merely focused pane or another worktree.
The live target receives one sanitized bracketed paste with no trailing newline,
is focused, and waits for the human to submit.

With no live target, repository-aware PR-queue config resolves a headless agent
or command. A confirmation menu is mandatory. Dispatch uses
`TaskKind::PrReview`, the active worktree, the existing off-loop runner and
waker, and the configured sandbox/isolation-floor gate. A floor miss becomes an
explicit hold; absence of both targets reports status and does nothing. The
handoff adds no forge write and carries the existing PR-task rules against
merge, approval, thread resolution, and non-lease force push.

## Help and validation

`docs/help/review-a-pr.md` documents Worktree versus PR-review sources,
`n`/`N`, `v`, `p`/`P`, non-submitting live paste, confirm-gated headless
fallback, and IDE anchor behavior. Focused core/host suites cover identity,
cache/migration, exact/outdated anchoring, prompt bounds and hostile input,
row/selection composition, confirmation, isolation-floor resolution, and paste
hardening.
