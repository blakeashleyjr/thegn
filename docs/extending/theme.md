# Add a theme preset or user theme

## User themes

The theme builder stores validated, versioned TOML files under
`$XDG_CONFIG_HOME/thegn/themes`. The closed `UserTheme` model contains only
editable surface, text, border, accent, focus, and eight hue roles; derived
tokens are always rebuilt by `extend_palette`. Do not add derived fields to a
user file when the palette grows.

Gogh YAML and JSON imports are intentionally local and bounded. The core
importer accepts `name`, optional `variant`, `background`, `foreground`,
`cursor`, and `color_01` through `color_16`; the host owns path reads and
validated atomic writes. `foreground`, `background`, and `cursor` become
`text`, `bg0`, and `focus`, while the ANSI pairs seed the semantic hues.

The CLI flow is headless:

```sh
thegn theme import ~/Downloads/theme.yml --name paper
thegn theme set paper
```

A selected user theme is persisted through the existing `[theme].preset` key
and `[theme.colors]` / `[theme.hues]` overrides. There is no separate user-theme
config key and no export or network-import command.

1. Add the name to `PRESETS` and a `match` arm in `preset()` in
   `crates/thegn-core/src/theme.rs`, returning a `Palette` of `"R;G;B"`
   fragments; legacy 12-slot presets are extended by `extend_palette`.
2. Name roles, not colors, at draw sites: chrome asks the active palette for a
   `Hue`; literal RGB outside `wire.rs` / `caps.rs` / `theme*` / the ratatui
   bridges is debt.
3. Mention the preset in `docs/help/configuration.md` (`[theme] preset`).
4. Clear the **contrast contract** (`crates/thegn-core/src/theme_contrast.rs`):
   run `cargo nextest run -p thegn-core theme_contrast` — the sweep audits your
   preset (resolved + `extend_palette`) against every rule and fails naming the
   pair, ratio, and floor. Tune lightness/hue until it clears; the readable
   tiers (`text`/`dim` ≥ 4.5, `faint` ≥ 3.0) must hold on **all five** surfaces
   including `panel2`/`raise`, so keep selection/hover dark enough that copy
   riding them stays readable. `ghost` is derived scaffolding — `extend_palette`
   nudges `ghost2`/`ghost3` up to the structural floor for you; a too-dim
   `ghost` still fails its own 3:1 floor.

**Gates:** the `PRESETS` iteration test (every fragment parses as R;G;B),
`every_shipped_preset_satisfies_the_contrast_contract` (the contrast contract
sweep — a new preset must pass it), `color_literals_stay_in_the_chokepoints`,
`just term-check` (degradation still resolves).
