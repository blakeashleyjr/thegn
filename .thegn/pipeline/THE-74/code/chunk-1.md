# THE-74 — chunk 1: roster time normalization + caps-ladder status glyphs

**Crate: `thegn-core` only.** Self-contained. No host file is touched.

## Dependencies / overlap

- Depends on nothing. **Start immediately.**
- File-disjoint from chunk 3 → the two may run **in parallel**.
- Chunk 2 depends on this one (it calls `glyph_set` and reads
  `GlyphSet::arrow_right`), so chunk 2 starts after this lands.

## Files touched (exact)

- `crates/thegn-core/src/db.rs`
- `crates/thegn-core/src/db_notification.rs`
- `crates/thegn-core/src/issue.rs`
- `crates/thegn-core/src/termcaps.rs`
- `crates/thegn-core/src/db_tests.rs`

## Background (verified, do not re-derive)

`put_agent_dispatch` (`db_notification.rs:299-320`) already inserts
`util::now_ms()`; its comment records that the old `util::now()` stored
**seconds**. It is the only writer of the column. The 28 legacy rows were never
migrated — confirmed on the live DB:

```
sqlite> select count(*) from agent_dispatches
        where dispatched_at_ms > 0 and dispatched_at_ms < 100000000000;
28
```

Those rows render as ~20 671 days old, because `db_notification.rs:463` reads
the column raw and `monitor_pipeline.rs:233` subtracts it from `now_ms`.

## Work

### 1a. Pure read-side guard in `issue.rs`

Add, next to `AgentDispatch`:

```rust
/// Unix-epoch cutoff separating a SECONDS stamp from a MILLISECONDS one.
/// As milliseconds this is 1973-03-03; as seconds it is the year 5138 — so
/// anything below it is unambiguously seconds for any timestamp this program
/// can legitimately hold.
pub const MS_EPOCH_FLOOR: i64 = 100_000_000_000;

/// Coerce a `dispatched_at_ms` value to milliseconds.
///
/// Rows written before the `util::now()` → `util::now_ms()` fix stored
/// SECONDS, which rendered as an age of ~20 671 days. The DB migration at
/// schema v58 rewrites those in place; this guard is the second defence, for
/// values that never pass through it — a roster deserialized over the control
/// API, a DB a newer build wrote, a hand-edited row.
///
/// `<= 0` is passed through unchanged: an unstamped row must read as
/// unstamped, never as 1970 multiplied by a thousand.
pub fn normalize_dispatch_ms(v: i64) -> i64 { … }
```

Unit tests (in the existing `mod spec` in `issue.rs`): a seconds value scales,
a milliseconds value is untouched, `0` and a negative are untouched, the exact
boundary `MS_EPOCH_FLOOR` is treated as milliseconds and `MS_EPOCH_FLOOR - 1`
as seconds, and the function is **idempotent** (applying it twice equals
applying it once) for every case above — idempotence is what makes it safe to
run after the migration.

### 1b. Apply the guard at the single read seam

In `db_notification.rs`, the row mapper around `:451-465` (`DISPATCH_COLS` /
`r.get(4)?`) is the one place every roster read goes through. Wrap the column:

```rust
dispatched_at_ms: crate::issue::normalize_dispatch_ms(r.get(4)?),
```

Do **not** scatter the call at display sites.

### 1c. Migration at schema v58

- `db.rs:123` — `SCHEMA_VERSION` 57 → **58**.
- In the same block that carries the `ver < 46` and `ver < 57` cleanups
  (`db.rs:875-893`), and **before** the `if ver < SCHEMA_VERSION` stamp at
  `db.rs:899`, add:

```rust
// v58: one-time normalization of `agent_dispatches.dispatched_at_ms` rows
// written while `put_agent_dispatch` stored `util::now()` (SECONDS) into a
// column every reader treats as milliseconds — a fresh row rendered as ~20 671
// days old. The write side was fixed; these rows never were. Gated on the
// pre-bump on-disk version so it runs exactly once, and the predicate is
// idempotent anyway (a scaled row is above the floor). Best-effort: the DB is
// a cache, and a fresh DB matches zero rows.
if ver < 58 {
    let _ = conn.execute(
        "UPDATE agent_dispatches SET dispatched_at_ms = dispatched_at_ms * 1000 \
         WHERE dispatched_at_ms > 0 AND dispatched_at_ms < 100000000000",
        [],
    );
}
```

