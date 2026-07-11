# Add localization

## Summary

Add a type-safe, zero-runtime-I/O localization (i18n) layer for the chrome:
Fluent (`.ftl`) translations compiled into the binary, a `t!("key")` macro, OS
locale detection with a `[ui] language` override, and cell-width-aware layout so
translated strings respect terminal geometry.

Source design: `docs/superpowers/specs/2026-06-25-localization-strategy.md`.

## Impact

- New capability `localization` (chrome i18n); touches `config.rs` (`[ui]`).
- Relates to the AI track only insofar as the proxy may pass the active locale to
  agents (Track 2, out of scope here).

## Rationale

The sub-300ms startup and zero-idle invariants forbid runtime file I/O for
translations, so `fluent-templates` + `rust-embed` bake locales into the binary and
the locale resolves once during the `thegn::startup` waterfall. Terminal i18n's
real hazard is layout geometry (a 4-cell label may become 9 cells), handled with
`unicode-width` and responsive/truncating layout.

## Non-goals

- Dynamic filesystem language packs without recompilation (Phase 1).
- Translating user data (branch names, commits, subprocess output).
- The agent/LLM translation layer (separate from UI localization).
