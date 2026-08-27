# THE-46 — security / test / bug review

**Verdict: PASS** (ready for the merge queue), with four defects found and fixed
on the lane and four non-blocking observations recorded below.

Branch `tg/the-46-weather`, reviewed at `1b4eef6e` (architect: APPROVED; lane
already merged `main` at `b783cadb` and `just test` ran green post-merge —
6493/20 — so the full suite was deliberately **not** re-run). Plus the one review
commit described in §1.

---

## 0. What was reviewed

The whole `main...HEAD` diff, adversarially, with the pass weighted toward this
lane's distinctive risk: **it adds a network fetch of untrusted remote data to
the host process.** Specifically —

- provider response parsing (malformed / hostile JSON, size limits, encoding)
- URL construction and injection through config values
- cache handling (traversal, permissions)
- timeout / retry behaviour against the 0%-idle contract
- secret custody
- `e2e_freeze` pinning of the new volatile chrome
- and the standard sweep: swallowed errors, missing failure-path tests, ratchets

The headline is that **the transport layer is genuinely well built** — the URL
builder, the size guards, the error redaction and the seam wiring all survived
the attacks I aimed at them, and I have recorded the specifics in §3 so they are
not re-litigated. The defects were all one layer further in: what happens to the
provider's _text_ after it has been decoded.

---

## 1. Defects found and fixed — `crates/`

All four are in one commit on this lane.

### 1.1 [HIGH] Remote weather text escaped the popup and crashed the renderer

`decode_wttr_j1` put `weatherDesc[0].value` and `nearest_area[0].areaName[0]
.value` straight onto the snapshot with nothing but `.trim()` — unbounded, and
with control characters intact. Both strings are drawn: `place` into the popup's
`WEATHER · <place>` heading, `description` into its table. Two consequences, both
reproduced on a rendered `Surface` before fixing:

**(a) It paints outside the popup's clip rect.** `\r` and `\n` are not inert in a
`Change::Text`: termwiz's `Surface::print_text` acts on them, resetting the
column / advancing the row. With `place = "Berlin\rZAP"` the tail landed at
**column 0 of the underlying chrome**, ~50 columns left of the popup's own left
border:

```
12 "ZAP                                          │ WEATHER · B      │"
```

From the last row, `print_text` calls `scroll_screen_up()` instead — i.e. a
hostile reading scrolls the entire composed frame. Nothing repairs those cells
until the next full repaint, because the compositor's damage model assumes a
draw stays inside the rect it was given.

**(b) With an unbounded string it panics the render path.** A 4 KiB description
plus control characters aborted in `seg::draw_line`:

```
panicked at crates/thegn-host/src/seg.rs:526: attempt to subtract with overflow
```

That is a **crash of the whole compositor, driven by a third party's HTTP
response**. In release it is worse in a quieter way: `w - used` wraps and the pad
becomes `" ".repeat(~usize::MAX)`. Root cause in §1.2.

