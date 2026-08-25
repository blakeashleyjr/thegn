# Design — per-event notification sounds

## Where the pieces already live

- **Routing (pure, core):** `notification_route.rs` resolves a
  `RouteDecision` including `Option<SoundEmit>` (`Bell` | `Chime` |
  `Command`) from mode, `min_priority`, `always_kinds`, rules, DND. This is
  where the new resolution order goes — pure logic, table-testable, under
  the 95% gate.
- **Playback (host):** `chime.rs` finds the first system player on PATH
  (cached), materializes the bundled synthesized WAV under the state dir on
  first use, and hands a command line to `notify::spawn_sound_command`
  (detached thread, best-effort, swallowed failures). `emit_sound` falls
  back to the terminal bell when no player/file exists — a cue never
  silently no-ops.

The change threads two new inputs (kind→spec map, pack dir) into the
resolver and two new outputs (a resolved file, a volume) into the player
command builder. No new processes, threads, or wake sources; the sound path
never touches the event loop (bell latch on render flush; subprocess for
files) — the wake path and render damage channels are untouched by this
change.

## Decisions

### Resolution order, stated once

First match wins, resolved in core:

1. A matched rule's `sound` action (existing — per-rule override).
2. `[notifications.sound.per_kind] <kind>` (new).
3. `[notifications.sound.per_priority] <priority>` (existing).
4. The `mode` default (`chime`/`bell`/`command`).

Gates apply after resolution, unchanged: `min_priority` + `always_kinds`
decide whether anything fires; `suppress_focused` and DND can still silence
it. This keeps exactly one gate system — the THE-35 "quiet hours" ask is
already the DND windows, and growing a parallel schedule for sounds is how a
user learns to trust neither (the `[usage.alerts]` precedent).

A sound _spec_ string is one vocabulary everywhere it appears (rule action,
per_kind, per_priority): `"bell"` | `"off"` | an absolute/`~` file path | a
bare name resolved against the pack. `per_priority` today is command-only;
it widens to the same vocabulary with commands still accepted.

### Pack convention: filenames, not manifests

`pack = "<dir>"`; `<kind>.{wav,ogg}` wins for that kind, `default.{wav,ogg}`
covers unmapped kinds, anything else in the directory is ignored. No
manifest file — the Agent-of-Empires convention, chosen because it is
already what shared CC0 packs look like and because `thegn config validate`
can lint it (unknown-kind filenames get a did-you-mean; an empty pack dir
warns). Pack scan happens at config load and on config reload, never per
event — per-event cost stays one HashMap lookup.

### Bundled sounds stay synthesized

The bundled chime is generated (sine synthesis at first use) precisely so no
binary asset ships in the repo; that property is worth keeping. The bundle
becomes a small family — a sharper two-tone for Alert, the current chime for
Notice — same synth code, different parameters, written once under the state
dir. Anyone who wants real foley sets `pack`.

### Volume via player flags, not an audio stack

**rodio (rejected):** in-process playback would give sample-accurate volume
and remove the player dependency — at the price of `cpal`→ALSA/CoreAudio C
dependencies in a compositor process, audio-device handles owned by the UI
binary, and a portability matrix thegn currently gets for free from
`paplay`/`afplay`/PowerShell. The existing shell-out is fail-safe, already
written, and its worst case (no player found) already falls back to the
bell. Dependency weight decides this: playing a ding does not justify an
audio stack. (Same shape as the viz decision in `expand-media-surfaces`:
external tool over in-process capture.)

**Chosen:** `volume` maps to the resolved player's flag —
`paplay --volume=$((v*65536))`, `pw-play --volume=v`, `afplay -v v`,
PowerShell `MediaPlayer.Volume` — built in `chime.rs` where the player table
already lives (runtime PATH detection, no `#[cfg]` spread; the platform
ratchet stays clean). A player with no volume flag (`aplay`) plays at
default; `thegn doctor` says which player resolved and whether volume is
honored, so "why is it loud" has a one-command answer.

### Accessibility

Distinct per-kind cues are the point, not polish: `agent_attention` vs
`queue_landed` vs `test_failed` become distinguishable without looking,
which serves users who are heads-down in a pane, away from the screen, or
using the terminal with a screen reader (where visually-coded badges are the
weak channel). `suppress_focused` already prevents the cue from doubling
what the user is watching.

## Security

- **No new credentials, doors, or catalog rows.** Config-driven local
  playback only.
- **Command execution surface (pre-existing, now wider-reaching):** sound
  specs can be commands (`mode = "command"`, per-priority — existing;
  per_kind inherits the vocabulary). These run via `sh -c` from the user's
  own config, which is the same trust boundary as `[[tools]]`/`[[agents]]`
  — but `add-config-trust-resolution` is scoping trust for exactly this
  class of key; per_kind command specs MUST be covered by whatever
  trust gate that change lands (noted as a soft dependency, not blocked on
  it). Pack files and `chime_file` are data paths handed to a fixed player
  argv — shell-quoted as today, no interpolation of file content.
- **Blast radius:** a malicious pack dir is at worst an annoying noise; the
  player subprocess is short-lived, unprivileged, and detached. No sandbox
  implications (playback is host-side chrome, like the existing chime).

## Open questions

- Should `per_kind` accept the calendar-reminder kind's per-account colors…
  no — but should calendar reminders (kind `calendar_reminder`) get a
  distinct bundled tone by default? Leaning no: defaults stay minimal,
  packs exist for taste.
- Whether `volume` should also scale the synthesized bundle at generation
  time (bake amplitude) as a fallback for flag-less players — cheap, but two
  volume paths can disagree; deferred unless flag-less players prove common.
- Windows player table: PowerShell `Media.SoundPlayer` is WAV-only; OGG in
  packs would silently skip on Windows. Document `.wav` as the portable
  choice, or transcode-reject at validation? Proposed: validation warns.
