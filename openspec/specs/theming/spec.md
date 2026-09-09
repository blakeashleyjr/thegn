# Theming

## Purpose

thegn's appearance is fully configurable and live-reloadable: named presets
plus per-color overrides resolve into a single `Palette` carried in the frame
model, and stats use threshold colors. Chrome never hardcodes colors, so themes
can change at runtime without a restart.

## Requirements

### Requirement: Named presets with per-color overrides

Theming SHALL provide named presets (e.g. storm / light / abyss / ember / aurora) selectable via `[theme] preset` and cycleable live, and `[theme.colors]` overrides MUST apply on top of the chosen preset. The preset namespace SHALL also include user themes loaded from `$XDG_CONFIG_HOME/thegn/themes/*.toml`; a user theme name is selectable everywhere a built-in preset name is (`[theme] preset`, the live cycle, `thegn theme list`, the theme builder), built-in names MUST win a collision with a warning, and `[theme.colors]` / `[theme.hues]` overrides MUST apply on top of user themes exactly as they do on built-in presets.

#### Scenario: Cycle preset at runtime

- **WHEN** the user cycles the theme preset
- **THEN** the new palette applies without a restart

#### Scenario: Override on top of a preset

- **WHEN** `[theme.colors]` sets a specific color
- **THEN** that value overrides the preset's color while the rest of the preset
  stands

#### Scenario: A user theme is selectable like a preset

- **WHEN** `[theme] preset` names a theme file present in the user themes
  directory
- **THEN** the resolved palette is built from that file, with any
  `[theme.colors]` / `[theme.hues]` overrides applied on top

#### Scenario: A user theme colliding with a built-in name is shadowed

- **WHEN** a user theme file shares a built-in preset's name
- **THEN** the built-in preset wins and a warning identifies the shadowed file

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

### Requirement: A theme-builder overlay with live preview

thegn SHALL provide an in-process theme-builder overlay (a boxed layer, opened
by a bindable `theme-builder-open` action) that lists built-in presets and user
themes, live-applies the highlighted candidate to the runtime palette so the
entire screen previews it, and renders an in-popup preview strip of sample
chrome (text tiers per surface, the eight hues, a filled chip, a selection
row, diff markers, activity dots) drawn exclusively from palette roles — never
color literals. Dismissing the overlay MUST revert the runtime palette to the
saved theme; confirming MUST persist the selection. While the overlay is open,
a configuration reload from disk MUST NOT clobber the live preview.

#### Scenario: Preview without commitment

- **WHEN** the user moves the highlight across presets in the builder
- **THEN** the runtime palette follows the highlight, and pressing Esc
  restores the theme saved in configuration

#### Scenario: Confirming persists the selection

- **WHEN** the user confirms a highlighted theme
- **THEN** `[theme] preset` is updated in the configuration file with comments
  preserved, and the running instance keeps the applied palette

### Requirement: Per-token palette editing with contrast feedback

The theme builder SHALL let the user edit every `[theme.colors]` and
`[theme.hues]` token by hex value, re-resolving and live-applying the palette
per edit, and SHALL surface the contrast-contract audit's findings for the
candidate palette inline (failing pair, measured ratio, required floor) as
warnings that never block the edit. Invalid hex input MUST leave the previous
value in effect. Confirmed token edits SHALL persist as `[theme.colors]` /
`[theme.hues]` overrides via comment-preserving config writes.

#### Scenario: An illegible choice is flagged but allowed

- **WHEN** the user sets a `faint` value whose ratio on `panel2` is below the
  contract floor
- **THEN** the edit applies and the token row shows the failing pair with its
  measured ratio and the floor

#### Scenario: Invalid hex keeps the previous value

- **WHEN** the user enters a string that does not parse as `#rrggbb`
- **THEN** the token keeps its previous value and the input is rejected with a
  message

### Requirement: The current palette can be saved as a named user theme

