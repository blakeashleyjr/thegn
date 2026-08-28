# THE-74 — security/test/bug review verdict

Reviewed 2026-08-27 by the security/test/bug lane, on
`tg/the-74-pipeline-board-v2` after the prior reviewer's merge of main
(`8da6f66e`, main tip `8dab5e9e` confirmed still an ancestor — no re-merge
needed) and their fix `cb34db84` (mirror-leaf pin_key), both re-reviewed here as
part of the branch. Full branch diff `main...HEAD` (~6.2k insertions, 38 files)
against `architect/design.md`, the three chunk specs + done reports (every
"Unverified" section read), and `architect-review/verdict.md`.

## Verdict

PASS

Ready for the merge queue (`thegn integrate` — not run here). Two small fixes
were applied by this review (commits below); neither changes behaviour.

## Lead's risk surface — all four checked

1. **DB migration (v58, seconds × 1000).** The `UPDATE` is gated on the
   pre-bump on-disk `ver < 58` and the version stamp lands LAST, so a crash
   mid-migration re-runs it — and the predicate (`> 0 AND < 100000000000`) is
   idempotent: a scaled row sits above the floor and is never touched again. A
   value at exactly 1e11 is a legitimate March-1973 millisecond stamp and the
   exclusive `<` leaves it alone, in step with `normalize_dispatch_ms`
   (`v < MS_EPOCH_FLOOR`, `<= 0` passes through, `saturating_mul`). Fresh DB ⇒
   empty table, zero rows matched; already-migrated DB ⇒ block skipped. The
   fixture test reads the column **raw** (proving the migration, not the read
   guard) and re-opens to prove no double-scale. **Fix applied:** the boundary
   itself had no SQL-level test (the literal in `db.rs` and the Rust const
   cannot share a definition) — `9bc47777` adds one, asserting a row at the
   floor is untouched while a row one below scales, and that the typed read
   agrees. Read-side guard verified on BOTH column read seams
   (`map_dispatch` + `dispatch_dispatched_at_ms`); the remaining raw SELECTs of
   the column are ORDER BY / id-lookup only and cannot render an age. Every
   roster source in the workspace is `list_dispatches`, so no display path can
   render decades.
2. **Pipeline board overlay.** Key and click handlers touch memory only; the
   roster sample is a one-shot `spawn_blocking` task (Background QoS, waker
   pulsed, send + wake both best-effort with logging) gated on
   `board.wants_dispatches()` — no timer, no thread, nothing while shut. The
   feed applies only on a real roster diff (`DispatchRoster` is `PartialEq`
   over stable data, no clock) and `render_plan.rs` pins both invariants with
   tests: a roster update is a bounded diff (worst case `sidebar`), and an open
   box takes the overlay rule ⇒ `Full`. Hit-tables are exact: in `Columns` each
   column cell is padded to exactly `col_w` cells (so `(x−inner.x)/col_w` maps
   1:1), in `Stacked` the hit map IS the drawn line list, and `relines()` runs
   after every cursor change. The cursor is a row **id** (survives resamples),
   all nav/wheel indices are clamp-bounded, empty boards/columns resolve to
   `Pending`, jumps that find no target leave a footer notice (no silent
   no-op), and unbound chords `Passthrough` (tested: `Alt b` toggles, `Ctrl-g`
   still locks). The board's own feed never blocks: `Db::open` is off-loop;
   `pipeline_target` reads the `FrameModel` only; activation rides the
   bounds-checked sidebar door.
