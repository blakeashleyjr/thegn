# Chunk 4 — done: host data plane (fetch, cache, ticker, model)

THE-46, stage `code`, chunk 4. Branch `tg/the-46-weather`, commit `123669c3`.

## What landed

| File                                             | Action                                                                    |
| ------------------------------------------------ | ------------------------------------------------------------------------- |
| `crates/thegn-host/src/hydrate_weather.rs`       | new — the off-loop pass (~205 l)                                          |
| `crates/thegn-host/src/hydrate_weather_tests.rs` | new — 4 tests, `#[path]`-included (~133 l)                                |
| `crates/thegn-host/src/hydrate.rs`               | edit — 2 `RefreshKind` variants, slot const + helper, ticker param & emit |
| `crates/thegn-host/src/hydrate_tests.rs`         | edit — the 2 ticker-slot tests                                            |
| `crates/thegn-host/src/run.rs`                   | edit — 2 drain arms, ticker arg, 3 `weather_cfg` mirrors                  |
| `crates/thegn-host/src/chrome.rs`                | edit — the 2 `FrameModel` fields, nothing else                            |
| `crates/thegn-host/src/render_plan.rs`           | edit — `a_weather_delivery_is_bars_only`                                  |
| `crates/thegn-host/src/e2e_freeze.rs`            | edit — forced off + module-doc bullet                                     |
| `crates/thegn-host/src/main.rs`                  | edit — `mod hydrate_weather;` (one line)                                  |

`git show --stat` is exactly these nine paths. No new dependency.
`main.rs` is the only file outside the chunk's list: the crate declares its
modules there (`mod hydrate_calendar;` &c.), so a new module cannot exist
without it. One alphabetically-placed line.

## Done criteria

- `just quick thegn-host` — **clean** (see the note below on how it was run).
- `cargo clippy -p thegn-host --all-targets -- -D warnings` — **clean**, so the
  new test file is linted too (`just quick` covers lib/bin only).
- `cargo nextest run -p thegn-host weather` — **6/6 pass**;
  `… render_plan` — **27/27**; the whole crate — **2332/2332, 7 skipped**.
- **`[weather]` absent ⇒ nothing happens.** `WeatherConfig::default().poll_secs()`
  is `None` ⇒ `weather_every_slots(None)` is `None` ⇒ the emit guard is
  `false` at every tick including `WEATHER_FIRST_SLOT`, so no `WeatherPoll` is
  ever sent and `model.weather` stays `None`. Asserted in
  `weather_emits_no_slot_when_disabled`, over the guard the ticker actually runs.
- **No `sched::spawn_bg` in `hydrate_weather.rs`** — `tokio::task::spawn_blocking`,
  with the reasoning in the module doc.
- **No `dirty = true` on a weather path in `run.rs`** — the delivery sets
  `bars_dirty`; the only `dirty` touched is `dirty |= retick_open(…)`, which is
  the pre-existing open-overlay re-render, identical to the `ClockTick` arm.
- **`test/ignored-result-ratchet.txt` unchanged** — see below.
- Shell ratchets re-run and clean: `ignored-result` (323 pinned), `forge-leak`,
  `async-trait`, `element`. Rust ratchets: `cargo nextest run -p thegn-host
ratchet` — 12/12 (glyph literals, color literals, platform cfgs, host keys,
  caret covers, help).
- `nix fmt` applied; the pre-commit treefmt hook passed on the commit.

## Decisions inside the chunk's latitude

- **No `let _ =` anywhere in the new file, so the ignored-result ratchet is
  genuinely unchanged.** That ratchet is a _file-level grep_
  (`let _ = |let _ =[[:space:]]*$|\.ok\(\);`) — a `// best-effort:` comment does
  not exempt a line, so writing the chunk's literal `let _ = db.set_ui_state(…)`
  and `let _ = waker.wake()` would have added `hydrate_weather.rs` to a
  shrink-only list, i.e. the opposite of the done criterion. Each of the three
  sites is instead handled one notch better than ignoring:
  - cache write ⇒ `if let Err(e) = … { tracing::debug!(…) }`
  - cache read / `Db::open` ⇒ `match` with a `tracing::debug!` on the error arm
    (an open failure logs "polling without it" and the pass continues; the cache
    is an accelerator, not a precondition)
  - waker pulse ⇒ `if let Err(e) = waker.wake() { tracing::debug!(…) }`, keeping
    the "best-effort: the loop may already be shutting down" comment.
    The semantics are identical — nothing propagates, nothing takes down the
    compositor — and a diagnosable failure now leaves a trace at `debug`.
- **`weather_every_slots(Option<u64>) -> Option<u64>` is a named function**, not
  an inline `.map()` in the ticker body. Tests 5 and 6 assert on the derived
  slot count, and the ticker's locals are unreachable from `hydrate_tests.rs`.
  The precedent is `ci_refresh::ci_every_slots` / `remote_poll::fetch_every_slots`,
  which exist for the same reason. The floor comment moved onto the function.
