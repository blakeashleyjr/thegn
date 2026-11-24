## Context

`SearchMatch::from_tail` is the number of logical history lines between the
match and the bottom of the terminal history. `PaneEmulator::locate_line_span`
receives that value as `hint_from_bottom` and chooses between occurrences of the
same logical text. The emulator walks history rows newest-first, but one
logical line can occupy several physical rows when output wraps.

## Goals / Non-Goals

**Goals**

- `hint_from_bottom` selects the occurrence the search ranked.
- The post-Enter mark can be stepped with `n`/`N` without reopening the overlay.
- A missing line still falls back to replay instead of an approximate scroll.

**Non-Goals**

- New key bindings or a persistent search mode.
- Reconstructing lines that have already left the emulator.

## Decisions

### Count logical lines, not physical rows

`locate_line_span` numbers the logical lines that end above the cursor row
(ordinal 0 is the newest line the history ring holds) while it joins wrapped
rows. The cursor row holds the unterminated partial line and the rows below it
are blank, so neither is counted. `consider` compares that ordinal with
`hint_from_bottom`; the physical `top` offset remains the returned scroll
target. This mirrors the distance the search engine computed and is stable
across wrapping and `clear`.

### Walk state is a snapshot of the ranked hits

`SearchWalk` clones the overlay's ranked matches and remembers the selected
index. When Enter lands before the scan finished, the walk keeps the overlay and
its first step ranks the overlay's snapshot, so `n`/`N` cover every hit. Enter seeds it from the selected result. `step` advances with modulo
arithmetic, so `n` at the last hit wraps to the first and `N` at the first wraps
to the last. The status renders the 1-based position and total.

The state lives beside the overlay in the host loop. It is cleared by any key
other than `n`/`N`, by a wheel scroll over a pane, by a mouse press, and when a
pane selection is invalidated. This keeps the mark from outliving the action it
describes without adding a new mode.

### Missing lines keep the replay fallback

`apply_search_jump` still returns no highlight when the emulator cannot locate
the line; the walk advances but the status remains the missing-line message
rather than claiming a successful position.
