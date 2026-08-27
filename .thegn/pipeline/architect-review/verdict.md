# THE-46 — architect review verdict: **APPROVED**

Branch `tg/the-46-weather`, reviewed against `.thegn/pipeline/architect/design.md`
and repo standards, **after** merging current `main` into the lane.

Revision chunks: **none.** The one real defect found was a missing one-liner and
is fixed in-lane (`b40171b6`).

---

## 1. Reconciliation with `main` (merge `b783cadb`)

The lane was behind `main` by board-access, THE-68's notify work and schema v57.
Merged and resolved:

| Conflict                                                   | Resolution                                                                                                                                                                                                                                                                     |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `crates/thegn-core/src/sandbox_cpucap.rs`                  | **Took main's.** As briefed: `f0e0a4bb` was out of lane scope, and main's board-access branch landed the _same_ `#[allow(clippy::manual_ok_err)]` on the same statement with a better comment (it names both gates). The lane's duplicate is dropped — nothing of it survives. |
| `config.rs`, `config_tests.rs`, `config_tests_coverage.rs` | Both sides were **pure additions at the same anchor** (`weather_enabled` vs `notifications_agent_attention_inbox` in the overlay struct, the `apply` fold and the env reader; the same pair in the two test fixtures). Kept both sides.                                        |
| `.thegn/pipeline/**`                                       | Kept this lane's artifacts.                                                                                                                                                                                                                                                    |

Post-merge gate: `THEGN_ALLOW_HEAVY=1 just test` → **6493 passed, 20 skipped,
exit 0**. Coverage was recorded green by chunk 5 pre-merge and my edits are
host-only, so the `thegn-core` 95% gate is unaffected.

## 2. Defect found and fixed — the reading never survived a hydration tick

`crates/thegn-host/src/run.rs`: `FrameModel::weather` is loop-owned (the weather
task pushes it, hydration never does — design §4.4, and the field's own doc
comment says exactly that), but the hydration model swap was **not carrying it**.
The carry block right above it does this for `panel.media`, `usage`,
`usage_history` and `usage_tokens` for precisely this reason.

Consequence, had it landed: hydration runs on the 2s safety tick, so a delivered
reading survived at most one tick before `next_model.weather = None` wiped it.
Recovery is the next weather poll — floored at 600s, default 1800s. The masthead
widget and the popup block would have been visible for roughly two seconds every
half hour, i.e. effectively never, with no error anywhere to explain it.

Fixed in `b40171b6`: one carry line, plus `weather` added to
`hydration_eq_ignores_non_hydration_fields` to pin the loop-owned contract that
makes the carry necessary. (`weather` is correctly absent from `hydration_eq`,
so the carry cannot trip the idle guard.)

This is the documented carry-over trap; worth noting that the design _stated_
the contract and the implementation _documented_ it — only the line was missing.

## 3. The two other items I was asked to judge

**e2e freeze — satisfied.** Design §6.4 chose _forced off_ over _pinned_, which
is the house precedent for network-backed live-numbers surfaces (`[usage]`,
`[media]`, `[model_proxy]`). `e2e_freeze::apply_to_config` sets
`cfg.weather.enabled = false` and the module doc gains the matching bullet.
Because the feature is off by default the frozen frames are unchanged, so no
baseline re-record is needed — and `preferred_cols` was deliberately written so
`weather_cols` is `0` when the block is absent, which is what keeps the calendar
popup's recorded width byte-identical. Correct on both counts.

**`sandbox_cpucap.rs` — reconciled.** See §1; the lane's out-of-scope commit is
fully superseded by main's.

## 4. Design conformance

Every invariant in design §2 holds, and every trap in §7 was actually hit:

- **0% idle** — `weather_every_slots` returns `None` when `poll_secs()` is
  `None`, so a disabled feature emits no ticker slot at all. `ticks` is
  incremented before the checks, so `WEATHER_FIRST_SLOT` cannot collide with
  tick 0 (nothing network-shaped on the launch→first-frame path).
- **Render decision** — the `Weather` arm sets `bars_dirty`, never `dirty`, and
  compares against `model.weather` first, so a cached redelivery raises no
  damage. `render_plan::a_weather_delivery_is_bars_only` pins both halves.
- **`spawn_blocking`, not `spawn_bg`** (§6.2) — done, with the silently-drops
  reasoning restated in the module doc so it survives a future tidy-up.
- **Seams** — `BoxFuture`, no `async fn`, `SeamError` classes argued (`Parse` is
  deliberately not transient), vendor knowledge confined to `wttr_in.rs`,
  `KNOWN_SEAMS += "weather"`, probe offline-by-contract and absent when disabled.
- **Cache in `ui_state`** (§6.1) — no new table, no `SCHEMA_VERSION` bump, so the
  collision trap is avoided; writes best-effort with reasons.
- **Reserved ⇒ `none`** (§6.3) — the enum-default/struct-default split is
  implemented and tested, and the unreachable `is_reserved()` probe arm carries
  the note explaining why it exists.
- **Glyphs** — eight BMP width-1 picks, ASCII fallbacks, `Glyph::ALL` pin 47→55,
  `Sky::Unknown` renders temperature alone. `config_enum!` pin 88→90 with the
  dated note. `env-overlay-ratchet` gains the nine structured keys with
  `THEGN_WEATHER_ENABLED` left as the real knob. No help-ratchet edits, because
  no new action/chord/zone was introduced — as designed.
- **Location custody** — never logged, never in an error (`reqwest`'s URL is
  stripped via `without_url`), never in a probe note; percent-encoded by
  `Url::path_segments_mut` so `../` and whitespace are data, not syntax; length-
  and newline-validated; `api_key` is SecretRef-only. A test asserts the probe
  does not leak it.
- **openspec** — the four deltas (§6.1–6.4) are folded back into
  `add-weather-widget/` rather than left to drift, including the spec scenario
  §6.3 invalidated.

## 5. Non-blocking observations (no action required)

1. **Popup width is fixed at open time.** `preferred_cols` measures the weather
   block when the popup opens; a reading that lands _while_ the popup is open is
   picked up by `retick_open` but cannot widen it, so the conditions row may clip
   on a popup opened before the first delivery. This is the same behaviour the
   agenda already has (`apply_calendar` fills rows into a popup sized before the
   fetch returned), so it is consistent rather than novel — worth a follow-up
   only if it shows up in practice.
2. **`.thegn/pipeline/**`is a shared path across lanes**, so every lane-vs-main
merge produces add/add conflicts on the chunk files and leaves another lane's`verdict.md` in the tree. A pipeline-infra concern, not this change's.

## 6. Verdict

**APPROVED.** The implementation follows the design closely, including the four
deltas it argued for, and its comments explain _why_ at the points where a future
edit would get it wrong. The one substantive defect was caught, fixed and gated
in-lane.

Commits added by this review:

- `b783cadb` — merge `main` into the lane (reconciliation above)
- `b40171b6` — `fix(weather): carry the reading across the model swap`
