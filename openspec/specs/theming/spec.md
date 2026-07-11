# Theming

## Purpose

thegn's appearance is fully configurable and live-reloadable: named presets
plus per-color overrides resolve into a single `Palette` carried in the frame
model, and stats use threshold colors. Chrome never hardcodes colors, so themes
can change at runtime without a restart.

## Requirements

### Requirement: Named presets with per-color overrides

Theming SHALL provide named presets (e.g. storm / light / abyss / ember / aurora) selectable via `[theme] preset` and cycleable live, and `[theme.colors]` overrides MUST apply on top of the chosen preset.

#### Scenario: Cycle preset at runtime

- **WHEN** the user cycles the theme preset
- **THEN** the new palette applies without a restart

#### Scenario: Override on top of a preset

- **WHEN** `[theme.colors]` sets a specific color
- **THEN** that value overrides the preset's color while the rest of the preset
  stands

### Requirement: A resolved palette drives all chrome

Colors SHALL be resolved into a `Palette` carried in the frame model and chrome MUST NOT reference theme color constants directly; an invalid hex override MUST fall back to the default.

#### Scenario: Malformed hex falls back

- **WHEN** a color override is not valid hex
- **THEN** the default color is used rather than failing to render

### Requirement: Theme reloads live via the config watch

A theme change SHALL apply through the existing configuration fs-watch without a restart.

#### Scenario: Edit theme config

- **WHEN** the theme configuration file changes on disk
- **THEN** the palette reloads and chrome repaints with the new colors
