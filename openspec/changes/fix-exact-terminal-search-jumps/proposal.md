# Fix exact terminal search jumps

## Why

Enter on a terminal-history search result is documented to land on the exact
grid line of the match. It could land on a different copy of the same text:
the emulator resolved duplicate lines by physical row distance, so preceding
wrapped output shifted the choice away from the logical line the result was
ranked from. A jump that missed the live grid also cleared the highlight and
left the user with no way to keep stepping through the ranked hits.

## What Changes

- Resolve duplicate matches by logical line distance from the bottom of the
  emulator history, matching the distance the search result carries.
- Keep the ranked hits after Enter closes the overlay, and let `n`/`N` step to
  the next and previous hit, moving the highlight and status position with it.
- End the walk and its mark on the next non-`n`/`N` key, a wheel scroll, a
  mouse press, or a selection invalidated by resize/tab change.
- Keep the existing missing-line fallback: the status names the unavailable
  line and points at replay rather than scrolling somewhere unrelated.

## Impact

Affected specs: `panes` (history-search requirements added).

Affected code: `thegn-host` emulator line lookup, search overlay walk state,
host key/scroll/mouse handling, and the search help page.