- **`WEATHER_FIRST_SLOT = 10`** (5 s), as specced — after `USAGE_FIRST_SLOT` (8)
  and `STARTUP_FETCH_SLOT` (6), so the three startup one-shots don't land on the
  same tick.
- **`poll()` is split from `spawn_poll()`** so the blocking body reads as a
  function rather than a closure, and `provider_id` is captured before
  `block_on` so the error arm doesn't re-borrow the boxed provider.
- **`should_fetch` orders the gates cache-freshness-then-offline.** Either order
  gives the same answer; this one makes the "a fresh cache costs zero requests
  even online" rule the first thing you read.
- **The cache key is derived from `cfg.provider.as_str()`, once**, and reused for
  the read and the write. Chunk 3's handoff suggests keying the write off
  `snapshot.provider`; they are the same string by construction (`fetch()` stamps
  the provider that `provider_for` selected from this same config), and using one
  key for both halves makes a read/write mismatch impossible rather than merely
  unlikely.

## Things worth knowing for chunk 5

- **`skip_net` is inert for `WeatherPoll` today.** The drain arm carries the
  specced `if !skip_net` guard, but `connectivity_gate::should_skip_refresh`
  gates only `Pr`/`PrQueue`/`Issues`/`Ci{force:false}`/`AutoFetch`, so the guard
  never fires. That is correct and should stay that way: `WeatherPoll` must run
  while offline, because delivering the _cached_ reading is the offline story —
  the fetch itself is suppressed inside `should_fetch`. **Do not add
  `WeatherPoll` to `should_skip_refresh`**; it would silently kill the cold-start
  paint on an offline machine.
- **`model.weather` is set only from the drain arm** and only on a change, so it
  survives hydration model swaps (loop-owned, like `stats`/`usage`). The widget
  can read it unconditionally.
- **`model.weather_cfg` is mirrored at three sites** (`run.rs` startup ~747,
  the hydrate-apply block ~9182, and the live config-reload block ~10208) —
  the same three the `usage_cfg` precedent uses. A fourth mirror site added
  later needs the weather line too.
- **`retick_open` is already wired** into the weather arm, so once the popup
  reads `model.weather` (chunk 5) an open calendar picks a new reading up with
  no further plumbing. The line is inert until then, but correct.
- **The reading is delivered twice per cold poll** (cache, then fetch) and the
  two differ at least in `fetch_at`, so the widget will paint twice at launch.
  That is by design — it is what makes weather appear instantly — and the
  change-comparison keeps a warm poll to zero repaints.
- **Hard expiry is not applied yet.** `FrameModel::weather`'s doc says `None`
  "once hard-expired", but nothing in this chunk drops an expired snapshot: the
  cache is delivered whatever its age. Chunk 5 owns that, via
  `weather::freshness(snap.fetched_at, now, cfg.stale_after_secs,
cfg.hard_expiry_secs)` at the draw site (which is also where dimming lives) —
  keeping it at render time means the popup and the widget can never disagree,
  and no timer is needed to make a reading disappear.

## One note for whoever runs the final gate

**`just quick <crate>` is still red on the same pre-existing lint chunks 1–3 all
flagged** — `clippy::manual_ok_err` at `crates/thegn-core/src/sandbox_cpucap.rs:297`.
`cargo clippy -p thegn-host` runs the driver over workspace path dependencies, so
`thegn-core` fails first and `thegn-host` is never reached. Confirmed untouched by
this branch (`git diff --name-only main...HEAD` lists no `sandbox_cpucap.rs`).

As chunk 3 did, I applied the one-line fix (`return v.parse().ok();`, comment
kept above it) locally, ran `just quick thegn-host`, `cargo clippy -p thegn-host
--all-targets -- -D warnings` and the test suite to completion — all clean — and
**restored the file**; `git status` confirms it is unmodified and it is not in
the commit. This is the fourth chunk to pay the same detour: it is a one-line
fix and it belongs in chunk 5 or in whatever runs `just ci`.

**The catch, verified:** clippy's own suggestion is `return v.parse().ok();`, and
`sandbox_cpucap.rs` is **not** in `test/ignored-result-ratchet.txt` — so the
literal fix trades a clippy error for a _new_ ratchet violation. It is not an
ignored `Result` in any real sense (the `None` is the answer, not a swallowed
error), so the honest resolutions are either an
`#[allow(clippy::manual_ok_err)]` with a one-line reason, or a rewrite whose text
doesn't end in `.ok();` — e.g. binding it (`let parsed = v.parse().ok(); return
parsed;`) reads worse, so the `#[allow]` is probably the right call. Don't
discover this at the end of a `just ci` run.
