# Notifications

## MODIFIED Requirements

### Requirement: Sound and bell channel

Qualifying notifications SHALL emit an audible cue per `[notifications.sound]`:
`mode = "bell"` (default) writes a terminal `BEL`, `mode = "chime"` plays a
sound file through the first available system audio player (falling back to
the terminal bell when no player or file is available, so a cue never
silently no-ops), `mode = "command"` runs a configured command, and
`mode = "off"` is silent. The effective sound for a notification SHALL be
resolved in order — a matched rule's `sound` action, then
`[notifications.sound.per_kind]`, then `[notifications.sound.per_priority]`,
then the mode default — with a single sound-spec vocabulary (`"bell"`,
`"off"`, a file path, a pack sound name, or a command) accepted at every
level. Sound MUST only fire for notifications at or above
`[notifications.sound] min_priority` (with `always_kinds` and
`suppress_focused` applying unchanged), MUST honor DND as the only quiet-hours
schedule (no second sound-specific schedule), and MUST be best-effort and off
the event loop's critical path (the terminal `BEL` is written on the next
render flush; file playback and commands spawn off-thread). A
`[notifications.sound] volume` (0.0–1.0) SHALL be applied best-effort via the
resolved player's volume flag; players without one play at their default, and
`thegn doctor` MUST report the resolved player and whether volume is honored.

#### Scenario: Bell on alert

- **WHEN** an Alert notification arrives with `mode = "bell"` and
  `min_priority = "alert"`
- **THEN** a terminal BEL is emitted on the next render flush

#### Scenario: Below threshold is silent

- **WHEN** a Notice notification arrives with `min_priority = "alert"`
- **THEN** no sound is emitted

#### Scenario: Chime falls back to the bell

- **WHEN** `mode = "chime"` resolves a file but no system audio player exists
- **THEN** the terminal bell rings instead of the cue silently no-opping

#### Scenario: Volume is best-effort

- **WHEN** `volume = 0.3` and the resolved player supports a volume flag
- **THEN** playback is attenuated; with a flag-less player it plays at
  default and `thegn doctor` reports volume as not honored

## ADDED Requirements

### Requirement: Per-event sound mapping

`[notifications.sound.per_kind]` SHALL map notification kind names to sound
specs, letting distinct events sound distinct. A per-kind entry MUST override
`per_priority` and the mode default, and MUST itself be overridden by a
matched rule's `sound` action. `per_kind` entries alter only which sound
plays: the firing gates (`min_priority`, `always_kinds`, `suppress_focused`,
DND) apply unchanged after resolution. `thegn config validate` MUST warn on
unknown kind names with a did-you-mean suggestion, and an unconfigured
`per_kind` table MUST leave behavior identical to today.

#### Scenario: Two kinds, two sounds

- **WHEN** `per_kind` maps `agent_attention` and `test_failed` to different
  files and both kinds fire
- **THEN** each plays its own file, off-thread, with all gates applied

#### Scenario: Rule still wins

- **WHEN** a matched rule sets `sound = "off"` for a kind that has a
  `per_kind` file
- **THEN** no sound plays

#### Scenario: Unknown kind name

- **WHEN** `per_kind` contains a misspelled kind
- **THEN** `thegn config validate` warns with a did-you-mean and the entry is
  ignored at runtime

### Requirement: Sound packs

`[notifications.sound] pack` SHALL name a directory whose `.wav`/`.ogg` files
map to kinds by filename (`<kind>.<ext>`), with `default.<ext>` covering
unmapped kinds — so existing filename-convention packs drop in without a
manifest. Explicit `per_kind` entries MUST override pack files, and a kind
with no pack file and no override MUST fall through to the bundled
synthesized sound. The bundled fallback SHALL remain synthesized at first use
(no recorded audio asset ships in the repo) and MAY provide distinct tones
per priority tier. Pack directories are scanned at config load/reload only —
never per event — and validation MUST warn on a missing or empty pack
directory and on pack filenames that match no kind.

#### Scenario: Pack file resolves by kind

- **WHEN** `pack` points at a directory containing `queue_landed.ogg` and a
  `queue_landed` notification qualifies
- **THEN** that file plays; kinds without a pack file use `default.<ext>` or
  the bundled sound

#### Scenario: Missing pack degrades

- **WHEN** `pack` points at a directory that does not exist
- **THEN** validation warns, and at runtime the bundled sound chain applies
  as if no pack were set
