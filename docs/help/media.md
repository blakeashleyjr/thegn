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
  media** section has the full view, and a centered control overlay opens
  from it.
- Transport actions cover the usual surface — play/pause, next/previous,
  chapter skip, seek, volume, shuffle, loop, fullscreen — plus playlist
  and player pickers. All are palette-runnable and bindable in
  `[keybinds]`.
- A corner video pin (`mpv --vo=tct`) pairs well with this — see
  [[drawer-and-corner]].

See the [[config-reference]] `[media]` section for player selection and
options.
