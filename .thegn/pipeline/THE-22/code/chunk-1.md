# THE-22 chunk 1 — per-thread core, roster, and forge seam

## Dependency and ownership

Run only after the Lead merges THE-27 (`tg/the-27-pr-comments-in-diff`) into
this branch. Consume its review model/cache/formatter; do not copy or rename
THE-27 types. This chunk is file-disjoint from chunks 2 and 3. Chunk 2 depends
on the public types and persistence/forge APIs created here, so run it
serially after this commit. Chunk 3 is documentation-only but depends on the
final action and config names from chunk 2, so run it last.

## Files touched

- `crates/thegn-core/src/pr_review_tasks.rs` (new)
- `crates/thegn-core/src/db_review_tasks.rs` (new)
- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/issue.rs`
- `crates/thegn-core/src/db.rs`
- `crates/thegn-core/src/db_migrate.rs`
- `crates/thegn-core/src/notification.rs`
- `crates/thegn-core/src/db_notification.rs`
- `crates/thegn-core/src/forge/mod.rs`
- `crates/thegn-core/src/github.rs`
- `crates/thegn-svc/src/forge/mod.rs`

Do not touch host files, `pr_queue.rs`, config, help, or openspec files in this
chunk.

## Approach

1. Add a substrate-free reconciler over THE-27's `PrReviewSnapshot` and
   `ReviewThread` data. Use canonical `(forge, repository, PR, thread id)`
   source keys and deterministic bounded revisions. Produce one upsert per
   unresolved thread, update the same task on new comments, and produce no
   duplicate for unchanged input. Handle a non-empty aggregate requested-change
   body as one deterministic `review_decision` source only when no thread
   exists; otherwise leave an empty aggregate blocker human-visible.
2. Render the task with existing `TaskKind::PrReview` template validation and
   THE-27's bounded formatter. Keep all remote text bounded/sanitized. Emit the
   pure `pr.thread_unresolved` payload only on create/revision change; never do
   DB, forge, terminal, or process work in the module.
3. Extend `agent_dispatches` with nullable review metadata and durable resolve
   bookkeeping: task kind, source key/revision, prompt, expected head, action
   attempts, and next-action time. Add schema 63 fresh DDL, additive migration,
   unique partial dedupe index, typed CRUD in `db_review_tasks.rs`, and mapping
   coverage without changing existing pipeline rows.
4. Add `resolve_review_thread` to the object-safe forge trait/capability
   catalog and service ladder. GitHub's implementation belongs in
   `crates/thegn-core/src/github.rs` and may use its vendor GraphQL mutation;
   no host code may call it directly. Providers without the operation retain
   unsupported/reserved behavior and a false capability.
5. Add notification wire kinds and once-keyed storage for queued/revised review
   tasks and resolved threads. Keep notification messages bounded and free of
   credentials.

## Tests to run

Use only scoped checks:

- `just quick thegn-core`
- `cargo nextest run -p thegn-core pr_review_tasks`
- `cargo nextest run -p thegn-core db_migrate`
- `cargo nextest run -p thegn-core forge`
- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc forge`

The core tests must cover identical-thread no-op, new-comment same-row
revision, resolved transition, thread-vs-decision source identity, bounded
prompt/event text, deterministic digest, migration from v61, and unsupported
resolve capability. Do not invoke a built `thegn`, use the live state DB, run
e2e, or run `just test`/`just ci`.

## Done criteria

- THE-27 types are consumed through their public modules; core task derivation
  has no Tokio, terminal, HTTP, forge SDK, or SQLite dependency.
- A thread id maps to exactly one durable roster source key, and a changed
  comment updates its prompt/revision without a concurrent duplicate run.
- Schema 63 is additive/idempotent, preserves old dispatch rows, and its
  unique index enforces the dedupe invariant.
- The forge seam exposes `resolve_review_thread`; GitHub is the only concrete
  implementation in this chunk and unsupported providers remain reserved.
- Notifications and event payloads are bounded and tested.
- All scoped tests pass and ratchet checks show no new seam/catalog/ignored
  result violation.
- Commit exactly as:

  `feat(the-22): add per-thread review task substrate`
