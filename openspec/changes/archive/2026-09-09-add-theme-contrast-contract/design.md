# Design — theme contrast contract

## The contract is a table, not scattered asserts

Today's legibility knowledge is spread across five tests with three different
floors and two different color-math bases (WCAG `contrast_ratio` vs the crude
`lum()` channel sum). The change centralizes it: one `const` table of rules in
`crates/thegn-core/src/theme_contrast.rs`, one pure `audit()` that evaluates a
resolved `Palette` against it, and one exhaustive test that sweeps
`PRESETS`. A failing preset names the rule, the pair, the measured ratio, and
the floor — the same shape the theme builder (THE-7) will render as a badge.

```rust
pub enum Bar { Default, Preset }          // prism carries a higher `text` bar
pub struct ContrastFinding {
    pub rule: &'static str,               // "faint-on-surface", …
    pub fg: &'static str, pub bg: &'static str,
    pub ratio: f32, pub min: f32,
}
pub fn audit(p: &Palette, bar: Bar) -> Vec<ContrastFinding>;
```

`audit()` takes the palette **after** `extend_palette` — derived tokens
(`ghost2`/`ghost3`, shadow, heat, `sel_accent()`) are what chrome actually
draws, so derivation bugs are in scope, not just table values.

## The rule matrix (WCAG 2.x ratios, adapted to terminal cells)

Floors are uniform across presets — the entire point of THE-6 is that light
mode must meet the same bar as dark — with a single exception: the shipped
default keeps its stricter `text` floor (AAA), preserving today's prism tests.

| foreground                     | backgrounds                | floor                    | rationale                                                                                                                                                                                            |
| ------------------------------ | -------------------------- | ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `text`                         | bg0 bg1 panel panel2 raise | 4.5 (7.0 on the default) | body copy: AA (AAA default)                                                                                                                                                                          |
| `dim`                          | bg0 bg1 panel panel2 raise | 4.5                      | secondary copy: AA                                                                                                                                                                                   |
| `faint`                        | bg0 bg1 panel panel2 raise | 3.0                      | muted labels: AA-large                                                                                                                                                                               |
| `ghost`                        | bg0 bg1 panel              | 3.0                      | faintest _readable_ tier (timestamps, counts, hints); panel2/raise exempt — selection/hover re-tiers text                                                                                            |
| `ghost2` `ghost3` `border`     | bg0 bg1 panel              | 1.5                      | structural floor: rules/fills/tracks must not vanish (deliberately below WCAG 1.4.11's 3:1 — a full-cell box-drawing line is far heavier than a 1-px border; the floor catches collapse, not web-AA) |
| `chip_fg`                      | accent + all 8 hues        | 3.5                      | filled chips (existing rule, kept)                                                                                                                                                                   |
| each hue                       | bg0 bg1 panel              | 3.0                      | hues are drawn as status/identity **text** (diff ±, CI states, agent names)                                                                                                                          |
| `focus` `accent`               | bg0 bg1                    | 3.0                      | UI affordances (focus frame, accent marks)                                                                                                                                                           |
| `activity_active/waiting/done` | bg0 bg1                    | 3.0                      | sidebar dots must be tellable apart from the tree background                                                                                                                                         |
| `text`                         | `sel_accent()`             | 4.5                      | selected-row copy on the derived accent tint                                                                                                                                                         |

Kept as-is from today's suite: fg-ramp monotonicity (text → ghost3 descends),
heat-ramp monotonicity away from `panel`, shadow-darker-than-bg0. Explicitly
out: `shadow_fg` vs `shadow_bg` (decorative dimming of covered content) and
heat-step absolute contrast (the ramp test already pins ordering).

Terminal adaptation notes baked into the floors: cell-sized glyphs at typical
terminal font sizes sit near WCAG's "large text" boundary, which is why the
metadata tiers use 3.0 rather than 4.5, and why the structural floor is a
visibility bound (1.5) rather than the non-text 3.0 — those lines are
one-cell-thick separators, not interactive component boundaries.

## What the sweep will force (known retunes)

Measured today, the sweep fails at least:

- `light`: `ghost` (1.54–1.76 → needs ~#6b7488-class ink), `faint`
  (2.09–2.99), derived `ghost2/ghost3` (1.07–1.40), `border` on panel (2.21).
- `solarized-light`: `ghost` (1.88 on panel), `border` (1.93), derived
  ghost2/3.
- An unknown number of dark presets' `ghost`/`faint` values (the tier was
  never gated outside prism) — e.g. tokyo-night's `ghost` is visibly sub-3.0
  on its panel. The implementation runs the audit, prints the findings, and
  retunes each failing value minimally (adjust lightness, keep hue).

`extend_palette`'s `ghost2 = blend(ghost, bg0, 0.62)` / `ghost3 = …0.38`
derivations fade _toward the background_ by construction, so on any palette
where `ghost` itself only just clears 1.5 the derived steps go under. Two
options: (a) retune per-preset `ghost` high enough that the derived steps
clear the structural floor, or (b) make the derivation contrast-aware (derive,
then nudge away from `bg0` until ≥ 1.5). Prefer (a) — it keeps
`extend_palette` a dumb blend and keeps every shipped value visible in the
table; fall back to (b) only if (a) distorts a preset's look. Either way the
audit gates the _result_, so the choice is an implementation detail.

## Alternatives considered

- **APCA (WCAG 3 draft) instead of WCAG 2.x.** APCA models dark-mode polarity
  better, but it is a moving draft, needs new math and new constants, and the
  repo already ships `contrast_ratio` with anchored tests. WCAG 2.x with
  terminal-adapted floors is sufficient to catch every measured THE-6 failure.
  Revisit only if retuning to WCAG floors visibly degrades a dark preset.
- **Gate user overrides too.** Rejected: `[theme.colors]` is the user's;
  warning is the builder's job (THE-7), not a config error. The contract binds
  shipped tables only.
- **Runtime doctor check.** Rejected: the palette is static data; a unit test
  is cheaper, earlier, and CI-enforced. `audit()` stays pure so any future
  surface (doctor, builder, import) can call it.

## Testing

All pure, all in `thegn-core` (95 % line gate): the `PRESETS` × matrix sweep;
anchor tests for `audit()` itself (a synthetic palette that violates exactly
one rule yields exactly that finding); the prism-strict tests kept; deletion of
`every_preset_keeps_copy_legible` and `light_preset_hues_are_paper_legible`
(subsumed). No smoke/e2e coverage needed beyond re-recording theme-styled muse
baselines that the retuned values alter.

## Security

None. Pure table + tests over static data: no I/O, no new dependencies, no new
externally invokable operation (no capability-catalog row), no config surface,
no subprocess, no change to the sandbox story.

## Open questions

- Should the `hues` floor also cover `panel2` (hue text on a selected row)?
  Measured `light` hues clear 3.0 on panel2 today except green/orange at
  ~3.06–3.10 — included would be nearly free; decide during implementation.
- Whether any dark preset's character genuinely cannot meet the uniform
  `ghost` 3.0 without losing its identity; if one does, the fallback is
  documented per-preset debt in the table (a named, shrink-only exception
  list), not a lowered global floor.
