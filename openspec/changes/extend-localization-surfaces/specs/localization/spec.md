# Localization

## ADDED Requirements

### Requirement: The e2e freeze pins the locale

Under `THEGN_E2E=1` the active locale SHALL resolve to `en-US` regardless of `[ui] language` and the host locale, so muse baselines are locale-deterministic; the pseudolocale hook MUST be inert under the freeze.

#### Scenario: Frozen instance ignores configured language

- **WHEN** thegn starts with `THEGN_E2E=1` and `[ui] language = "ja-JP"`
- **THEN** the active locale is `en-US` and rendered chrome matches the recorded en-US baselines

### Requirement: A pseudolocale proves layout safety without translations

thegn SHALL derive a test-only pseudolocale from the en-US bundle (non-ASCII, cell-width expanded, interpolation placeholders preserved) and unit tests MUST use it to prove translated strings truncate/flex within their panel budgets; it SHALL be reachable for developers only via `THEGN_PSEUDOLOCALE=1` (never a selectable `[ui] language` value, never active under `THEGN_E2E`).

#### Scenario: Expanded strings truncate

- **WHEN** a chrome layout site renders a pseudolocale string wider than its allotted columns
- **THEN** it truncates/flexes to the cell budget rather than overflowing

#### Scenario: Pseudolocale is not a language

- **WHEN** a user sets `[ui] language` to the pseudolocale identifier
- **THEN** it is treated as an unknown locale (per-key fallback to en-US), not served

### Requirement: en-US is the translation key schema

Every embedded locale SHALL be validated against the en-US key set by a unit test: a key present in another locale but absent from en-US (orphan) MUST fail the test; keys missing from a locale are per-key fallback (allowed) and MUST be reported, with `thegn doctor` printing the resolved locale and per-locale key coverage.

#### Scenario: Orphan key fails

- **WHEN** a locale file adds a key that en-US does not define
- **THEN** the parity test fails naming the locale and key

#### Scenario: Partial locale is usable

- **WHEN** the active locale lacks a key
- **THEN** the en-US string renders for that key and `thegn doctor` shows the locale's coverage below 100%

### Requirement: Relative times and calendar names localize through one core layer

Relative ages, durations, and the month/weekday names thegn draws SHALL render through a shared pure `thegn-core` formatter family backed by Fluent keys with CLDR plural categories, replacing the per-site English literals and duplicated helpers in chrome; the user-configured `[bars]` clock/date strftime strings SHALL keep their structure, with name-producing tokens (`%a`/`%A`/`%b`/`%B`) resolved from the locale name tables rather than chrono's English defaults; with only en-US embedded the layer's output MUST equal the current literals (no visual churn).

#### Scenario: Plural category applies

- **WHEN** a relative age of 1 minute and of 2 minutes render in a locale with distinct plural forms
- **THEN** each uses that locale's correct plural variant via the Fluent key

#### Scenario: en-US is byte-stable

- **WHEN** the scattered "N ago" sites are routed through the layer with only en-US embedded
- **THEN** the rendered strings are identical to the previous literals

#### Scenario: Date widget names localize without changing the format

- **WHEN** the masthead date widget renders its configured `%a %b %-d` under a locale with translated name tables
- **THEN** the weekday/month names come from the locale tables while the user's configured token structure and numerics are unchanged

### Requirement: RTL locales are withheld and RTL user data is neutralized

thegn SHALL NOT embed an RTL locale until it has a terminal-bidi rendering story (an RTL `[ui] language` value resolves with per-key fallback to en-US); bidi control characters (U+202A–U+202E, U+2066–U+2069, LRM/RLM) in user-supplied strings MUST be neutralized at the chrome compose edge so they cannot reorder or spoof thegn-drawn text, while pane content passes through untouched.

#### Scenario: RTL branch name cannot reorder chrome

- **WHEN** a branch named with embedded bidi override characters renders in the sidebar
- **THEN** the composed chrome cells contain no bidi control characters and adjacent labels keep their visual order

#### Scenario: Interpolated data is data

- **WHEN** a user string containing `{` and Fluent-like syntax is interpolated via `t!`
- **THEN** it renders literally with no expansion

### Requirement: Locale changes take effect on restart

The active locale SHALL resolve exactly once at startup; editing `[ui] language` in a running instance MUST NOT relocalize live chrome, and the documented behaviour (config comment, help) SHALL state that a restart applies it.

#### Scenario: Live edit does not switch

- **WHEN** `[ui] language` changes while thegn is running
- **THEN** chrome language is unchanged until the next start
