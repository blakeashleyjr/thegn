# Add a theme preset

1. Add the name to `PRESETS` and a `match` arm in `preset()` in
   `crates/thegn-core/src/theme.rs`, returning a `Palette` of `"R;G;B"`
   fragments; legacy 12-slot presets are extended by `extend_palette`.
2. Name roles, not colors, at draw sites: chrome asks the active palette for a
   `Hue`; literal RGB outside `wire.rs` / `caps.rs` / `theme*` / the ratatui
   bridges is debt.
3. Mention the preset in `docs/help/configuration.md` (`[theme] preset`).

**Gates:** the `PRESETS` iteration test (every fragment parses as R;G;B),
`color_literals_stay_in_the_chokepoints`, `just term-check` (degradation
still resolves).
