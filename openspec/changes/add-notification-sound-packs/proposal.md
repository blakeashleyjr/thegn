# Configurable notification sound effects

Linear: THE-35

## Why

thegn already has one notification route and a sound channel, but the old
configuration described a synthesized chime and the host implementation had
no safe way to select different user-provided effects for different events.
Users need to distinguish events such as `agent_attention`, `agent_done`,
`queue_landed`, and `test_failed` without adding another notification bus or
making audio a compositor dependency.

## What changed

The existing `[notifications.sound]` configuration now has a bell default and
an opt-in `[notifications.sound.per_kind]` map. The pure core accepts one
restricted `SoundRef` vocabulary:

- `off`/`none` disables a sound;
- `bell`/`terminal` and `builtin:bell` select the terminal bell;
- `pack:<name>` selects a named entry from the trusted pack directory; and
- an absolute or `~`-expanded path selects a user-provided file.

Bare pack names, relative paths, and commands are rejected for `per_kind`.
Legacy rule `sound`, `command`, and `per_priority` command-mode behavior stays
available as a compatibility boundary. Resolution applies the existing mute,
route, DND, focus, and priority gates before selecting a rule override,
per-kind reference, legacy priority command, or generic mode.

The host owns pack/file inspection and the platform provider. It builds an
immutable pack/file snapshot at startup and reload, resolves references
without per-event filesystem access, and sends file or command jobs through a
bounded `notify-sound` utility worker. Providers use fixed argv vectors and
report their supported formats and volume capability. Missing references,
packs, providers, unsupported formats, and playback failures degrade to a
terminal-bell latch with best-effort diagnostics.

The live `session_attention` state is observed at hydration edges: the first
snapshot establishes a baseline, and only new or changed sessions route an
`agent_attention` cue. This does not insert another inbox row.

`thegn doctor` reports the sound provider, availability, formats, volume
support, selected pack, entry count, and fallback reason in its existing
provider report. No new command, control field, MCP tool, completion slot,
capability-catalog row, database table, or bundled audio file is added.

## Pruned draft claims

The initial draft proposed bare pack names, filename-convention `default.*`
fallbacks, a synthesized family of built-in tones, filesystem validation in
the core, and a widened command vocabulary for per-kind entries. Those claims
were removed: pack-vs-path syntax is explicit, core remains substrate-free,
the only built-in sound is the terminal bell, and per-kind values cannot run
commands. The host provider is platform-owned and fixed-argv; it does not
shell-quote or execute pack paths.

The draft also proposed a second event-bus sink, new SQLite state, a sound CLI
action, and control/capability surface. Those are not part of THE-35: the
existing `NotifyState` route remains the single emission funnel, SQLite stays
a best-effort notification cache, and existing live attention state supplies
the edge trigger.

## Impact

- Roadmap: THE-35 extends the existing notification sound configuration.
- Core: `SoundRef` parsing, config validation, and route precedence remain
  pure and use `NotificationKind::ALL` as the only event catalog.
- Host: platform provider, bounded queue, immutable snapshots, bell fallback,
  live-attention edge observation, and doctor presentation.
- Docs: the configuration example, notifications help, and this openspec
  change describe the same accepted values and trust boundary.
- No schema, database, command, completion, capability, or e2e surface change.

The notification route, priority/DND/focus gates, command worker boundary,
terminal-bell latch, and live attention state were already present or are now
implemented on this branch; this document records their final contract.