thegn SHALL save the currently resolved palette as a named user theme file
under `$XDG_CONFIG_HOME/thegn/themes/`, using the same key vocabulary as the
`[theme.colors]` / `[theme.hues]` override tables plus minimal metadata. Names
MUST be slugified to a safe character set and writes MUST be confined to the
themes directory. User theme files SHALL participate in live reload through
the configuration fs-watch, and a corrupt or oversized theme file MUST be
skipped with a warning rather than failing startup or reload.

#### Scenario: Save-as from the builder

- **WHEN** the user saves the current palette as "paperback"
- **THEN** `themes/paperback.toml` is written and "paperback" immediately
  appears in the preset namespace

#### Scenario: A corrupt theme file never takes down startup

- **WHEN** a file in the themes directory fails to parse
- **THEN** it is skipped with a warning naming the file and the remaining
  themes load normally

### Requirement: Terminal color schemes import from Gogh

thegn SHALL import terminal color schemes in Gogh YAML format
(`color_01..color_16`, `background`, `foreground`, `variant`) from a local
file via `thegn theme import <file> [--name <n>]` and from the builder overlay,
mapping the scheme onto the token palette with a pure, unit-tested converter
in `thegn-core` (a scheme's light variant MUST yield a light palette). The
import MUST run the contrast-contract audit on the mapped result and report
failing pairs as warnings, MUST cap the input file size and parse untrusted
input without panicking, and MUST funnel every color through hex parsing to
numeric channels so an imported file cannot inject terminal escape sequences.
Saved user themes use the user-theme TOML form; there is no export command.
Import is local-file only; no network access is performed.

#### Scenario: Importing a Gogh scheme

- **WHEN** the user runs `thegn theme import dracula.yml` on a Gogh-format
  file
- **THEN** a user theme is written mapping the scheme's colors onto the token
  palette and the command prints any contrast warnings for the result

#### Scenario: A light-variant scheme stays light

- **WHEN** an imported scheme declares `variant: light`
- **THEN** the mapped palette's surfaces are lighter than its text ramp

#### Scenario: A hostile file cannot reach the terminal

- **WHEN** an import file contains escape sequences or non-color garbage in
  its values
- **THEN** offending values fail hex parsing and are rejected or defaulted;
  no byte from the file is ever emitted to the terminal as-is

### Requirement: Theme selection persists to the key the configuration reads

Every surface that persists a theme selection (the builder, `thegn theme set`)
SHALL write the `[theme] preset` key — the key `ThemeConfig` deserializes —
via comment-preserving edits, and `thegn theme set <name>` SHALL work
non-interactively without external picker tools.

#### Scenario: The persisted selection survives a restart

- **WHEN** the user confirms a theme and restarts thegn
- **THEN** the selected theme is active, because the write landed on
  `[theme] preset` rather than an unread key

#### Scenario: Headless set

- **WHEN** the user runs `thegn theme set nord` with no interactive picker
  installed
- **THEN** `[theme] preset = "nord"` is written and confirmed on stdout

### Requirement: Shipped palettes satisfy a machine-checked contrast contract

thegn SHALL define a single contrast contract — a table of foreground-role ×
background-role pairs with minimum WCAG 2.x contrast ratios adapted to
terminal cells — covering every token pair the chrome composes: the readable
text tiers (`text` ≥ 4.5, `dim` ≥ 4.5, `faint` ≥ 3.0) on every standard
surface (`bg0`, `bg1`, `panel`, `panel2`, `raise`); the recessive-metadata
floor (`ghost` ≥ 3.0 on `bg0`/`bg1`/`panel`); the structural floor (`ghost2`,
`ghost3`, `border` ≥ 1.5 on `bg0`/`bg1`/`panel`); chip text (`chip_fg` ≥ 3.5
on the accent and all eight hues); hues as status text (≥ 3.0 on
`bg0`/`bg1`/`panel`/`panel2` — including a selected row); focus/accent/activity
affordances (≥ 3.0 on `bg0`/`bg1`); and selected-row copy (`text` ≥ 4.5 on the
derived
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
