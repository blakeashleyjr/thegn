# Tasks — theme builder overlay

Depends on `add-theme-contrast-contract` (`theme_contrast::audit`) for 3.4 and
5.5; everything else is independent.

## 1. User themes (core model + host loading)

- [ ] 1.1 New `crates/thegn-core/src/theme_user.rs`: `UserTheme` (`meta` +
      `colors`/`hues` mirroring `ThemeColors`/`ThemeHues`), TOML
      parse/serialize, name slugification, and pure
      `to_palette(&UserTheme) -> Palette` (+ `extend_palette`). Unit tests:
      round-trip, slug rules, malformed-hex fallback, collision detection
      (95 % gate).
- [ ] 1.2 Host: themes-dir scan (`$XDG_CONFIG_HOME/thegn/themes/*.toml`) on a
      background thread at startup and on watch events — channel +
      `TerminalWaker` pulse, never on the loop; per-file size cap; corrupt
      files skipped with a status warning.
- [ ] 1.3 Resolution: user themes join the preset namespace — built-ins win
      collisions (warn); `palette_with_preset`-equivalent path applies
      `[theme.colors]`/`[theme.hues]` overrides on top of a user theme;
      `Action::CycleTheme` order and `thegn theme list` append user themes
      (marked `user`).
- [ ] 1.4 Extend the config fs-watch registration to the themes dir so an
      edited theme file live-reloads the palette.

## 2. Gogh import (pure core + CLI)

- [ ] 2.1 New `crates/thegn-core/src/theme_import.rs`: minimal flat
      `key: value` YAML-subset parser, panic-free, size-capped, no serde_yaml
      dependency.
      Hostile-input unit tables: truncation, BOM, huge lines, escape bytes in
      values, non-UTF-8.
- [ ] 2.2 Gogh mapping (`color_01..16`/`background`/`foreground`/`variant`)
      → token `Palette` per the design's mapping table; `variant: light`
      flips surface derivation. Unit tests with dark and light Gogh fixtures
      assert light-stays-light and hue assignment.
- [ ] 2.3 `thegn theme import <file> [--name <n>]` in `cmd/theme.rs`; import
      writes the user-theme file and prints the contrast-audit summary.
- [ ] 2.4 Fix the persist bug: one helper writing `[theme] preset` via
      `toml_edit` (not `theme.name`); `thegn theme set <name>` becomes
      non-interactive; drop the hard fzf/gum requirement. Smoke-test the
      write in `test/smoke.sh` (config write seams are smoke territory).

## 3. The overlay (host)

- [ ] 3.1 New `crates/thegn-host/src/theme_builder.rs`: overlay state machine
      (preset list / token editor / preview strip; filter, selection,
      pending-edit state) with logic factored pure where possible for unit
      tests; render via `layer::open_layer` + `seg`, every swatch a palette
      role (color-literal ratchet must not grow).
- [ ] 3.2 `handlers/theme_builder.rs` + `run.rs` dispatch arm: open/close,
      key routing, live `chrome::set_palette` preview, Esc revert to
      `current_config.palette()`, Enter persist (via 2.4's helper),
      config-reload-while-open re-applies the preview candidate.
- [ ] 3.3 Token editing: inline hex input (`menu::InputOverlay` pattern),
      re-resolve + live apply per edit; persist confirmed edits to
      `[theme.colors]`/`[theme.hues]` via `toml_edit`.
- [ ] 3.4 Contrast badges: run `theme_contrast::audit` on the candidate
      palette; render failing pairs inline (ratio + floor). Warn-only.
- [ ] 3.5 In-overlay import (`i` → path input → off-loop read/parse →
      result + warnings) and save-as (`s` → name input → 1.1 writer).
- [ ] 3.6 Mouse: click-to-select and wheel scroll in the lists via the layer
      hit-test scope.

## 4. Action, keymap, help (the enforced gates)

- [ ] 4.1 `Action::ThemeBuilderOpen` per `docs/extending/action.md`: enum +
      `key()`/`from_key()` (`theme-builder-open`), `ActionSpec` (label, hint,
      keywords, palette row), default chord `Ctrl+Alt+Shift+t` (verify no
      collision), dispatch arm. Gates:
      `every_action_key_has_a_spec_and_round_trips`,
      `declared_default_chords_actually_dispatch`,
      `every_action_has_search_keywords`.
- [ ] 4.2 New `docs/help/theming.md`: claims `theme-builder-open` (and
      `cycle-theme` if it migrates here), documents overlay keys, user
      themes, Gogh import, contrast badges. Help ratchet + prose ratchet
      stay shrink-only — no new allowlist entries.
- [ ] 4.3 Document user-theme names for `[theme] preset` and the themes dir in
      `config/config.toml.example` (no new config keys).
- [ ] 4.4 Update `docs/extending/theme.md`: the second path (a user theme
      file) beside the built-in-preset recipe.

## 5. Verification

- [ ] 5.1 Iterate with `just quick thegn-host` / `just quick thegn-core` and
      targeted `cargo nextest run -p <crate> theme` — no full-workspace gates
      per edit (dev-loop policy).
- [ ] 5.2 Drive the overlay by hand with `muse session` (read
      `docs/testing-with-muse.md` first) — open, preview, revert, edit,
      import, save-as.
- [ ] 5.3 Record muse e2e coverage for the overlay (open + preview + revert)
      and re-record any baselines a new default-visible change touches with
      `just e2e-update`. Note: e2e is currently a **local-only** gate with
      stale committed baselines (see CLAUDE.md) — record locally, review the
      diff, do not gate on CI e2e.
- [ ] 5.4 `just term-check`: degraded (256/16/mono) rendering of the overlay
      still resolves through the chokepoints.
- [ ] 5.5 Confirm import warnings and builder badges agree with
      `theme_contrast::audit` on a deliberately washed-out fixture.
- [ ] 5.6 Run `just ci` once, when the change is complete (includes
      openspec-validate, lint ratchets, coverage).
