---
id: media
title: Media
order: 12
contexts: [panel:media]
actions:
  [
    media-play-pause,
    media-next,
    media-previous,
    media-shuffle-toggle,
    media-loop-cycle,
    media-volume-up,
    media-volume-down,
    media-seek-forward,
    media-seek-back,
    media-open-panel,
    media-chapter-next,
    media-chapter-prev,
    media-fullscreen,
    media-select-playlist,
    media-select-player,
  ]
---

# Media

The optional `[media]` feature: now-playing and transport control for a
local player, without leaving the compositor. Hidden unless
`[media] enabled = true`.

- The status bar shows a now-playing badge; the [[panel]]'s **system →
  media** section is the canonical docked view. `Alt-m`, `media-open-panel`,
  or a media-badge click focuses it. Normal shows the current track and
  transport; Half adds the player/source list and detail; Full adds the queue
  and optional artwork. `j`/`k` select rows, `↵` selects a source or queue
  item, and painted transport controls use the same operations as the
  keyboard actions.
- In the section, `space` toggles play/pause, `n`/`p` skip, `s` shuffles, and
  `L` cycles repeat. Existing `media-*` actions remain palette-runnable and
  bindable; no new action IDs were added.
- Transport actions cover the usual surface — play/pause, next/previous,
  chapter skip, seek, volume, shuffle, loop, fullscreen — plus playlist
  and player pickers. All are palette-runnable, bindable in `[keybinds]`,
  and live under the `Alt-m` chord prefix by default.
- A corner video pin (`mpv --vo=tct`) pairs well with this — see
  [[drawer-and-corner]].

Spotify remains a reserved backend kind: the supported desktop route is the
existing MPRIS integration (or SMTC/AppleScript on their platforms), with no
OAuth, credentials, network client, or interactive Spotify-player adapter.
Visualization is intentionally absent because providers do not expose level
samples; a synthetic meter would be misleading and add idle wake/process cost.

See the [[config-reference]] `[media]` section for player selection and
options.
