# THE-87 · Chunk 1 — `thegn-core`: open fast path tolerates a newer schema; warn once

Issue: https://linear.app/blakeashley/issue/THE-87 · Design: `.thegn/pipeline/THE-87/architect/design.md` §1 · HEAD `a65b42a3` (citations against this HEAD)

**Crate:** `thegn-core`. **Parallelizable:** yes — file-disjoint from chunks 2
and 3 (`thegn-host` only there); no logical dependency on either.

## Files touched (exact paths)

| Path                                  | Change                                                                                                                                                                                                                                                                                                                                                                                            |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/db.rs`         | `open_mode` (`:250-255`) becomes `on_disk >= current → Fast`; the fast-path early return (`:337-343`) sets `schema_mismatch: detect_newer_schema(ver, SCHEMA_VERSION)` instead of `None`; the mismatch warn is emitted HERE (single site) behind `static WARNED: std::sync::Once`; update the doc comments at `:237-243` and the fast-path safety comment at `:329-336` to state the new contract |
| `crates/thegn-core/src/db_migrate.rs` | Delete the `tracing::warn!` from `detect_newer_schema` (`:29-31`) — the classifier stays pure (`Option<i64>` return unchanged). Update its doc comment (`:17-22`)                                                                                                                                                                                                                                 |
| `crates/thegn-core/src/db_tests.rs`   | Rename + update the exhaustive test `open_mode_is_fast_only_on_exact_version_match` (`:3017-3032`); add `newer_db_takes_the_fast_path_and_still_serves_reads_writes`                                                                                                                                                                                                                              |

Nothing else. No host-crate change, no migration, no config.

## Approach

1. **`open_mode`**: `if on_disk >= current { OpenMode::Fast } else { OpenMode::Full }`.
   - `on_disk < current` MUST stay `Full` — a migration is genuinely due.
   - Safety argument for tolerating newer (also write it into the `:237-243`
     doc): `user_version` is stamped only after a full init completes
     (`db.rs:920-928`), so `on_disk >= current` proves the schema batch ran;
     the schema is additive by construction (`IF NOT EXISTS` DDL + idempotent
     ALTER probes; migrations only add tables/columns or drop-and-recreate
     _cache_ tables under the same names), so an older binary's named-column
     reads/writes are unaffected by columns it doesn't know.
2. **Fast path keeps the mismatch flag**: at `db.rs:337-343` replace
   `schema_mismatch: None` with `schema_mismatch:
detect_newer_schema(ver, SCHEMA_VERSION)`. Exact match ⇒ `None` (no
   behaviour change for the common path); newer ⇒ `Some(ver)`, so the
   existing once-at-startup status (`thegn-host` `handlers/startup.rs:254`,
   consumed at `run.rs:758`) keeps working unchanged.
3. **Warn once per process**: at the `db.rs::init` site,
   ```rust
   if schema_mismatch.is_some() {
       static MISMATCH_WARNED: std::sync::Once = std::sync::Once::new();
       MISMATCH_WARNED.call_once(|| tracing::warn!(
           target: "thegn::db",
           on_disk = ver, build = SCHEMA_VERSION,
           "database schema v{ver} is newer than this build (v{SCHEMA_VERSION}); \
            data written by the newer build may be invisible"
       ));
   }
   ```
   Same target and message text as the deleted `db_migrate.rs` warn. This is
   the fix for ~74k duplicate warns: `Db::open()` has 356 host call sites and
   runs per hydration/prefetch/action.
4. Keep the `PRUNE_ONCE` logic in `Db::open()` (`db.rs:266-291`) untouched —
   the prune is per-process already and orthogonal to the open mode.

## Tests (scoped)

```
just quick thegn-core
cargo nextest run -p thegn-core open_mode
cargo nextest run -p thegn-core newer_db
cargo nextest run -p thegn-core detect_newer_schema
cargo nextest run -p thegn-core fast_reopen
```

- `open_mode_is_fast_on_current_or_newer_schema` (rename of
  `:3017`): `0 → Full`, `SCHEMA_VERSION - 1 → Full`, `SCHEMA_VERSION → Fast`,
  `SCHEMA_VERSION + 1 → Fast`.
- `newer_db_takes_the_fast_path_and_still_serves_reads_writes`: `Db::open_at`
  a scratch file (existing test patterns set scratch dirs + cleanup), write a
  row, bump `PRAGMA user_version` to `SCHEMA_VERSION + 1` on the connection,
  reopen **at least twice**: reads/writes keep working,
  `db.schema_mismatch() == Some(SCHEMA_VERSION + 1)` on every open. Optionally
  assert the `thegn::db` "newer than this build" line appears **at most once**
  in `thegn_core::diagnostics::ring_snapshot()` (the always-on WARN+ ring
  makes this order-independent) — if the harness proves the ring unpopulated
  in unit tests, keep the mismatch round-trip assertions and leave the
  warn-once property review-verified.
- `detect_newer_schema_flags_only_a_newer_db` (`db_migrate.rs:632-637`) must
  pass UNCHANGED (return values are identical; only the log moved).

## Done-criteria

- `just quick thegn-core` clean; the scoped nextest filters above green.
- `grep -n "tracing::warn" crates/thegn-core/src/db_migrate.rs` returns only
  non-schema-mismatch hits (the per-open warn is gone from there).
- The exhaustive `open_mode` test covers all four regions including
  `current + 1`.
- No new ignored `Result`s; no new dependencies; `db.rs` doc comments state
  the `>=` contract (no stale "Anything else … is Full" prose left behind).

**Commit subject (exact):** `fix(db): fast-path opens tolerate a newer on-disk schema; warn once`
