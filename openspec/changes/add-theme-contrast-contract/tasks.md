# Tasks — theme contrast contract

## 1. The contract module (thegn-core)

- [ ] 1.1 New sibling module `crates/thegn-core/src/theme_contrast.rs`
      (god-file guidance: do not grow `theme.rs`): the rule table, `Bar`
      (`Default`/`Preset`), `ContrastFinding`, and pure
      `audit(&Palette, Bar) -> Vec<ContrastFinding>` built on
      `theme::contrast_ratio`. Audit the palette **post** `extend_palette`,
      including the derived `ghost2`/`ghost3` and `sel_accent()`.
- [ ] 1.2 Anchor unit tests for `audit()` itself: a synthetic palette
      violating exactly one rule yields exactly that finding (rule name, pair,
      ratio, floor); a fully passing palette yields an empty report; findings
      are deterministic and ordered. (Pure logic — 95 % line gate on core.)

## 2. The preset sweep

- [ ] 2.1 One exhaustive test: for every name in `PRESETS`, resolve +
      `extend_palette`, run `audit(…, Bar::Preset)` (prism with
      `Bar::Default`), assert the report is empty, printing every finding on
      failure.
- [ ] 2.2 Delete `every_preset_keeps_copy_legible` and
      `light_preset_hues_are_paper_legible` (subsumed by 2.1); keep the
      prism-strict tests, the fg-ramp/heat monotonicity tests, and
      `filled_chip_text_is_legible_on_every_hue` only if not folded into the
      table (fold preferred — one source of floors).

## 3. Retune failing presets

- [ ] 3.1 Retune `light`: lift `ghost` (measured 1.54–1.76 on its surfaces),
      `faint` (2.09–2.99), `border`, and whatever the derived
      `ghost2`/`ghost3` need (prefer darker `ghost` over making
      `extend_palette` contrast-aware; see design). Keep the paper-bright
      character and the hand-tuned ink hues.
- [ ] 3.2 Retune `solarized-light` the same way (measured `ghost` 1.88,
      `border` 1.93, derived ghost2/3 ≤ 1.70).
- [ ] 3.3 Run the sweep across the remaining presets and retune every other
      failure minimally (adjust lightness, keep hue/character). If any preset
      genuinely cannot meet a floor without losing its identity, record it in
      a named in-table exception with a comment — never lower a global floor.
- [ ] 3.4 Eyeball each retuned preset live (`thegn theme list`, Ctrl+Alt+t
      cycle) — the contract catches collapse, not taste.

## 4. Docs + downstream

- [ ] 4.1 Update the `[theme.colors]` comment block in
      `config/config.toml.example` if any documented default hex changed, and
      the `docs/extending/theme.md` recipe: adding a preset now also means
      passing the contrast contract test (name it as a gate).
- [ ] 4.2 Check the theme-styled muse baselines (task 740 recorded some):
      re-record any affected by the retunes with `just e2e-update` and review
      the diff. Note: e2e is currently a local-only gate with stale
      baselines (see CLAUDE.md) — record the delta locally; do not block on
      CI e2e.

## 5. Validation

- [ ] 5.1 `just quick thegn-core` + `cargo nextest run -p thegn-core theme`
      while iterating (dev-loop policy: no full-workspace gates per edit).
- [ ] 5.2 Run `just ci` once, when the change is complete (includes
      openspec-validate and the core coverage gate).
