# Configurable sound effects — per-event mapping, packs, volume

Linear: THE-35

## Why

THE-35 asks for "configurable sound effects", pointing at Agent of Empires'
sound system (per-session-transition sounds, a directory of files referenced
by name, platform players). thegn's notification sound channel already covers
more than half of that (roadmap AI 429, `openspec/specs/notifications`):
`[notifications.sound]` with `bell` / `chime` (a bundled, build-time
**synthesized** WAV — no binary asset shipped — or a custom `chime_file`) /
`command` / `off` modes, `min_priority`, `always_kinds`, `suppress_focused`,
per-priority command overrides, per-rule `sound` overrides, and DND/quiet
windows. Playback already shells out to the first available system player
(`paplay`/`aplay`/`afplay`/…) on a detached thread, best-effort, with a
terminal-bell fallback — never an in-process audio stack.

What "configurable sound effects" still means on top of that: **different
events sounding different.** Today every kind that qualifies plays the same
chime; there is no event→sound mapping, no sound-pack convention, and no
volume control. The accessibility case is real: a user who is heads-down in a
pane (or away from the screen) can learn to distinguish "agent needs you"
from "queue landed" from "tests failed" by ear — that is information the
single chime cannot carry.

## What Changes

All strictly additive to the existing channel — an unconfigured setup behaves
byte-identically, and no audio system is ever initialized in-process (zero
default overhead; playback remains a short-lived subprocess).

- **Per-kind mapping: `[notifications.sound.per_kind]`.** Maps a
  `NotificationKind` name to a sound spec — `"bell"` | `"off"` | a file path
  | a bare pack-sound name (see packs). Unknown kind names warn at
  validation with a did-you-mean. Resolution order becomes (first match
  wins): rule `sound` action → `per_kind` → `per_priority` → the mode
  default; all existing gates (`min_priority`, `always_kinds`,
  `suppress_focused`, DND) apply unchanged after resolution.
- **Sound packs: `[notifications.sound] pack = "<dir>"`.** A directory of
  `.wav`/`.ogg` files where a file named `<kind>.<ext>` (e.g.
  `agent_attention.ogg`) is that kind's sound and `default.<ext>` covers the
  rest — the Agent-of-Empires convention, so existing CC0 packs drop in.
  `per_kind` entries override pack files; a missing pack file falls through
  to the bundled chime. The bundled fallback grows into a small synthesized
  family — distinct tones per priority tier (alert / notice), still
  generated at first use, still no binary asset in the repo.
- **Volume: `[notifications.sound] volume = 0.0..=1.0`** (default 1.0),
  passed to players that support it (`paplay --volume`, `pw-play --volume`,
  `afplay -v`, the PowerShell player); players without a volume flag play
  at their default — documented best-effort. The terminal bell is unaffected.
- **Quiet hours: nothing new.** `[notifications.dnd]` windows already
  silence the sound channel below `allow_priority`; the spec states
  explicitly that sound effects compose with DND rather than growing a
  second schedule.
- **Doctor:** the existing chime probe extends to report the resolved
  player, whether it supports volume, and the pack directory resolution
  (found / missing / empty).

## Non-goals

- **An in-process audio stack (rodio/cpal).** Argued and rejected in
  design.md — dependency weight and device-lifetime cost for zero functional
  gain over the existing subprocess players.
- **Shipping recorded sound assets.** The bundled sounds stay synthesized;
  users who want real foley point `pack` at any directory of files.
- **Per-worktree or per-profile sound schemes.** `[[notifications.rules]]`
  (worktree glob → `sound`) and profile overlays already compose to this;
  no new mechanism.
- **Sounds for non-notification events** (keypress clicks, UI whooshes).
  The sound channel rides the notification bus only.

## Impact

- Roadmap: extends **AI 429** (sound/bell config — landed); the audit phase
  wires THE-35 in beside it.
- Specs: `notifications` — MODIFIED `Sound and bell channel` (chime mode —
  shipped but never specced — plus volume and the resolution order), ADDED
  `Per-event sound mapping` and `Sound packs`.
- Code (indicative): `thegn-core/src/notification_route.rs` (resolution
  order — pure, coverage-gated), `config_notifications.rs` (`per_kind`,
  `pack`, `volume` + validation), `thegn-host/src/chime.rs` (pack
  resolution, volume flags, synthesized family), `cmd/doctor.rs` (probe).
- Capability catalog: no new rows (no externally invokable operation). No
  SQLite change. No new dependencies. No e2e impact (sound is invisible;
  nothing under `THEGN_E2E` changes).
- Help: the page claiming the notification actions gains the per-kind/pack/
  volume prose (help-prose ratchet identifies the page).
- In-flight overlap: `add-osc-attention-signaling` emits attention via OSC —
  orthogonal channel, same bus; no shared config keys. None other.
