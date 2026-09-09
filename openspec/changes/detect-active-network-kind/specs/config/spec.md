# Config — network icon selection delta

## ADDED Requirements

### Requirement: Network icon configuration preserves fixed overrides and supports automatic semantics

The built-in network-icon default SHALL use automatic medium selection. An
existing literal `[stats].net_icon` value SHALL remain a fixed override rather
than being reinterpreted. Automatic mode SHALL expose configurable Wi-Fi,
Ethernet, and generic glyphs with capability-appropriate ASCII-safe fallbacks.
The schema, example configuration, validation, help, environment layering, and
live reload SHALL describe the same auto-versus-fixed contract.

#### Scenario: An existing literal configuration is loaded

- **WHEN** a configuration written before automatic selection contains a literal `net_icon`
- **THEN** that literal remains the fixed displayed icon and validation does not silently opt it into detection

#### Scenario: Automatic glyphs reload

- **WHEN** automatic mode or one of its semantic glyphs changes during config reload
- **THEN** the next statusbar frame uses the newly resolved setting without restarting the compositor

#### Scenario: The terminal lacks semantic glyph capability

- **WHEN** automatic mode selects any medium in an ASCII-only terminal
- **THEN** rendering uses the configured or built-in ASCII-safe fallback without changing the selected semantic state
