# Onboarding

## ADDED Requirements

### Requirement: First launch opens a skippable welcome tour

On first launch (no `onboarding_done` marker) thegn SHALL open a full-screen
welcome tour of eight chapters with progress dots. Every chapter SHALL show a
persistent, visible skip control naming `Esc` outside the footer's chord list,
SHALL be skippable, and `Esc` SHALL close the whole tour at once from any
chapter without writing any configuration. Closing the tour either way SHALL
record the outcome (`completed` or `skipped`) under `onboarding_done` so it does
not reopen. `THEGN_SKIP_ONBOARDING` SHALL suppress it. The command palette's
`setup-wizard` row SHALL reopen it.

#### Scenario: Skip everything

- **WHEN** a first-run user presses `Esc` on the welcome chapter
- **THEN** the tour closes, the config file is unchanged, and the marker reads `skipped`

#### Scenario: The skip control is on every chapter

- **WHEN** the user leaves from any chapter, the key drill included
- **THEN** the skip control is painted there and `Esc` closes the tour as skipped

#### Scenario: Skip one chapter

- **WHEN** the user changes the theme and then presses `Tab` on the Look chapter
- **THEN** the theme choice is reset to the configured one and no theme write is pending

### Requirement: The tour writes only on the finish screen, showing the diff

The tour SHALL collect choices and write nothing until the user confirms on the
finish chapter, which SHALL list every pending write as the key with its old
and new value. Writes SHALL go through `config_write`, preserve unrelated keys
and comments, and run off the event loop. A choice equal to the current
configuration SHALL produce no write.

#### Scenario: Unchanged walk-through

- **WHEN** the user presses `Enter` through every chapter without changing anything
- **THEN** the finish chapter shows no changes and saving writes nothing

#### Scenario: Default agent

- **WHEN** the user picks an installed harness that is not the current default
- **THEN** saving makes it the first `[[agents]]` entry and keeps every other entry

### Requirement: The sandbox chapter is honest about protection and availability

The sandbox chapter SHALL show what the `open`, `standard` and `sealed` tiers
allow and deny, SHALL state what no tier protects, and SHALL show each backend
in the chain with the same support state `thegn doctor` reports (ready, not
running with its remedy, not installed, unsupported), probed off the event
loop. When no backend is ready it SHALL say that sandboxed launches will be
refused.

#### Scenario: Stopped runtime

- **WHEN** docker is installed but its daemon is not running
- **THEN** the chapter shows docker as not running with the remedy, and `r` re-probes

### Requirement: The key drill uses the live keymap

The keys chapter SHALL resolve each drilled action's chord from the user's
effective keymap, SHALL accept the real chord and show its effect on a
practice diagram without creating anything, and SHALL, after three wrong
presses, name the terminal-compatibility help page.

#### Scenario: Rebound chord

- **WHEN** the user has rebound `new-worktree` to `Alt n`
- **THEN** the drill asks for `Alt-n` and accepts it

### Requirement: The tour is accessible

The tour SHALL fit an 80x24 terminal, SHALL NOT animate, and SHALL never carry
meaning in colour alone; glyphs SHALL degrade through the active glyph set.

#### Scenario: Monochrome terminal

- **WHEN** `NO_COLOR` is set
- **THEN** every status in the tour is still readable as a word or glyph
