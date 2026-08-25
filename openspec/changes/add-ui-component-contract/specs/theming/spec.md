# theming — delta for add-ui-component-contract

## ADDED Requirements

### Requirement: Glyphs are tokens beside color slots

The theming layer SHALL provide a glyph token vocabulary — a pure-data token enum in `thegn_core::termcaps` beside `GlyphSet`, core-coverage-gated — resolved once through the capability chokepoint (`caps::active_glyphs()`), symmetrical with how color tokens resolve through the palette and quantize once in `wire.rs`. Element content MUST be expressible entirely in tokens (color and glyph), so a migrated draw site carries no glyph literal; each migrated site SHALL delete its `test/glyph-literal-ratchet.txt` entry, and the ratchet remains shrink-only.

#### Scenario: A glyph token degrades on an ASCII terminal

- **WHEN** an element row uses a glyph token and the active terminal capabilities select the ASCII glyph set
- **THEN** the token resolves to the ASCII fallback at the chokepoint, with no branching at the draw site

#### Scenario: A new glyph literal at a draw site is refused

- **WHEN** a migrated element's builder embeds a raw Unicode glyph instead of a token
- **THEN** the glyph-literal ratchet fails for that file, since its allowlist entry was already burned
