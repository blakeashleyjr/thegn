# Theming — contrast contract deltas

## ADDED Requirements

### Requirement: Shipped palettes satisfy a machine-checked contrast contract

thegn SHALL define a single contrast contract — a table of foreground-role ×
background-role pairs with minimum WCAG 2.x contrast ratios adapted to
terminal cells — covering every token pair the chrome composes: the readable
text tiers (`text` ≥ 4.5, `dim` ≥ 4.5, `faint` ≥ 3.0) on every standard
surface (`bg0`, `bg1`, `panel`, `panel2`, `raise`); the recessive-metadata
floor (`ghost` ≥ 3.0 on `bg0`/`bg1`/`panel`); the structural floor (`ghost2`,
`ghost3`, `border` ≥ 1.5 on `bg0`/`bg1`/`panel`); chip text (`chip_fg` ≥ 3.5
on the accent and all eight hues); hues as status text (≥ 3.0 on
`bg0`/`bg1`/`panel`); focus/accent/activity affordances (≥ 3.0 on
`bg0`/`bg1`); and selected-row copy (`text` ≥ 4.5 on the derived
`sel_accent()` tint). The contract MUST be evaluated on the resolved palette
after extension/derivation, MUST be implemented as a pure audit function in
`thegn-core` reusable by other surfaces, and every shipped preset MUST pass it
via a unit test over the preset table. The shipped default preset SHALL
additionally hold `text` to ≥ 7.0 on every standard surface.

#### Scenario: A regressed preset value fails the build

- **WHEN** a shipped preset's `faint` is changed such that its ratio on
  `panel2` drops below 3.0
- **THEN** the contract unit test fails, naming the preset, the token pair,
  the measured ratio, and the required floor

#### Scenario: Derived tokens are audited, not just table values

- **WHEN** a preset's `ghost` value causes the derived `ghost2`/`ghost3`
  extension tokens to fall below the structural floor on `panel`
- **THEN** the audit reports the derived pair as a failure even though no
  table literal changed

#### Scenario: The audit is reusable on arbitrary palettes

- **WHEN** another surface (e.g. a theme editor or importer) resolves a
  palette and calls the audit function
- **THEN** it receives the list of failing pairs with measured ratios and
  floors, with no I/O performed

### Requirement: Light presets are held to the same contrast floors as dark presets

Every shipped light preset SHALL satisfy the same contract floors as the dark
presets — light-on-paper is not an excuse for a lower bar — and the previous
looser all-preset floors and channel-sum luminance heuristics SHALL be
replaced by the contract audit. User `[theme.colors]` / `[theme.hues]`
overrides MUST NOT be gated by the contract; it binds only what thegn ships.

#### Scenario: Light preset metadata text is legible

- **WHEN** the `light` preset renders recessive metadata (timestamps, counts,
  key hints) in the `ghost` tier on the panel surface
- **THEN** the pair's contrast ratio is at least 3.0, verified by the contract
  test rather than by inspection

#### Scenario: A user override below the floor still applies

- **WHEN** a user sets `[theme.colors] faint` to a value with a 1.4:1 ratio on
  their background
- **THEN** the override applies unchanged (the contract gates shipped presets,
  not user configuration)
