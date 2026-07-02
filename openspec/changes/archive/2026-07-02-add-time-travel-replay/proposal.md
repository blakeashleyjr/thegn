# Add time-travel replay

## Summary

Record each pane's full byte stream so the user can scrub its history like a video
(play/pause, forward/back, skip idle gaps) and **search for any string that ever
appeared on screen** — including inside full-screen apps (vim/htop) where output
never reaches scrollback. Plus three smaller borrowings from `cy`: search-across-
time, vim-style registers (persisted), and alt/main screen swap.

Source design: `docs/superpowers/specs/2026-06-22-time-travel-replay-design.md`.

## Impact

- **I** (session persistence) / **AN** (audit / replay) — terminal time-travel.
- New capability `time-travel`; touches `state-db` (registers table, DB **v27**;
  optional on-disk replay logs).
- Recording ships **enabled by default** (bounded 8 MiB / 30 m per pane); the cost
  is measured as a perf delta (`just bench`) since it now rides the always-on path.
- Entry keybind **`Alt+r`** (`Ctrl+Alt+r` is already the asciinema whole-session
  `Recorder`, a distinct feature — task AN 483). New module `replay.rs`.

## Rationale

Every pane byte already funnels through one `PtyPane::feed`, the natural recording
tap. A bounded ring + periodic keyframes gives O(1)-ish seeking by re-feeding a
fresh emulator (`AlacrittyEmulator::new`) over a bounded byte slice — zero
`PaneEmulator` trait changes. Search-across-time extracts grid text by iterating
`PaneEmulator::cell(row,col).text` (styling-agnostic), never `row_text` (which
bails on any styled row), so it also finds text painted inside alt-screen apps.

## Non-goals

- cy's Janet scripting / multiplayer / node-graph model.
- Live screen-swap if alacritty's `Term` doesn't expose the inactive (retained
  main) screen through the trait — replay subsumes the need; ship the
  replay-subsumed path and defer the live toggle.
