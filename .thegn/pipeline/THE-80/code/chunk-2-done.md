# THE-80 — Chunk 2 completion: Background sweep (samplers/timers) + surviving-pin rewrite

**Commit:** `deb25094` — `fix(the-80): declare Background QoS on the forward sampler + bench timer, rewrite the surviving pin reasons`
**Lane:** tg/the-80-qos-sweep · ran SECOND (after chunk 1 `be6a5945`).

## What was done

1. **`crates/thegn-host/src/forward.rs`** (`tgforward`, `spawn_detector`): added
   `crate::platform::qos::set_self(crate::platform::qos::Qos::Background);` as the
   first statement of the spawned thread body, with the spec'd two-line rationale
   comment, before the `tracked`/`last` bookkeeping. Fixed-cadence sampler
   (poll → backoff between blocking container probes) — same shape as the
   proc/metrics samplers.
2. **`crates/thegn-host/src/perf.rs`** (`thegn-bench-window`, `request_stop_after`):
   same declaration as the first statement, before `std::thread::sleep(...)`.
   Bench-only one-shot timer; nothing waits on it.
3. **`test/thread-qos-ratchet.txt`**: deleted the `forward.rs` and `perf.rs`
   entries (4 remain: `db_task.rs`, `frame_writer.rs`, `loading/ticker.rs`,
   `pane_writer.rs`, byte-sorted) and rewrote the file to §3's final form — all
   per-entry reasons consolidated into the single leading `#` block (the only
   part `file_ratchet` regeneration preserves), including the db_task
   session.rs:820 / main.rs:944 await sites, the ticker's
   Background-vs-Utility tradeoff, the pane_writer keystroke-path note, and the
   metrics.rs `thegn-metrics-collect` not-an-entry note (supervisor thread
   declares, so the file-level scan passes it). Lines 1–17 of the previous
   header kept verbatim.

No behaviour change on Linux (`platform::qos::set_self` is a no-op off macOS);
no `#[cfg]` added; no other files touched.

## Verification (scoped, per dev-loop policy)

- [x] `just quick thegn-host` — clean (clippy lib/bin, 24.5s).
- [x] `cargo nextest run -p thegn-host long_lived_threads` — 1 passed; ratchet
      accepts exactly the 4 remaining entries.
- [x] `cargo nextest run -p thegn-host forward perf` — 28 passed, 0 failed.
- [x] `grep -rn 'qos::set_self' forward.rs perf.rs` → 2 hits, both
      `Qos::Background`, each the first statement of its thread body.
- [x] Regeneration round-trip: `cp` the file → `THEGN_RATCHET_UPDATE=1 cargo
test -p thegn-host long_lived_threads` (1 passed) → `diff` vs the copy —
      **byte-identical**. A `just ratchet-update` regen preserves the
      consolidated header and 4 entries unchanged. (Note: the done-criteria
      command as written — `... && git diff --exit-code test/thread-qos-ratchet.txt`
      — compares against HEAD, which necessarily differs for an uncommitted
      rewrite; the equivalent pre-commit check is the copy+diff above. Now that
      the commit exists, `git diff --exit-code test/thread-qos-ratchet.txt`
      also passes post-regen, verified via the same byte-identical result.)
- [x] No `#[cfg]` added (plain statements only).
- [x] Commit subject matches the spec exactly.

## Unverified

- **macOS effect** (`Qos::Background` → `thread_policy_set`): unverified on
  hardware — none available here; the call is compile-checked only.
- **Full gates** (`just test`, `just lint`, `just ci`) not run, per the
  lead addendum (no heavy/full-workspace builds while iterating). Clippy on
  lib/bin + the scoped ratchet/forward/perf tests are the only runs.
- **e2e**: not run (forbidden by addendum). These changes cannot alter a frame
  (no render-path touch; QoS is a no-op on Linux), so no snapshot re-record is
  expected.
- `just ratchet-update` itself (the justfile wrapper) was not invoked; the
  underlying regeneration path (`THEGN_RATCHET_UPDATE=1` test run) was, with a
  byte-identical round-trip.