**Fix** — `thegn_core::weather::safe_text`, applied inside `first_value`, which
is the one seam both strings enter the domain model through: control characters
dropped (matching `cache_key`'s existing filter), bounded to 64 characters
(wttr.in's longest real `weatherDesc` is 43), re-trimmed. Pure, so it is covered
by the core gate rather than by a render test.

Two regression tests, and I verified both fail without the fix:

- `thegn-core weather::tests::provider_text_is_stripped_of_control_chars_and_bounded`
- `thegn-host detail::tests::a_hostile_provider_body_cannot_paint_outside_the_popup`
  — drives a hostile `j1` body through the real decode, renders, and asserts that
  **every column left of the popup's own border is untouched**. Content assertions
  would not have caught this; the geometry assertion is the contract.

Sanitizing at the decode seam (rather than at the two draw sites) is deliberate:
it is where remote data becomes domain data, it protects the SQLite cache as well
as the frame, and it keeps the guarantee in the 95%-covered pure core.

### 1.2 [MEDIUM] `seg::draw_line` pad underflow — shared chrome

The panic above is not really a weather bug. Two width models disagree about
control characters:

| function                    | measures with             | control char   |
| --------------------------- | ------------------------- | -------------- |
| `seg::take_cols` (→ `cut`)  | `UnicodeWidthChar::width` | `None` ⇒ **0** |
| `seg::cells` / `Seg::width` | `UnicodeWidthStr::width`  | **1**          |

Measured directly: `"Sunny\u{1b}[31m\u{7}\nPWNED…"` — 77 chars, 74 by per-char
width, 77 by str width. So `cut(l, w)` fits a run to `w` by its own accounting,
`seg_width` then measures that same run as _wider_ than `w`, and the three
`w - …` pad computations underflow. (termwiz sides with `cells`: it nerfs the
character to a space and gives it a cell.)

**Fix** — the three pads in `draw_line` are now `saturating_sub`, with a doc
comment stating why, plus
`seg::tests::a_control_char_run_wider_than_the_line_clips_rather_than_panicking`
covering all three `Line` variants. This is defensive only: it turns a crash into
a clip. **It does not reconcile the width models**, and that is deliberate —
choosing which model is correct is a change to shared chrome that belongs to
whoever owns `seg`, not to this lane.

**This is pre-existing and still reachable from another untrusted source.** ICS
event titles are unbounded, `calendar::ics::unescape` turns `\\n` into a real
`\n`, and they land in the same `Cell::Text` → `draw_table` → `draw_line` path
via `agenda_table`. After this fix that path clips instead of panicking, but the
titles are still unsanitized and can still be made to paint outside the popup by
the `\r` mechanism of §1.1. **Recommend a follow-up ticket**: give the ICS
decode the same `safe_text` treatment, or reconcile the width models and nerf
`\r`/`\n` at the `seg` chokepoint so no future draw site has to remember.

### 1.3 [LOW] Silent return in `hydrate_weather::poll`

`let Ok(rt) = …build() else { return };` — every other failure in that function
logs, so a runtime-build failure was the one outcome indistinguishable from "the
service was quiet". Now a `tracing::debug!`, consistent with its neighbours.

### 1.4 [LOW] Misleading `skip_net` guard on `RefreshKind::WeatherPoll`

`run.rs` wraps the weather spawn in `if !skip_net`. `connectivity_gate::
should_skip_refresh` does not list `WeatherPoll`, so the guard never fires — but
that is load-bearing, not incidental: the poll's **rule 2 is "cache first,
always"**, and gating the whole spawn on connectivity would suppress the cached
delivery too, blanking the widget on an offline machine that has a perfectly good
reading on disk. Offline is correctly decided one layer down by `should_fetch`.
Left in place with a comment saying why it must not be "fixed" by adding
`WeatherPoll` to the gate list — this is the exact shape of a future regression.

### 1.5 [trivial] Two committed artefacts fail `treefmt`

`.thegn/pipeline/{architect-review,review}/verdict.md` were committed
unformatted, which `just lint` (and therefore `just ci`) fails on. Formatted in
the same commit. See the git-hooks observation in §4.

---

## 2. Tests

Scoped runs only, per the dev-loop policy.

| gate                                                                   | result                     |
| ---------------------------------------------------------------------- | -------------------------- |
| `cargo nextest run -p thegn-core`                                      | **3402 passed**, 2 skipped |
| `cargo nextest run -p thegn-host`                                      | **2355 passed**, 7 skipped |
| `cargo nextest run -p thegn-svc`                                       | **569 passed**, 11 skipped |
| `cargo nextest --test env_overlay_coverage`                            | 2 passed                   |
| `just quick thegn-core` / `just quick thegn-host`                      | clean                      |
| ratchets (platform / color / glyph / help / async-trait / env-overlay) | green                      |

The full workspace suite was **not** re-run: the architect recorded it green at
`b783cadb` and my edits are additive plus three `saturating_sub`s. `just
coverage`, cross/MSRV and e2e are pre-PR gates and were not run here.

**Failure-path coverage is good** and I did not find a gap worth blocking on.
`should_fetch` is pure and exhaustively tested (inactive / reserved kinds /
interval boundary / floored-zero / offline). The seam tests cover URL encoding,
error classification, transient-vs-permanent, and the never-leak-the-location
property. The decode tests cover garbage JSON, empty `current_condition`,
string-vs-number fields, unparseable numbers, out-of-range humidity, undated
forecast days and the legacy cached row. What was missing was the _hostile_ case
rather than the _broken_ case — now added.

---

## 3. Attacked and clean

Recorded so these are not re-derived next time.

- **URL injection — clean.** `url_for` builds through `reqwest::Url::
path_segments_mut().push()`, which percent-encodes: a space, non-ASCII, `/`,
  `?` and a `../` traversal attempt all become data. Verified by test and by
  reading the encode set. `WTTR_IN_BASE` is a constant with no config key, so
  there is no user-supplied endpoint at all.
- **Request splitting — clean, twice over.** `validate_weather` rejects `\n`/`\r`
  in `location` and caps it at 128 chars; even bypassing validation, the
  percent-encoding makes it impossible.
- **Response size — clean.** Two-step guard: advertised `content_length` refused
  before the read, then the actual body re-checked (chunked responses carry no
  length). 1 MiB cap on a ~10 KiB payload.
- **ANSI / OSC-52 injection — clean, and worth knowing why.** I chased this to
  the wire and it does _not_ land: `Cell::new_grapheme` nerfs C0/C1 to a space,
  so ESC and BEL never reach `wire.rs`'s raw `out.push_str(t)`. `\r`/`\n` are the
  exception because `print_text` acts on them _before_ the cell is built — which
  is §1.1, and the only reason that class was live.
- **Cache — clean.** It is a SQLite `ui_state` row keyed by
  `weather::cache_key` (provider|location|units, control characters filtered),
  not a file. No provider-derived path, no traversal, no permission surface. A
  row that fails to deserialize is dropped rather than failing the pass.
- **Error redaction — clean.** `network_error` strips the URL a `reqwest::Error`
  embeds; no variant carries the location; the decode error never quotes the
  body. All three are pinned by tests.
- **Secrets — clean.** `api_key` is `SecretRef`-only (a raw literal is a
  validation _error_), is read by nothing yet, and is never logged. The doctor
  probe prints "location: as configured", never the value — with a test asserting
  the leak does not happen.
- **TLS — clean.** rustls only; `danger_accept_invalid_certs` appears nowhere in
  the tree.
- **0%-idle — clean.** Disabled ⇒ `poll_secs()` is `None` ⇒ the ticker emits no
  weather slot at all. The fetch rides `spawn_blocking` with its own
  current-thread runtime, deliberately _not_ `sched::spawn_bg` (which silently
  sheds when saturated — and the retry is 30 minutes away). Nothing
  network-shaped on the launch→first-frame path: first slot is tick 10 (5 s), and
  `ticks` is incremented before the modulo so tick 0 cannot fire. `render_plan`
  pins the delivery as `bars`-only damage, and an identical redelivery raises no
  damage at all.
- **`e2e_freeze` — clean, including the ordering.** `apply_to_config` forces
  `weather.enabled = false`, and it runs at `run.rs:573` _after_ the env overlay,
  so `THEGN_WEATHER_ENABLED=1` cannot defeat the freeze. Because the feature is
  off by default, no recorded baseline moves — pinned by
  `the_popup_has_no_weather_block_without_a_reading`, which also asserts the
  popup keeps its historical width of 44.
- **Doctor probe — clean.** Pure config read, no round trip.
- **Ratchets — clean.** The ten `weather.*` entries added to
  `test/env-overlay-ratchet.txt` are the sanctioned path for new keys (the file's
  own header requires a knob _or_ a pin); `enabled` correctly gets the knob. The
  `config_enum` pin was moved 88 → 90 with a reason. Glyph/color literals go
  through `caps`, the seam op is a `BoxFuture` not an `async fn`, and the help
  pages claim and mention the new ids.

---

## 4. Non-blocking observations

1. **Redirects are unpinned.** `reqwest`'s default policy follows up to 10
   redirects, including an https→http downgrade. Impact is low — no credentials
   are sent and the payload is public — but a `redirect::Policy::none()` (or
   same-host) on the client would remove a compromised-endpoint pivot for free.
2. **`number()` admits infinity.** `"1e40".parse::<f32>()` is `INFINITY`, and
   `fmt_temp` renders it as `9223372036854775807°C` — 21 cells in the masthead.
   Bounded in practice, since `fit_stats_cluster` sheds `weather` second, so the
   blast radius is "the weather and date widgets disappear". A `clamp` in the
   decode would be tidier.
3. **Two artefacts reached HEAD unformatted (§1.5) and I could not explain how.**
   The hooks _are_ installed and working — `pre-commit`, `pre-merge-commit` and
   `pre-push` all live in the shared `.git/hooks` and the treefmt hook fired on
   my own review commit — so the `CLAUDE.md` "pre-push is the only gate"
   assumption does hold here. The verified fact is only that
   `treefmt --fail-on-change` failed at `1b4eef6e` on those two markdown files;
   whether that was a `--no-verify` commit or something else, I did not
   establish. One plausible mechanism, which I hit myself while writing this
   file: the treefmt hook _fixes_ the file and then fails the commit, so a
   re-`commit` without a re-`add` lands the stale staged blob while the working
   tree holds the formatted one. Flagged as worth a glance, not as a diagnosis.
4. **ICS titles carry §1.2's shape.** See the follow-up recommendation there.

---

## Verdict

**PASS** — ready for the merge queue.

The lane's design and transport work are solid and the invariants the architect
signed off on all hold. The one serious defect was a class the design's own
threat model named ("untrusted remote data") but stopped one layer short of:
custody of the provider's _text_ after decoding. It is fixed at the right seam,
with regression tests that were verified to fail without the fix, and the shared
chrome behind it no longer aborts the compositor on hostile input.

`thegn integrate` is the merge step and is not mine to run.
