# THE-22 — merge main (brings THE-27), then implement chunk 1 (Lead work order)

files:
  - .thegn/pipeline/THE-22/code/chunk-1.md   (the real spec — follow it)

## Why this exists

Row 357 correctly reported BLOCKED: chunk 1 hard-depends on THE-27's `review`
module and `PrReviewSnapshot` API, and the spec forbids copying or renaming
those types. It was right to refuse.

The dependency is now satisfiable: `tg/the-27-pr-comments-in-diff` **landed on
main** as `b3aff883`. It simply is not merged into this branch yet.

## Done criteria

1. `git merge main` first. This is the Lead-owned prerequisite the chunk spec
   was waiting on — you are authorized to perform it. Resolve conflicts by
   keeping BOTH sides for registry/module-list/help additions.
2. Confirm the substrate is actually present after the merge: the THE-27
   `review` module and `PrReviewSnapshot` must resolve. If they still do not,
   STOP and report BLOCKED again with what is missing — do not work around it.
3. Then implement **chunk 1 exactly as specified** in
   `.thegn/pipeline/THE-22/code/chunk-1.md`, consuming THE-27's real types.
   Never copy or re-declare them.
4. Write your completion summary to this row's `{artifact}` and commit it.
5. Scoped tests per the chunk spec are sufficient for this row; the full gate
   runs at the queue boundary in review.
