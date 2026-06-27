# Tasks

## 1. Recorder (host, Phase 1)

- [ ] 1.1 `replay.rs`: `Recording` ring + keyframe markers + dual byte/time budget;
      `feed()` tap behind `Option<Recording>` — **unit tests**: seek-to-T re-feed
      determinism, eviction drops orphaned keyframes, skip-inactivity, `parse_duration`.
- [ ] 1.2 `[replay]` config (enabled default off, budgets, intervals); assert zero
      cost / no wakeups when off.
- [ ] 1.3 Optional `persist` off-loop writer thread + resurrection load.

## 2. Replay overlay (host, Phase 2)

- [ ] 2.1 `ReplayOverlay` (scratch emulator, scrub keys) + playback clock thread
      alive only while playing (waker pulse, parks on pause) — assert 0 idle wakeups.
- [ ] 2.2 Time-search over re-fed frames on `spawn_blocking` (regex/literal/time-
      expr) — **test** finds a string that only appeared in an alt-screen app.

## 3. Registers (core/host, Phase 3)

- [ ] 3.1 `registers.rs` (pure, 95% gate) + `registers` table (DB **v16**) +
      `Action::PasteRegister` — **migration test** (v15→v16 additive).

## 4. Validate

- [ ] 4.1 Headless PTY: record → replay → scrub, isolated `XDG_STATE_HOME`/replay dir.
- [ ] 4.2 Run `just ci` (includes `openspec-validate`).
