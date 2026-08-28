# Chunk 3 done — sidebar drag drops land on the nearest row, not nothing

**Commit:** `b4c740dd` `fix(the-67): sidebar drag drops land on the nearest row,
not nothing` (finished and committed by the finisher pass after the original
coder died mid-turn; the code was ~complete and uncommitted in the worktree,
verified, rustfmt-fixed, boundary test strengthened, and committed).

## What landed

- `crates/thegn-host/src/sidebar_view.rs` — `row_at_clamped` immediately after
  `row_at`: the row under `my` when there is one, else first-row-above /
  last-row-below via `first()`/`last()` (paint order ⇒ sorted by `y`), `None`
  for an empty slice. `row_at` is byte-for-byte unchanged. Four unit tests
  beside `row_at_maps_screen_row_into_row_bounds` using the same fixture:
  identical-to-`row_at` over painted rows, tail→last, above-first→first,
  empty→`None`. Doc comment marks it drag-only; the click paths keep the
  strict resolver.
- `crates/thegn-host/src/handlers/sidebar_mouse.rs` — `spot_at` now bounds the
  clamp by the sidebar rect (`my < rect.y` or `my >= rect.y + rect.rows` ⇒
  `Spot::Invalid`, the surface keeps its boundary) and otherwise resolves via
  `row_at_clamped`, `Invalid` only when no rows are painted at all. The clamp
  only _chooses_ a row — `spot_for_hover` still validates (cross-workspace,
  home's anchored slot, source row all stay `Invalid`), recorded in the doc
  comment. `on_left_press` / `on_right_press` still use strict `row_at`
  (sidebar_mouse.rs:205, :306). Three new tests: blank-tail sample ≡ last-row
  sample; one-past-the-bottom **and** one-above-the-top stay `Invalid`
  (shifted-rect fixture for the `my < rect.y` half); a clamped sample onto
  another workspace's row is still `Invalid`.
- Explicitly **not** done, per the chunk's "do NOT" list and design §2.1: no
  between-rows insertion band, no `Pressed → Dragging` threshold change, no
  `autoscroll_step` change.

## Verification (scoped per the dev-loop policy)

- `just quick thegn-host` — clean (clippy `-D warnings`).
- `cargo nextest run -p thegn-host row_at` — 5/5 pass (incl. the 4 new
  `row_at_clamped` tests and the strict regression test).
- `cargo nextest run -p thegn-host sidebar_mouse` — 22/22 pass (incl. the 3
  new spot tests and `pressed_becomes_dragging_only_after_leaving_the_row_band`).
- Pre-commit treefmt gate green (reformatted one `row_at_clamped` one-liner).

## Unverified

- No live TUI drag (e2e excluded per spec; behavior is exercised at the
  `spot_at` unit level only — the actual release→resolve path in `run.rs` is
  untouched by this chunk).
- The rect-boundary test covers both edges via the fixture geometry; the
  masthead/panel wander case is represented by "outside the rect", not by a
  composited frame.

## Architect review verification (post-merge, commit a9829c82)

- **Live TUI drag**: reviewed the release path — `spot_at` is the release
  resolver and its clamping + validation are unit-covered (22/22 sidebar_mouse
  tests on the review tree); the run.rs side of the gesture is untouched by
  this chunk, as claimed.
- **Rect-boundary fixture**: verified — the shifted-rect test exercises the
  `my < rect.y` half directly.
