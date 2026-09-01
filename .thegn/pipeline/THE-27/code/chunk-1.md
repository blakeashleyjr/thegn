# Chunk 1 — core review projection, forge seam, and cache

## Files touched

- `crates/thegn-core/src/review.rs` (new): `PrReviewSnapshot`, anchor result,
  bounded/sanitized thread-to-prompt projection, and unit tests.
- `crates/thegn-core/src/lib.rs`: register the substrate-free module.
- `crates/thegn-core/src/store/cache.rs`: add typed get/put review-cache methods.
- `crates/thegn-core/src/db_cache.rs`: SQLite implementation with best-effort
  read/write behavior.
- `crates/thegn-core/src/db.rs`: schema v62 documentation/base DDL and verifier
  call.
- `crates/thegn-core/src/db_migrate.rs`: idempotent `pr_review_cache` migration,
  schema verifier, pre-v62 and idempotence tests.

## Approach

Implement the pure anchor/prompt contract first. Match only exact PR diff
`path + new_lineno`; put deleted/missing anchors into explicit outdated buckets.
Bound remote text, strip terminal controls except newline/tab, delimit review
text as data, and guarantee no final newline. Reuse `TaskKind::PrReview`’s
existing `{threads}` contract; do not add a task kind or config key.

Add one atomic JSON cache row per canonical worktree key, carrying branch/PR/
head identity so stale data cannot silently attach to a different PR. Preserve
the existing best-effort cache doctrine and additive migration ladder. If a
provider lacks deep review data, keep its capability false and expose the
existing reserved/unavailable probe state rather than returning an empty
success.

## Overlap/dependency

No file overlap with chunk 2. Chunk 2 depends on this chunk’s public core types,
cache methods, and schema contract, so Lead runs chunk 1 first and then chunk 2.
The pure module itself is independently testable before any host work.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core review`
- `cargo nextest run -p thegn-core db_migrate`
- `cargo nextest run -p thegn-core forge::model`

Run the existing forge-leak, async-trait, and cache/migration ratchets scoped
by the normal repository test commands if this chunk changes their inputs. No
binary invocation, live DB migration, e2e, `just test`, or `just ci`.

## Done criteria

- Core anchor/prompt tests cover hit, miss, deleted-side, duplicate-line,
  resolved filtering, top-level feedback, hostile controls, bounds, and
  no-final-newline.
- The provider path remains `thegn_core::forge::Forge`; the existing
  `review_threads` optional op/caps and Forgejo/Gitea reserved kinds are
  verified rather than duplicated, with no vendor call in host.
- Schema v62 creates the cache additively, preserves old rows, verifies its
  shape, and passes migration/idempotence tests.
- No config, completion, control, or env-overlay surface is introduced; any
  ratchet made necessary by an actual new surface is updated here, not deferred.
- Commit exactly as: `feat(the-27): add cached review anchoring substrate`
