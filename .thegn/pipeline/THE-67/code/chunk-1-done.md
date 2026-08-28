# Chunk 1 done — pure drag grab-band geometry + tab-chip gap hit

**Commit:** `71d6c96e` `feat(the-67): pure drag grab-band geometry + tab-chip gap hit`
(finished and committed by the finisher pass after the original coder died
mid-turn; the code was ~complete and uncommitted in the worktree, verified,
lint-fixed, and committed as-is apart from the deviation below).

## What landed

- `crates/thegn-host/src/drag_hit.rs` (new) — substrate-free column geometry:
  `SepSide {Sidebar, Panel}`, `sep_grab`, `sep_is_exact`, `sep_follow` with the
  exact signatures chunk 2 calls. Band takes its extra cell from the center
  column's outer frame cell, never from a sidebar/panel row. Saturating
  `sep_follow` holds the press offset for the whole drag. 8 unit tests
  including the degenerate 1-column-center overlap (sep_left=40, sep_right=42 ⇒
  column 41 in both bands; caller resolves sidebar-first) and saturation at
  column 0.
- `crates/thegn-host/src/main.rs` — `mod drag_hit;` immediately before
  `mod dragdrop;` in the alphabetical run.
- `crates/thegn-host/src/chrome.rs` — `strip_chip_end` helper factored out of
  `strip_chip_spans` (placement math byte-identical) and reused by
  `center_tab_hit`, whose hit span is now `[sx, sx + w + 1)` clamped at the
  chip-stop boundary — the inter-chip spacing column resolves to the chip on
  its left; env-cluster/pin cells stay out of reach. Doc comment records why.
- `crates/thegn-host/src/chrome_tests.rs` —
  `center_tab_hit_claims_the_gap_between_chips` (gap → `Some(0)`, next chip
  start → `Some(1)`, left-widening only: pre-gap column and column 0 stay
  `None`) and `center_tab_hit_widening_stops_at_the_pin_strip` (pin strip's
  first cell still `pin_chip_hit`'s, `center_tab_hit` → `None` there).

## Verification (scoped per the dev-loop policy)

- `just quick thegn-host` — clean (clippy `-D warnings`).
- `cargo nextest run -p thegn-host drag_hit center_tab pin_chip row_at
sidebar_mouse` — 44/44 pass.
- Pre-existing guards pass untouched (chrome_tests.rs diff purely additive):
  `center_tab_spans_use_display_width_for_wide_leaf` (column 11 `None`) and the
  column-0 `None` check; `pin_chip_hit` tests unchanged.

## Deviations

- The four exported items carry
  `#[cfg_attr(not(test), expect(dead_code))]`. The chunk spec said "no
  `#[allow(dead_code)]`" on the assumption that the module's own tests satisfy
  clippy — they don't for `just quick`, which compiles the bin **without**
  test targets, and a bin crate gives `pub` no dead-code exemption. The tests
  do call all four; the attribute is the repo's sanctioned transitional marker
  (`host_provision::plan_summary` pattern) and **self-destructs**: once chunk 2
  wires `run.rs`, the expectation stops being fulfilled and clippy `-D
warnings` forces its removal. Chunk 2 must delete these four attributes.

## Unverified

- The sidebar-first resolution of the 1-column-center overlap is asserted only
  as geometry (`sep_grab` both-true); the caller-side ordering lives in
  `run.rs` and is wired/verified in chunk 2.
- No e2e / visual run (excluded per spec; this chunk changes no frame —
  `center_tab_hit` is hit-test only, placement untouched).
- Real-mouse feel of the widened band (a click on the former gap column actually
  selecting the tab in the running TUI) — covered by unit tests only.
