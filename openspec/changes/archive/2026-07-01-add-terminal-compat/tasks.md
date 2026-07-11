# Tasks

## 1. Foundation (core)

- [x] 1.1 `thegn_core::termcaps`: `TermEnv`, `ColorDepth`, `UnicodeLevel`,
      `TermCaps`, pure `detect`, `GlyphSet` + `UNICODE`/`ASCII` + `glyphs(level)`;
      move `undercurl_supported_env` into core (re-export from `wire`).
- [x] 1.2 Host `caps.rs` render-time holder (atomics + `RwLock`), `resolve_termcaps`
      in `run.rs`, installed at startup + config reload.
- [x] 1.3 `cfg(test)` thread-local capability overrides for test isolation.

## 2. Display width

- [x] 2.1 Adopt `unicode-width`; `Seg::width`, `cut`, `take_cols`, `draw_text`,
      borders title, logotype centering measure display width.

## 3. Color degradation

- [x] 3.1 Pure `rgb_to_256` / `rgb_to_16` / `index_256_to_rgb` quantizers in core.
- [x] 3.2 Depth-aware `wire::color_spec`; `WireRenderer.set_depth`; drop color
      SGRs under `none`.

## 4. Glyph degradation

- [x] 4.1 Route borders/chrome/pins/logotype glyphs through `caps::active_glyphs()`.
- [x] 4.2 Force the text splash on ASCII terminals.

## 5. Startup probe

- [x] 5.1 Pure `interpret_probe` / `apply_probe` in core.
- [x] 5.2 Host `probe.rs`: tty-gated, time-bounded raw DA + XTVERSION read before
      `BufferedTerminal::new`; fold into caps at install.

## 6. Config + diagnostics

- [x] 6.1 `[theme] color` / `glyphs` `config_enum!`s + `THEGN_THEME_*` env
      overrides, documented in `config.toml.example`.
- [x] 6.2 `thegn doctor [--json]` (`cmd/doctor.rs`).

## 7. Docs

- [x] 7.1 README compatibility section; CLAUDE.md rendering note.
- [x] 7.2 `openspec validate --strict`.
