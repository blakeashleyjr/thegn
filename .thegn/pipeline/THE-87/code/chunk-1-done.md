# THE-87 · Chunk 1 — Done

**Commit:** `4eba4407` — `fix(db): fast-path opens tolerate a newer on-disk schema; warn once`

## Changes made

### `crates/thegn-core/src/db.rs`

- **`open_mode`** (`:250-255`): changed from `on_disk == current` to `on_disk >= current` for `Fast`. Newer schema now takes the fast path; only `on_disk < current` (fresh or genuinely stale) takes `Full`.
- **Doc comments updated** (`:237-243` + `:329-336`): both doc strings now state the `>=` contract and the safety argument (additive schema).
- **Fast-path early return** (`:337-343`): sets `schema_mismatch: detect_newer_schema(ver, SCHEMA_VERSION)` instead of `None`. On exact match ⇒ `None` (unchanged). On newer ⇒ `Some(ver)`.
- **Warn once**: `static MISMATCH_WARNED: std::sync::Once` guards a `tracing::warn!` with the same target + message as the removed `db_migrate.rs` warn.

### `crates/thegn-core/src/db_migrate.rs`

- **`detect_newer_schema`** (`:23-31`): removed `tracing::warn!` — the function is now a pure classifier returning `Option<i64>`. Doc comment updated to note the caller is responsible for warning.

### `crates/thegn-core/src/db_tests.rs`

- **Renamed**: `open_mode_is_fast_only_on_exact_version_match` → `open_mode_is_fast_on_current_or_newer_schema`. Now asserts `SCHEMA_VERSION + 1 → Fast` (was `Full`).
- **New test**: `newer_db_takes_the_fast_path_and_still_serves_reads_writes`. Creates a scratch DB, stamps it with `SCHEMA_VERSION + 1`, reopens twice: verifies reads/writes work and `schema_mismatch() == Some(SCHEMA_VERSION + 1)` on every open.

## Test results

All targeted tests pass:

```
PASS thegn-core db::tests::open_mode_is_fast_on_current_or_newer_schema
PASS thegn-core db::tests::newer_db_takes_the_fast_path_and_still_serves_reads_writes
PASS thegn-core db::tests::fast_reopen_round_trips_and_reports_no_mismatch
PASS thegn-core db_migrate::tests::detect_newer_schema_flags_only_a_newer_db
```

`just quick thegn-core` compiles clean.

`grep -n "tracing::warn" crates/thegn-core/src/db_migrate.rs` returns only the v6 migration failure warning (line 178) — the per-open `detect_newer_schema` warn is gone.

## Unverified

- **Warn-once diagnostics ring assertion**: The chunk spec suggested optionally asserting that the `thegn::db` "newer than this build" line appears at most once in `ring_snapshot()`. This was left out — `ring_snapshot()` uses `try_lock()` and may return empty under contention. The warn-once property is instead review-verifiable from the `std::sync::Once` guard at the single warn site in `db.rs`.
