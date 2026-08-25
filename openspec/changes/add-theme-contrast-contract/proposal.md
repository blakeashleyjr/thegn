# Theme contrast contract — light themes must be as legible as dark ones

Linear: THE-6

## Why

"Light themes don't have enough contrast" (THE-6) is measurably true, and the
reason is structural: the machine-checked contrast bar applies **only to the
shipped default**. `readable_text_tiers_clear_contrast_on_every_surface` and
`ghost_tier_stays_legible_on_standard_surfaces` (in
`crates/thegn-core/src/theme.rs`) hold **prism** to WCAG floors (text ≥ 7.0,
dim ≥ 4.5, faint ≥ 3.0, ghost ≥ 3.0), while every other preset only passes
`every_preset_keeps_copy_legible` — looser floors (4.5 / 4.0 / 2.3), on three
of the five surfaces, and with **no check at all** on the ghost tier, the hues,
the chip text, selection tints, or the focus/accent affordances beyond a crude
channel-sum heuristic for `light`.

Measured on today's palettes (WCAG 2.x ratios, the same math as
`theme::contrast_ratio`):

- `light` `ghost` on bg0/bg1/panel: **1.76 / 1.64 / 1.54** — and chrome
  renders _readable_ recessive metadata in `ghost` (timestamps, counts, empty
  states, key hints, path prefixes). The prism ghost-lift that fixed exactly
  this class of bug (see the `ghost_tier…` test comment) lifted **dark mode
  only**; light was left at ~1.5:1, which is the substance of THE-6.
- `light` `faint` on the five surfaces: **2.99 / 2.79 / 2.61 / 2.31 / 2.09** —
  every one below the 3.0 AA-large bar prism is held to; the two
  selection/hover surfaces aren't gated at all today.
- `light` derived `ghost2`/`ghost3` (via `extend_palette`): **1.07–1.40** —
  structural rules that effectively vanish on paper.
- `solarized-light` `ghost` on panel: **1.88**; border vs panel: **1.93**.

Nothing catches a regression here, and nothing caught the original drift. The
palette is a pure table in 95 %-covered `thegn-core` — this is exactly the kind
of logic the coverage gate exists for.

## What Changes

- **Define the contrast contract**: a single, named table of (foreground role ×
  background role × minimum WCAG ratio) covering every token pair chrome
  actually composes — readable text tiers on all five surfaces, the ghost
  metadata floor, the structural floor, chip text on accent/hues, hues as
  status text, focus/accent/activity affordances, and the derived selection
  tints (`sel_accent()`), evaluated on the **resolved** palette (post
  `extend_palette`).
- **Implement it as a pure audit** in a new sibling module
  `crates/thegn-core/src/theme_contrast.rs`: `audit(&Palette, Bar) ->
Vec<ContrastFinding>` (empty = pass), reusing the existing
  `theme::contrast_ratio`. No I/O, no new dependencies.
- **Gate every shipped preset** with one exhaustive unit test over
  `PRESETS` × the contract table, replacing the loose
  `every_preset_keeps_copy_legible` floors and the channel-sum
  `light_preset_hues_are_paper_legible` heuristic. The prism-strict tests stay
  (the default keeps its higher `text` bar).
- **Retune the failing presets** — `light` and `solarized-light` first (the
  THE-6 complaint), plus any other preset the sweep flags — adjusting
  lightness while keeping each preset's character. Derivation constants in
  `extend_palette` may need light-aware handling where blending toward `bg0`
  produces sub-floor structural tones on light surfaces.
- **Expose the audit for reuse**: the theme-builder overlay
  (`add-theme-builder-overlay`, THE-7) consumes `audit()` for live per-token
  contrast badges and import warnings. User `[theme.colors]` overrides are
  _not_ gated (user freedom) — the contract binds what thegn ships.

## Impact

- **Linear**: THE-6.
- **Roadmap**: group **N** — hardens **N 172** (Light/dark/auto) and the
  legibility story behind **N 171/173**; the audit is the reusable half of the
  theme-builder work (**N 182** via `add-theme-builder-overlay`).
- **Specs**: `theming` — ADDED requirements (contrast contract; light presets
  held to the same floors).
- **Code**: `crates/thegn-core/src/theme_contrast.rs` (new),
  `theme.rs` (preset value retunes + test replacement). Pure core logic under
  the 95 % line gate; no host changes, no config keys, no DB, no catalog rows,
  no event-loop or render-path involvement.
- **In-flight changes**: `add-theme-builder-overlay` depends on this change's
  `audit()` API. No overlap with any other in-flight change.
- **e2e**: retuned `light`/`solarized-light` values alter frames only in muse
  specs that style or cycle themes (task 740 recorded "theme styled
  snapshots"). Affected baselines must be re-recorded with `just e2e-update`
  (e2e is currently a local-only gate with stale baselines — see CLAUDE.md);
  noted in tasks.
- **Non-goals**: APCA/OKLCH perceptual scoring (WCAG 2.x is already in-repo,
  order-independent, and good enough to catch collapse); gating user
  overrides; runtime enforcement (the contract is a build-time table test —
  the runtime cost is zero).