Keep the literal in the SQL (it cannot bind a Rust const) but reference
`issue::MS_EPOCH_FLOOR` in the comment so the two never drift.

`let _ =` is the sanctioned pattern here and matches the two neighbours; the
`// best-effort:` line above satisfies the ignored-`Result` ratchet.

### 1d. Caps-ladder status glyphs

`AgentDispatchStatus::glyph()` (`issue.rs:384-395`) stays — `thegn dispatch
list` prints it. Add a sibling that goes through the ladder, following the
established shape of `attention.rs:394` (`glyph(self, gl: &GlyphSet) ->
(&'static str, Hue)`) and `notification.rs:311` (`hued_glyph`):

```rust
pub fn glyph_set(
    self,
    gl: &crate::termcaps::GlyphSet,
) -> (&'static str, crate::theme::Hue) { … }
```

Mapping — every glyph is an **existing** `GlyphSet` field, and the
queued/spawning/running collision is resolved:

| status                 | field            | hue                                                         |
| ---------------------- | ---------------- | ----------------------------------------------------------- |
| `Queued`               | `diamond_hollow` | (dim — return the hue the board maps to `S::Dim`; see note) |
| `Spawning`             | `refresh`        | `Teal`                                                      |
| `Running`              | `dot_filled`     | `Teal`                                                      |
| `WaitingHuman`         | `attention`      | `Amber`                                                     |
| `PrOpen`               | `hex`            | `Blue`                                                      |
| `Merged` / `Done`      | `check`          | `Green`                                                     |
| `Abandoned` / `Failed` | `cross`          | `Red`                                                       |
| `Unknown`              | `diamond_hollow` | `Grey`-equivalent                                           |

Note on `Queued`/`Unknown`: `Hue` has no "dim" member — pick the closest
existing `Hue` used elsewhere for inert state and say so in a one-line comment;
the board (chunk 2) is free to override the tone for those two. The tones must
match `monitor/build.rs:984-994` `dispatch_tone` so the board, the sidebar and
the CLI never tell different stories.

Unit tests in `issue.rs`: every variant returns a non-empty glyph under both
`termcaps::UNICODE` and `termcaps::ASCII`; the ASCII result is 7-bit; and
`Queued`, `Spawning` and `Running` are pairwise **distinct** under `UNICODE`
(the collision this fixes).

### 1e. One new `GlyphSet` field

`termcaps.rs` — add `arrow_right` beside `arrow_up` / `arrow_down`
(`termcaps.rs:357-358`):

- `UNICODE`: `"\u{2192}"` (`→`)
- `ASCII`: `">"`

It must satisfy the existing `GlyphSet` invariants asserted in that file's
tests: BMP, display width 1, and 7-bit ASCII in the `ASCII` set. Run the
termcaps tests and fix anything that enumerates fields.

## Tests to run (scoped — no full-workspace gate)

```sh
just quick thegn-core
cargo nextest run -p thegn-core dispatch
cargo nextest run -p thegn-core termcaps
cargo nextest run -p thegn-core glyph
cargo nextest run -p thegn-core schema
```

Add a DB-level test in `db_tests.rs` (near
`dispatch_dispatched_at_ms_reads_latest_timestamp`, `:1961`) that inserts a
seconds-valued row **directly via SQL** (bypassing `put_agent_dispatch`, which
now writes ms) and asserts `list_dispatches` returns a millisecond value — this
covers 1b even on a DB that skipped the migration. Isolate `XDG_STATE_HOME`
using whatever the neighbouring tests in that file already do; never touch the
real DB.

## Done criteria

- `SCHEMA_VERSION == 58`; the `ver < 58` normalization sits with its
  siblings and above the version stamp.
- `normalize_dispatch_ms` is pure, idempotent, tested at the boundary, and
  applied **once**, in the `db_notification.rs` row mapper.
- `glyph_set` exists, is caps-driven, and `Queued`/`Spawning`/`Running` are
  visually distinct.
- `GlyphSet::arrow_right` exists in both ladders and passes the BMP/width-1/
  ASCII assertions.
- The scoped commands above are green. Coverage on `thegn-core` is gated at
  95 % lines in CI — every new function carries unit tests.
- **Do not** start `just test`, `just ci`, `just coverage`, or any
  full-workspace compile.

## Commit

Exactly one commit, this subject verbatim:

```
fix(pipeline): migrate legacy seconds dispatch stamps + caps-ladder status glyphs (THE-74)
```
