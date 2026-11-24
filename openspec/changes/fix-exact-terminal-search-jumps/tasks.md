## 1. Emulator distance

- [x] 1.1 Count logical lines from the bottom while joining wrapped rows in
      `locate_line_span`.
- [x] 1.2 Resolve duplicate text by logical distance instead of physical row
      offset.
- [x] 1.3 Cover wrapped output before a duplicate match in an emulator test.

## 2. Stepping after Enter

- [x] 2.1 Snapshot the ranked hits and selected index when the overlay jumps.
- [x] 2.2 Add `n`/`N` stepping with wrap and a 1-based status position.
- [x] 2.3 Clear the walk on another key, wheel scroll, mouse press, or stale
      selection.
- [x] 2.4 Cover forward/backward stepping and the no-hit case in tests.

## 3. Documentation

- [x] 3.1 Document the exact-line jump, mark lifetime, `n`/`N`, and the
      missing-line replay fallback in `docs/help/search.md`.

## 4. Verification

- [ ] 4.1 Run `just quick thegn-host` and the focused nextest filters.
- [ ] 4.2 Verify Enter and `n`/`N` against a live pane with wrapped history.
