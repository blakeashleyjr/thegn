# Chunk 4 — Host data plane: fetch, cache, ticker, model (`thegn-host`)

THE-46. Read `.thegn/pipeline/architect/design.md` §2, §3, §4.4, §4.5, §6.1,
§6.2, §6.4, §7 first. The two files to study before writing anything:
`crates/thegn-host/src/hydrate_calendar.rs` (off-loop refresher shape) and
`crates/thegn-host/src/actions.rs::spawn_usage` (why this one is **not** on
`sched::spawn_bg`).

Iterate with `just quick thegn-host`.

## Scope

Everything between the provider seam and the model: the off-loop task that
reads the cache and (maybe) fetches, its ticker slot, the two `RefreshKind`
variants, the drain arms in `run.rs`, the `FrameModel` fields, and the e2e
freeze. **No rendering** — that is chunk 5.

Code against chunk 1/2/3's frozen signatures (design §4.1–§4.3).

## Files

| File                                             | Action                                              |
| ------------------------------------------------ | --------------------------------------------------- |
| `crates/thegn-host/src/hydrate_weather.rs`       | new                                                 |
| `crates/thegn-host/src/hydrate_weather_tests.rs` | new (`#[path]`-included)                            |
| `crates/thegn-host/src/hydrate.rs`               | edit — two `RefreshKind` variants + the ticker slot |
| `crates/thegn-host/src/run.rs`                   | edit — drain arms, spawn, `bars_dirty`              |
| `crates/thegn-host/src/chrome.rs`                | edit — **the two `FrameModel` fields only**         |
| `crates/thegn-host/src/e2e_freeze.rs`            | edit — force off + module doc bullet                |

**Shared file:** `chrome.rs` is also touched by chunk 5 (the
`masthead_widget` arm and `fit_stats_cluster`). Your anchor is the `FrameModel`
struct definition and its `Default`/construction sites — nothing else.

## Approach

### 1. `hydrate_weather.rs`

Module doc, in the register of `hydrate_calendar.rs`, stating the three rules
that are easy to get wrong here:

1. **Not on `sched::spawn_bg`.** That lane silently skips work when its eight
   permits are exhausted, on the assumption a periodic trigger retries
   shortly. The lane is busiest at startup — exactly when the one-shot first
   poll fires — and the retry here is _thirty minutes_ away. Use
   `tokio::task::spawn_blocking`, the same call and the same reasoning as
   `actions::spawn_usage`.
2. **Cache first, always.** The cached snapshot is delivered before any
   network work is even considered, so a cold launch paints weather with no
   request at all.
3. **A failure never touches the cache and never reaches the UI.** Last-good
   survives; the only trace is a `tracing::warn!` and `thegn doctor`.

```rust
/// Consider a weather refresh: deliver the cached snapshot, then fetch if it
/// is older than the (floored) refresh interval.
pub(crate) fn spawn_poll(
    cfg: thegn_core::config_weather::WeatherConfig,
    locale: Option<String>,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
);
```

Body, on the blocking task:

1. `if !cfg.is_active() { return; }` (belt-and-braces; the ticker already
   gates).
2. `let units = cfg.resolved_units(locale.as_deref());`
   `let key = weather::cache_key(cfg.provider.as_str(), &cfg.location, units);`
3. Cache read — `Db::open().ok()` then
   `db.get_ui_state("weather", &key)` → `serde_json::from_str::<WeatherSnapshot>`.
   On success, `deliver(&tx, &waker, snap.clone())` **immediately**, before
   any network work.
4. Freshness gate: if a cached snapshot exists and
   `now - snap.fetched_at < cfg.refresh_secs() as i64`, return. This is what
   makes a restart within the interval cost zero requests.
5. Offline gate:
   `if thegn_core::connectivity::current() == Connectivity::Offline { return; }`
   — the cached snapshot was already delivered, so an offline machine is fully
   served by step 3.
6. `let Some(p) = thegn_svc::weather::provider_for(&cfg, units) else { return };`
   Build a `tokio::runtime::Builder::new_current_thread().enable_all()` inside
   the blocking task (the `hydrate_calendar` pattern), `block_on(p.fetch())`.
7. `Ok(snap)` ⇒ `connectivity::report_success()`; write the cache
   (`let _ = db.set_ui_state("weather", &key, &json);` with a
   `// best-effort: the DB is a cache; the provider is the source of truth.`);
   `deliver(...)`.
   `Err(e)` ⇒ `if e.is_transient() { connectivity::report_failure(); }`;
   `tracing::warn!(target: "thegn::weather", provider = %..., error = %e,
"weather fetch failed — keeping the cached reading");` and **return without
   sending anything**. No status message, no toast.

Add `pub(crate) const CACHE_SCOPE: &str = "weather";` so the scope string has
one home.

`deliver` mirrors `hydrate_calendar::deliver`: send
`RefreshKind::Weather(Box::new(snap))`, and pulse the waker only when the send
succeeds (`// best-effort: the loop may already be shutting down.`).

### 2. `hydrate.rs` — channel + ticker

Add the two variants from design §4.5 to `RefreshKind`, each with the doc
comment given there (the existing variants are all documented; match that).

Ticker wiring, beside the `calendar_every` / `usage_every` code:

- a new parameter carrying `cfg.weather.poll_secs()` (an `Option<u64>`) into
  the ticker spawn, exactly as `calendar_poll_secs` is threaded;
