## 1. Core: aggregation model

- [x] 1.1 Add `crates/thegn-core/src/aggregate.rs` and export from `lib.rs`.
- [x] 1.2 Define `ExcerptKind` (CiFailure/DirtyFile/ContentMatch, with a severity order), `Excerpt { worktree, worktree_label, kind, file, line, text, detail }`, `Aggregation`, and `AggRow` (Group{label,count} | Excerpt(usize)).
- [x] 1.3 `Aggregation::from_excerpts` sorts by `(worktree_label, kind severity, file, line, text)`; add `excerpts()/is_empty()/len()`.
- [x] 1.4 `rows()` interleaves per-worktree divider rows with excerpt-index rows; `jump_target(flat)` resolves an excerpt; `summary()` gives per-kind counts.
- [x] 1.5 Pure builders `ci_failure_excerpts`, `dirty_file_excerpts`, `content_match_excerpts`.

## 2. Core: unit tests (95% gate)

- [x] 2.1 Deterministic sort + grouping (same inputs → same order; groups in label order, failures first).
- [x] 2.2 `rows()` shape (divider then its excerpts; counts correct); `jump_target` returns the right worktree; empty aggregation empty.
- [x] 2.3 Builders: `ci_failure_excerpts` keeps only failing runs; content/dirty builders map fields correctly; `summary()` counts.

## 3. Host: panel section

- [x] 3.1 `panel/mod.rs`: add `Section::Across` (enum, `SECTION_ORDER`, label/key, `tab()=Work`) and `PanelData.across: Aggregation`.
- [x] 3.2 `panel/sections/mod.rs`: `mod across;` + dispatch arm.
- [x] 3.3 `panel/sections/across.rs`: `content(ctx)` renders `across.rows()` grouped (`worktree · file:line · text`), three view widths, empty state; carries `PanelHit::Row(Section::Across, i)`.
- [x] 3.4 Unit-test `content` with a synthetic `Aggregation` (rows render, empty state).

## 4. Host: population (off-loop)

- [x] 4.1 In `hydrate.rs`, a `spawn_blocking` producer reads `db.worktrees()` + `db.get_ci_cache(path)`, deserializes `Vec<CiRun>`, builds `ci_failure_excerpts`, and hands an `Aggregation` back over the model channel (waker-pulsed) into `model.panel.across`.
- [x] 4.2 Stale-safe via the existing hydration generation discipline; never blocks the loop.

## 5. Verification

- [x] 5.1 `cargo test -p thegn-core` green (new aggregate tests).
- [x] 5.2 `cargo test -p thegn-host` green (new across-section test); `cargo clippy` + `cargo fmt --check` clean; god-file ratchet OK (no growth in run.rs/chrome.rs/keymap.rs).
- [x] 5.3 Manual: with a worktree that has a failing CI cache row, the Across section lists it grouped by worktree; empty otherwise. NOTE: live CI population depends on the CI cache being warm.
