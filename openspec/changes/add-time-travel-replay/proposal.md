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
- New capability `time-travel`; touches `state-db` (registers table, v16; optional
  on-disk replay logs).

## Rationale

Every pane byte already funnels through one `PtyPane::feed`, the natural recording
tap. A bounded ring + periodic keyframes gives O(1)-ish seeking by re-feeding a
fresh emulator over a bounded byte slice — zero `PaneEmulator` trait changes.

## Non-goals

- cy's Janet scripting / multiplayer / node-graph model.
- Live screen-swap if vt100 0.16 doesn't expose the inactive screen (replay
  subsumes the need).
