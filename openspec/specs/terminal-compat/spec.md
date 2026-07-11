# terminal-compat Specification

## Purpose

Detecting the outer terminal's capabilities (color depth, glyph level, undercurl, mouse) and degrading color and glyphs at the render edges while composing in truecolor+Unicode.

## Requirements

### Requirement: Capabilities are detected purely from the environment

The host SHALL resolve the outer terminal's capabilities — color depth (truecolor/256/16/none), glyph level (full/basic/ascii), and undercurl/mouse/osc52/sync flags — from a `TermEnv` snapshot via a pure function, so detection is unit-testable without a terminal, and `NO_COLOR` MUST force color off unless an explicit `[theme] color` overrides it.

#### Scenario: Modern emulator gets full fidelity

- **WHEN** `$TERM`/`$TERM_PROGRAM` names a modern emulator (kitty/wezterm/ghostty/…) in a UTF-8 locale
- **THEN** detection resolves truecolor + full Unicode + undercurl

#### Scenario: Bare terminal degrades

- **WHEN** `TERM=xterm` (no `COLORTERM`) in a non-UTF-8 locale
- **THEN** detection resolves 16-color + ASCII glyphs + no undercurl

#### Scenario: NO_COLOR forces monochrome

- **WHEN** `NO_COLOR` is set and `[theme] color` is `auto`
- **THEN** the resolved color depth is none and no color SGRs are emitted

### Requirement: Color is quantized at a single wire chokepoint

The frame SHALL always be composed in truecolor and quantized to the resolved depth at the one place colors reach the wire, mapping 24-bit to the nearest 256- or 16-color palette index and emitting no color at all under `none` — covering chrome and pane content identically, without per-cell allocation.

#### Scenario: Truecolor downsamples to a 256 index

- **WHEN** the terminal is 256-color and a cell carries a 24-bit color
- **THEN** the wire emits the nearest palette index, not a 24-bit SGR

#### Scenario: Monochrome emits no color

- **WHEN** the resolved depth is none
- **THEN** no foreground/background/underline-color SGRs are emitted, while non-color attributes (bold/italic) still are

### Requirement: Glyphs degrade to ASCII

Chrome glyphs (box drawing, status dots, arrows, the splash wordmark) SHALL be sourced from an active glyph set selected by the resolved glyph level, falling back to 7-bit ASCII (`+ - |`, `* o`, `^ v`, plain-text wordmark) when the terminal/locale lacks Unicode, with no glyph drawn that the set does not provide.

#### Scenario: Pane frame uses ASCII box

- **WHEN** the glyph level is ascii
- **THEN** pane frames render with `+ - |` and the logotype renders the text wordmark, never half-block glyphs

### Requirement: Chrome layout uses display width

Chrome layout and truncation SHALL measure text by display width (wide glyphs count as two cells), so a line that measures `w` paints exactly `w` columns and truncation never splits a wide glyph across the edge.

#### Scenario: Wide glyphs do not overflow

- **WHEN** a fixed-width row contains CJK/wide glyphs
- **THEN** the rendered row occupies exactly the allotted columns with no spill into the next cell/row

### Requirement: An optional startup probe refines detection without stalling launch

The host MAY query the outer terminal (Device Attributes + XTVERSION) before the input reader takes the tty, and SHALL fold a confirmed-modern reply into the env baseline by only upgrading `auto` fields; the probe MUST be tty-gated and time-bounded so a non-responding terminal never stalls launch, and MUST never feed response bytes into the event loop.

#### Scenario: Modern terminal over ssh is upgraded

- **WHEN** `$TERM` is generic but the XTVERSION reply names a modern emulator and config is `auto`
- **THEN** color/glyphs/undercurl are upgraded to full for the first frame

#### Scenario: Non-tty skips the probe

- **WHEN** stdin or stdout is not a tty (pipe / CI / tests)
- **THEN** the probe is skipped and env detection stands

### Requirement: Capabilities are configurable and inspectable

`[theme] color` and `[theme] glyphs` (each `auto` or an explicit value, with matching `THEGN_THEME_*` env overrides) SHALL override detection, and `thegn doctor [--json]` SHALL report the raw environment, the effective config modes, and the resolved capabilities with an enabled-vs-degraded summary.

#### Scenario: Explicit config beats detection and probe

- **WHEN** `[theme] glyphs = "ascii"` on a modern terminal
- **THEN** chrome renders ASCII glyphs regardless of detection or probe

#### Scenario: Doctor reports the resolution

- **WHEN** `thegn doctor` runs
- **THEN** it prints the detected environment, the resolved capabilities, and which features are enabled vs degraded