- `let weather_every = weather_poll_secs.map(|s| (s.max(MIN_REFRESH_SECS) * 1000) / 500);`
  with the same belt-and-braces comment `calendar_every` carries;
- a new `const WEATHER_FIRST_SLOT: u64 = 10;` beside `USAGE_FIRST_SLOT`, with
  its own doc: the first poll rides a startup slot so the widget fills within
  seconds of launch rather than after a full interval — but **not** tick 0, so
  nothing network-shaped is ever on the launch path;
- the emit:
  ```rust
  if weather_every.is_some_and(|n| ticks == WEATHER_FIRST_SLOT || ticks.is_multiple_of(n)) {
      if tx.send(RefreshKind::WeatherPoll).is_err() { break; }
      wake = true;
  }
  ```

When weather is off, `weather_every` is `None` and **no slot is emitted at
all** — that is the 0%-idle contract for this feature.

### 3. `run.rs` — the drain

Two arms, beside the `RefreshKind::Usage*` arms:

```rust
RefreshKind::WeatherPoll => {
    if !skip_net {
        crate::hydrate_weather::spawn_poll(
            current_config.weather.clone(),
            crate::calendar_docs::CalendarDocs::env_locale(),
            refresh_tx.clone(),
            waker.clone(),
        );
    }
}
RefreshKind::Weather(snap) => {
    // Only a CHANGED reading repaints. A cached redelivery (every restart,
    // and every poll inside the interval) is byte-identical, and a
    // half-hourly datum must not become a half-hourly repaint source.
    if model.weather.as_ref() != Some(&*snap) {
        model.weather = Some(*snap);
        bars_dirty = true;
        // An open calendar popup carries the same reading.
        dirty |= crate::detail::retick_open(&mut bar_detail);
    }
}
```

Two things to get right:

- **`bars_dirty`, never `dirty`.** A weather delivery is the same damage class
  as the clock tick — two 1-row rects (`Damage::bars`), not a full-chrome
  recompose. Setting `dirty` here is the regression `render_plan`'s tests
  exist to catch.
- `retick_open` already re-renders an open calendar overlay from the model on
  a clock tick; reusing it means the popup picks the new reading up with no
  new plumbing. (Chunk 5 makes the popup read `model.weather`; until then this
  line is inert but correct.)

Also mirror `[weather]` into the model wherever `usage_cfg` is refreshed from
`current_config`, so a config reload reaches the widget.

### 4. `chrome.rs` — `FrameModel` fields only

Add the two fields from design §4.4 with their doc comments, plus their
entries in `FrameModel::default()` / the construction site(s). **Do not touch
`masthead_widget` or `fit_stats_cluster`** — those are chunk 5's.

### 5. `e2e_freeze.rs`

In `apply_to_config`, beside the `[usage]` line:

```rust
// Weather reaches the network and renders a live reading whose text changes
// on its own — the two things a byte-identical frame cannot survive. Off
// entirely while frozen, like `[usage]` and `[media]`.
cfg.weather.enabled = false;
```

Add the matching bullet to the module doc's "what it pins" list. Because the
feature is off by default, **no baseline changes and `just e2e-update` is not
needed.**

## Tests

`hydrate_weather_tests.rs` (pure decisions only — no network, and any DB test
must isolate `XDG_STATE_HOME`; this shell often runs inside a live thegn):

1. `an_inactive_config_never_spawns` — `spawn_poll` with a default config is a
   no-op (assert via the gate function, factored out as
   `pub(crate) fn should_fetch(cfg, cached_at, now, offline) -> bool` so it is
   testable without a runtime).
2. `a_fresh_cache_suppresses_the_fetch` — inside the interval ⇒ `false`;
   outside ⇒ `true`; no cache ⇒ `true`.
3. `offline_suppresses_the_fetch_but_not_the_delivery` — offline ⇒ `false`,
   and document in the test that the cached snapshot has already been sent.
4. `the_cache_key_round_trips` — a snapshot serialized and deserialized
   through the `ui_state` value shape is unchanged.

In `hydrate.rs`'s existing ticker tests (or a new one alongside):

5. `weather_emits_no_slot_when_disabled` — `weather_poll_secs == None` ⇒
   `weather_every == None`.
6. `a_stray_zero_interval_is_floored` — `refresh_interval_secs = 0` ⇒ the
   computed tick count corresponds to 600 s.

In `render_plan.rs`'s tests:

7. `a_weather_delivery_is_bars_only` — `Damage { bars: true, ..default }` with
   no overlays ⇒ `RenderPlan::Incremental`, never `Full`. (An assertion of the
   contract, so a future change that routes weather through `dirty` fails a
   test rather than quietly costing a full frame.)

## Done criteria

- `just quick thegn-host` clean.
- `cargo nextest run -p thegn-host weather` and
  `cargo nextest run -p thegn-host render_plan` green.
- With `[weather]` absent from config: no `WeatherPoll` is ever sent (assert
  in the ticker test), and `model.weather` stays `None`.
- No `sched::spawn_bg` call in `hydrate_weather.rs`.
- No `dirty = true` on a weather path in `run.rs`.
- `test/ignored-result-ratchet.txt` unchanged — every new `let _ =` carries a
  `// best-effort: <why>` comment.
- Nothing outside the files listed above is modified (in particular
  `chrome.rs`'s widget code is untouched).
