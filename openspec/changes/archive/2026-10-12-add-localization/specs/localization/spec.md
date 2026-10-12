# Localization

## ADDED Requirements

### Requirement: Translations are compiled in with zero runtime I/O

Chrome strings SHALL be looked up through a `t!` macro against Fluent locale files compiled into the binary, and the active locale MUST be resolved exactly once during the `thegn::startup` waterfall before the first render (no runtime filesystem reads).

#### Scenario: Locale resolves before first frame

- **WHEN** thegn starts
- **THEN** the active locale is resolved once (from `[ui] language`, else the host
  locale) before the first frame, with no per-string file I/O

#### Scenario: Missing key falls back

- **WHEN** a `t!` key is absent in the active locale
- **THEN** it falls back rather than panicking

### Requirement: Translated strings respect terminal cell geometry

Layout SHALL measure translated strings by terminal cell width (`unicode-width`), not byte length, and MUST truncate or flex when a string exceeds its panel budget; padding MUST be applied after translation.

#### Scenario: Long translation truncates

- **WHEN** a translated label exceeds its allotted columns
- **THEN** it is truncated/flexed to fit rather than overflowing the layout
