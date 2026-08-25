# Tasks — theme contrast contract

## 1. The contract module (thegn-core)

- [x] 1.1 New sibling module `crates/thegn-core/src/theme_contrast.rs`
      (god-file guidance: do not grow `theme.rs`): the rule table, `Bar`
      (`Default`/`Preset`), `ContrastFinding`, and pure
      `audit(&Palette, Bar) -> Vec<ContrastFinding>` built on
      `theme::contrast_ratio`. Audit the palette **post** `extend_palette`,
      including the derived `ghost2`/`ghost3` and `sel_accent()`.
- [x] 1.2 Anchor unit tests for `audit()` itself: a synthetic palette
      violating exactly one rule yields exactly that finding (rule name, pair,
      ratio, floor); a fully passing palette yields an empty report; findings
      are deterministic and ordered. (Pure logic — 95 % line gate on core.)
      (`audit_reports_exactly_the_violated_pair` isolates a single faint/raise
      finding; `audit_of_a_clean_palette_is_empty`; `audit_is_deterministic`;
      `audit_reports_derived_tokens_not_just_table_values`.)

## 2. The preset sweep

- [x] 2.1 One exhaustive test: for every name in `PRESETS`, resolve +
      `extend_palette`, run `audit(…, Bar::Preset)` (prism with
      `Bar::Default`), assert the report is empty, printing every finding on
      failure. (`every_shipped_preset_satisfies_the_contrast_contract`.)
- [x] 2.2 Delete `every_preset_keeps_copy_legible` and
      `light_preset_hues_are_paper_legible` (subsumed by 2.1); keep the
      prism-strict tests, the fg-ramp/heat monotonicity tests, and
      `filled_chip_text_is_legible_on_every_hue` only if not folded into the
      table (fold preferred — one source of floors). (All three deleted; the
      chip rule is folded into the contract as `chip-on-fill`.)

## 3. Retune failing presets

- [x] 3.1 Retune `light`: lift `ghost`, `faint`, and whatever the derived
      `ghost2`/`ghost3` need. Kept the paper-bright character and the
      hand-tuned ink hues. (ghost #b6bccb→#7f828d, faint #888f??→#6b707f,
      dim darkened; ghost2/ghost3 now derive contrast-aware — see note below.
      `border` already cleared, so unchanged.)
- [x] 3.2 Retune `solarized-light` the same way. (ghost →#6e7a7a, dim
      darkened; green/amber/orange hues deepened for status text on panel2.)
- [x] 3.3 Run the sweep across the remaining presets and retune every other
      failure minimally (adjust lightness, keep hue/character). No preset
      needed a named exception. **Structural derivation note:** plain option
      (a) (brighten `ghost` enough to feed the dumb `ghost2`/`ghost3` blend)
      forced `ghost` _above_ `faint` on dracula/onedark/solarized-dark (a ramp
      inversion), so `extend_palette` now uses the design's sanctioned option
      (b): derive the fade, then nudge back toward `ghost` until the step
      clears the structural floor (`structural_fraction`). This is a single
      seam change that fixes structural failures for both polarities and keeps
      every `ghost` below `faint`. Every other retune is per-preset table
      values (ghost/faint/dim/text lifts, panel2/raise darkened where a
      selection/hover surface was too light for its text, nord/dracula red
      lifted off their lighter panels).
- [ ] 3.4 Eyeball each retuned preset live (`thegn theme list`, Ctrl+Alt+t
      cycle) — the contract catches collapse, not taste. **Not done in this
      env (no live TUI; e2e frozen).** Two presets to eyeball for taste:
      `solarized-dark` and `everforest-dark` selection/hover collapsed close
      to `panel` (still readable, subtler highlight) — flagged for review.

## 4. Docs + downstream

- [x] 4.1 `config/config.toml.example` needed no change (its documented
      `[theme.colors]` defaults are the prism hexes, and prism is unchanged).
      Updated `docs/extending/theme.md`: adding a preset now names the contrast
      contract sweep as a gate, with the 5-surface / derived-ghost guidance.
- [ ] 4.2 Theme-styled muse baselines: **not re-recorded** (e2e is the
      known-broken/frozen local gate — per the change brief, do not run or
      update e2e). Retuned presets that alter frames: `storm`, `light`,
      `abyss`, `ember`, `aurora`, `catppuccin-macchiato`, `nord`, `dracula`,
      `gruvbox-dark`, `tokyo-night`, `solarized-dark`, `solarized-light`,
      `rose-pine`, `onedark`, `monokai-pro`, `ayu-dark`, `ayu-mirage`,
      `everforest-dark`, `kanagawa` (night-owl already passed, unchanged).

## 5. Validation

- [x] 5.1 `just quick thegn-core` (clippy clean) + `cargo nextest run -p
thegn-core theme` (43 pass, incl. the sweep + 4 anchors) while iterating.
- [ ] 5.2 Run `just ci` once, when the change is complete (includes
      openspec-validate and the core coverage gate). **Left for the reviewer**
      (change brief: do not run full-workspace gates; leave uncommitted).
