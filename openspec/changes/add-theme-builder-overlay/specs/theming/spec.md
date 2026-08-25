# Theming — theme builder deltas

## MODIFIED Requirements

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

## ADDED Requirements

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

### Requirement: Terminal color schemes import from Gogh and base16 formats

thegn SHALL import terminal color schemes in Gogh YAML format
(`color_01..color_16`, `background`, `foreground`, `variant`) and base16 YAML
format (`base00..base0F`, both classic flat and `palette:`-nested), from a
local file via `thegn theme import <file> [--name <n>]` and from the builder
overlay, mapping the scheme onto the token palette with a pure, unit-tested
converter in `thegn-core` (a scheme's light variant MUST yield a light
palette). The import MUST run the contrast-contract audit on the mapped result
and report failing pairs as warnings, MUST cap the input file size and parse
untrusted input without panicking, and MUST funnel every color through hex
parsing to numeric channels so an imported file cannot inject terminal escape
sequences. A named user theme SHALL be exportable back to the user-theme file
form via `thegn theme export <name>`. Import is local-file only; no network
access is performed.

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