3. **Sidebar lane folders.** The fold is pure, any-status, and deterministic
   (earliest-stamp order, key tie-break) — lanes survive a restart with no live
   sessions and vanish only when their roster rows are removed. No duplicates
   or orphans: each lane files under exactly one workspace (first resolvable
   worktree) or the tail `unfiled` group (never dropped); worktree leaves are
   deduped by path; a roster row referencing a removed worktree yields a faint
   leaf with `tab_target: None`, never an omission. Identity anchors are stable
   (keys derive from `issue_id`/path), and `cb34db84` gives each mirror its own
   lane-scoped path-qualified `pin_key` so menu re-anchor, double-click and
   cursor re-seek resolve the mirror — with pins/marks/drags still kind-gated
   away from mirrors (`is_markable`/`is_pinnable`/`drag_src_for` all verified).
   The mouse posture follows the folder precedent (single click selects,
   double/caret/Enter folds) — chunk 3's flagged deviation, resolved correctly.
   The sidebar now carries **no clock-dependent text** (the architect's
   c3eccb97 removed the volatile ages), so its e2e flap risk is gone.
4. **Caps ladder in ascii mode.** `AgentDispatchStatus::glyph_set` is total
   across both rungs (7-bit ASCII asserted), `GlyphSet::arrow_right` carries the
   ASCII `">"`, and the board's render test asserts body + rails + legend are
   7-bit under `UnicodeLevel::Ascii` with an identical hit map while the
   Unicode rung is asserted non-ASCII. No glyph/color literal was introduced
   (both ratchet tests pass with no allowlist edits).

## Fixes from this review

| commit     | subject                                                                                      |
| ---------- | -------------------------------------------------------------------------------------------- |
| `c037cbab` | fix(the-74): clippy field_reassign_with_default in two new sidebar tests (review)            |
| `9bc47777` | fix(the-74): v58 migration boundary test — a value at MS_EPOCH_FLOOR must not scale (review) |

`c037cbab` is load-bearing for the queue: `just lint` runs
`cargo clippy --workspace --all-targets -- -D warnings`, and the two new
sidebar tests tripped `field_reassign_with_default` — the pre-push/merge gate
would have gone red on landing.

## Checks run (scoped, per addendum)

- `just quick thegn-core`, `just quick thegn-host`, `just quick thegn-svc`
  (closes the chunk done-reports' "thegn-svc never typechecked" gap) — clean.
- `cargo clippy -p thegn-host --tests` — clean after `c037cbab`.
- `cargo nextest run -p thegn-core` filtered
  `dispatch|termcaps|glyph|schema|normalize|v57|v58|ladder|migrat|ratchet|literal`
  — 170 + 25 + 43 + 2 new, all pass.
- `cargo nextest run -p thegn-host` filtered
  `pipeline_board|sidebar_pipeline|sidebar|monitor|help|ratchet` — 381 + 77 +
  279 batches, all pass (help + prose + context ratchets included).
- No `just test` / `just ci` / `just coverage` / e2e, per the addendum.

## Notes (non-blocking)

- **Resize window.** After a SIGWINCH with the board open, `handle_click`
  resolves against the last-rebuilt `self.screen` while `render` clamps to the
  current one until the next rebuild (sample tick or key). Identical to the
  monitor's pre-existing pattern; heals within one sample interval, and a
  frozen board is by definition a static picture. Not a regression; noting for
  completeness.
- **Cosmetic:** a click in the box's right border pad clamps to the last column
  (`x >= inner.x + inner.cols` is not rejected, only `x < inner.x`). No
  security consequence — the box is modal to the mouse and the selection is a
  highlight.
- **e2e (frames changed; which snapshots).** No committed spec or baseline
  exercises the board or the monitor modal (the 45 baselines cover
  startup/chrome/sidebar/panels/themes; `06-panel-system` is the system panel,
  not the monitor). The sidebar gained rows only when roster rows exist, which
  the hermetic e2e env has none of — so no committed baseline is expected to
  flap from this lane. The repo's baselines are stale repo-wide (pre-existing
  debt per CLAUDE.md); when e2e is re-enabled, the board's relative ages are
  volatile chrome and must be pinned in `e2e_freeze` before any board baseline
  is recorded on either platform.
- Roster text (issue ids, worktree paths) renders through termwiz surface cells
  exactly as the sidebar already did before this lane — no new untrusted-text
  path (input source remains the local control API over the unix socket).
- Coverage (thegn-core 95%) not measured per the addendum; the core delta this
  review added is test-only.
