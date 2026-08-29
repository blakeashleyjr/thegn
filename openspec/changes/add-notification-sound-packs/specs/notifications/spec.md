# Notifications

## MODIFIED Requirements

### Requirement: Sound and bell channel

Qualifying notifications SHALL resolve an audible result through
`[notifications.sound]` after the existing mute, route, DND,
focused-worktree, and priority gates. `mode = "bell"` is the default and
emits a terminal `BEL`. `mode = "chime"` is a legacy generic file mode: it
plays `chime_file` when configured and otherwise emits the terminal bell.
`mode = "command"` runs the trusted configured command off-loop, and
`mode = "off"` is silent. `always_kinds` SHALL bypass only `min_priority`;
they SHALL NOT bypass mute, DND, a rule route that excludes sound, or focused
worktree suppression. `volume` SHALL be validated as a finite value in
`0.0..=1.0` and passed as a best-effort provider hint.

File playback, command execution, provider probing, and filesystem access
MUST stay off the event loop. Missing or unreadable references, packs,
providers, unsupported formats, and playback failures SHALL produce a
best-effort diagnostic and fall back to the terminal bell where an audio file
was requested. A provider without volume support SHALL use its default
invocation and doctor SHALL report that limitation.

#### Scenario: Default bell on an alert

- **WHEN** an Alert notification qualifies with the default sound config
- **THEN** a terminal BEL is emitted through the existing coalesced latch
  without requiring an audio provider or file

#### Scenario: Below threshold is silent

- **WHEN** a Notice notification is evaluated with `min_priority = "alert"`
  and is not in `always_kinds`
- **THEN** no sound is emitted

#### Scenario: Chime without a file uses the bell

- **WHEN** `mode = "chime"` has no `chime_file`
- **THEN** the terminal bell is emitted and no synthesized or bundled file is
  created

#### Scenario: File playback falls back to the bell

- **WHEN** an eligible file reference has no provider, has an unsupported
  format, or cannot be resolved
- **THEN** a best-effort diagnostic is recorded and the terminal bell latch is
  requested

#### Scenario: Volume is best-effort

- **WHEN** `volume = 0.3` and the selected provider supports volume
- **THEN** the provider receives the hint; with a provider that does not
  support volume, playback uses its default and doctor reports unsupported

### Requirement: Per-event sound mapping

`[notifications.sound.per_kind]` SHALL map the snake_case names from
`NotificationKind::ALL` to `SoundRef` values. The map SHALL be resolved after
all audible gates and SHALL override legacy per-priority command-mode values
and the generic mode. A matched rule's `sound` action SHALL override the
per-kind map. `off`/`none` SHALL silence the kind; bell aliases and
`builtin:bell` SHALL select the terminal bell; `pack:<name>` SHALL select a
trusted pack entry; and an absolute or `~`-expanded path SHALL select a user
file. Bare pack names, relative paths, and commands SHALL be rejected for
per-kind values. Unknown kind names and malformed references SHALL be
reported by core validation with a did-you-mean suggestion when applicable.

#### Scenario: Two kinds, two sounds

- **WHEN** `per_kind` maps `agent_attention` and `test_failed` to different
  valid file references and both notifications qualify
- **THEN** each file job is enqueued independently on the bounded off-loop
  worker, subject to best-effort queue capacity

#### Scenario: Rule still wins

- **WHEN** a matched rule sets `sound = "off"` for a kind that has a
  per-kind file
- **THEN** no sound job is enqueued

#### Scenario: Unknown kind name

- **WHEN** `per_kind` contains a misspelled catalog kind
- **THEN** core validation reports the unknown name and suggests the nearest
  known kind; it does not become a new runtime event kind

### Requirement: Trusted sound packs

`[notifications.sound].pack` SHALL be an absolute or `~`-expanded trusted
directory. A `pack:<name>` reference SHALL resolve from the immutable snapshot
built at startup and configuration reload, without per-event filesystem
access. The host MAY index both a pack filename and its stem; the selected
provider SHALL decide whether the file extension is supported. Missing,
unreadable, or unsupported pack entries SHALL fall back to the terminal bell.
There SHALL be no manifest requirement, filename-derived default sound,
synthesized sound family, or recorded audio asset shipped by this change.

#### Scenario: Pack entry resolves by explicit name

- **WHEN** `pack` points to a readable directory containing `attention.wav`
  and `per_kind.agent_attention = "pack:attention"` qualifies
- **THEN** the startup/reload snapshot resolves that entry and the provider
  receives the file off-loop

#### Scenario: Missing pack degrades

- **WHEN** `pack` points to a directory that does not exist
- **THEN** doctor reports the fallback reason and an eligible pack reference
  requests the terminal bell without blocking notification routing

### Requirement: Live attention sound edge

The live `session_attention` state SHALL be observed during hydration, not
render. The first snapshot SHALL seed `(session, since)` values without sound.
Thereafter, a new session or changed `since` SHALL route one synthetic
`agent_attention` cue through `NotifyState`; cleared or removed sessions SHALL
be forgotten. This observer SHALL NOT insert a duplicate durable inbox row.

### Requirement: Sound diagnostics and boundaries

`thegn doctor` SHALL report the host sound provider id and availability,
supported formats, volume capability, selected pack path, pack entry count,
and fallback reason in its existing text and JSON provider surfaces. Missing
optional players, packs, and files SHALL be diagnostics rather than doctor
failures. THE-35 SHALL add no CLI action, control snapshot field, MCP tool,
completion slot, capability-catalog row, or SQLite state.
