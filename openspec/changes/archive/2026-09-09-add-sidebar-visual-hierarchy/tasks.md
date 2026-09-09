# Tasks — sidebar visual hierarchy (THE-64)

## Delivered scope

- [x] Add `[ui] sidebar_dividers` with documented default and config coverage.
- [x] Add the derived `panel_alt` token through the complete theme-slot chain and
      contrast audit.
- [x] Tint alternating project blocks without adding geometry rows; preserve
      section-heading spacing and clipping behavior.
- [x] Make project/host headers the strong tier and folder headers subordinate.
- [x] Preserve row/hit geometry, rail/filter behavior, and token/glyph ratchets.
- [x] Update sidebar help and changelog.
- [x] Cover block parity, row backgrounds, header tiers, config, and geometry
      with focused tests.
- [x] Reconcile the accepted tint design after the rejected separator-row
      experiment (`149bb700`, `36947a32`, fixes `d934df80`).
- [x] Strictly validate this change.

## Validation boundary

No separate historical live mono/16-color pass or visual-baseline re-record is
claimed. The accepted implementation is geometry-neutral and is covered by the
theme/terminal and sidebar unit gates recorded in the merged history.
