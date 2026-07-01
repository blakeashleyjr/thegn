# Tasks

## 1. Recorder (host, Phase 1)

- [x] 1.1 `replay.rs`: `Recording` ring + keyframe markers + dual byte/time budget;
      `feed()` tap behind `Option<Recording>` — **unit tests**: seek-to-T re-feed
      determinism, eviction drops orphaned keyframes, skip-inactivity, `parse_duration`.
- [x] 1.2 `[replay]` config (enabled default **on**, numeric budgets/intervals —
      `max_bytes_per_pane`/`max_duration_secs`/`keyframe_interval_ms|bytes`/
      `idle_threshold_ms`); `None` when off (single null check in `feed`).
- [ ] 1.3 Optional `persist` off-loop writer thread + resurrection load. _(Deferred —
      off by default; in-memory recording delivers the feature. Follow-up.)_

## 2. Replay overlay (host, Phase 2)

- [x] 2.1 `ReplayOverlay` (scratch emulator, scrub keys) + `PlaybackClock` thread
      that pulses the waker only while playing and parks (condvar, zero CPU) on
      pause/exit — 0 idle wakeups when paused/closed.
- [x] 2.2 Time-search over re-fed frames (literal, case-insensitive + time-expr
      jump), grid text via `cell().text` iteration (not `row_text`) + `Action::
      EnterReplay` bound to **`Alt+r`** — **test** finds a string that only
      appeared in an alt-screen app (`search_finds_alt_screen_string_never_in_scrollback`).
      _(Bounded + Enter-triggered ⇒ runs inline; spawn_blocking is a future
      optimization for very large persisted logs.)_

## 3. Registers (core/host, Phase 3)

- [x] 3.1 `registers.rs` (pure, coverage-gated) + `registers` table (DB **v27**) +
      `clipboard::paste()` + `Action::PasteRegister` (armed next-key paste,
      honoring `bracketed_paste`; `"+` = live clipboard) + copy populates the
      persisted default register — **migration test** (v26→v27 additive).
      _(Named-register YANK arming via a `"`-prefix in copy mode is deferred — the
      `"` key must stay pane-bound in the insert-like default; store/DB/paste
      layers already support named registers.)_

## 4. Screen swap (Phase 4)

- [x] 4.1 Replay-subsumed screen swap: scrubbing back to before a full-screen app
      launched shows the prior main screen (free, delivered by Phases 1–2). Live
      `s` toggle deferred — alacritty's `Term` doesn't expose the inactive
      (retained main) screen through the `PaneEmulator` trait, and replay already
      answers the need, so we don't fork the emulator.

## 5. Validate

- [x] 5.1 Unit coverage for record → reconstruct/seek → search → overlay scrub
      (replay/replay_overlay/registers tests, isolated). Full headless-PTY e2e
      through the compositor is a follow-up on the smoke harness.
- [ ] 5.2 Run `just ci` (fmt + lint + build + test + `openspec-validate` +
      coverage + smoke + nix-build).
