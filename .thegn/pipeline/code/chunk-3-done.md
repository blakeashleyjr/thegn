# Chunk 3 — done: weather provider seam + doctor probe (`thegn-svc`)

THE-46, stage `code`, chunk 3. Branch `tg/the-46-weather`, commit `9bf7301a`.

## What landed

| File                                      | Action                                                   |
| ----------------------------------------- | -------------------------------------------------------- |
| `crates/thegn-svc/src/weather/mod.rs`     | new — error, trait, factory (~117 l)                     |
| `crates/thegn-svc/src/weather/wttr_in.rs` | new — the only file that names the vendor (~139 l)       |
| `crates/thegn-svc/src/weather/tests.rs`   | new — 4 tests, `#[path]`-included (~142 l)               |
| `crates/thegn-svc/src/lib.rs`             | edit — `pub mod weather;` (one line)                     |
| `crates/thegn-svc/src/seam/registry.rs`   | edit — `weather_probes()`, its call, module doc, +1 test |
| `crates/thegn-svc/src/conformance.rs`     | edit — `KNOWN_SEAMS += "weather"` (one line)             |

`git show --stat` is exactly these six paths — nothing outside the chunk's file
set. No new dependency.

## Public surface — design §4.3 verbatim

`WeatherError` (5 variants, `Display` + `std::error::Error` + `SeamError`) ·
`WeatherProvider` (`provider_id`, `fetch → BoxFuture`) · `provider_for(cfg,
units) -> Option<Box<dyn WeatherProvider>>`. Additions inside the module (no
frozen signature changed): `wttr_in::WttrInBackend`, `wttr_in::PROVIDER_ID`,
`wttr_in::url_for` (`pub(crate)`, so the URL builder is testable without a
client).

## Decisions inside the chunk's latitude

- **`BASE` aliases the core constant** — `const BASE: &str =
thegn_core::config_weather::WTTR_IN_BASE;`. The chunk asked for a `BASE` const
  in `wttr_in.rs` with the "no user-supplied provider URL" reasoning attached,
  and chunk 2 had already put the same literal in `config_weather`. Aliasing
  keeps the named constant and its doc comment where the chunk wants them while
  the URL _string_ exists exactly once in the workspace. Re-spelling it would
  have been two sources of truth for a value the design deliberately made
  unconfigurable.
- **`reqwest::Error` is stripped with `without_url()` before it becomes a
  `WeatherError`.** This is the one thing the chunk's approach section did not
  anticipate and it matters: reqwest's `Display` renders as `error sending
request for url (https://wttr.in/<location>?format=j1): …`, so the obvious
  `.map_err(|e| WeatherError::Network(e.to_string()))` would have put the user's
  location into every transport error and every `tracing` field derived from it
  — the exact leak trap 5 in design §7 names. One helper (`network_error`) owns
  the conversion, and `errors_never_carry_the_location` pins it by asserting no
  rendered message contains `wttr.in` either.
- **`url_for` uses `pop_if_empty().push(loc)`, not a bare `push`.** The base
  ends in `/`, i.e. one trailing _empty_ path segment; a bare push would have
  produced `https://wttr.in//Berlin`. Verified by assertion, not by reading:
  every URL case in `url_building_encodes_the_location` is an exact-string
  `assert_eq!` rather than a `contains`.
- **No caps struct, said out loud.** The seam rule is "an optional operation
  exists iff it has a caps bit"; this seam has no optional operations, so a caps
  type would be an empty struct every probe serialized as `{}`. The trait doc
  records that the omission is a decision and that a second op must bring caps
  with it.
- **The probe id comes from `wttr_in::PROVIDER_ID`**, not a `"wttr_in"` literal
  in `registry.rs`, and the registry's doc comment says "the implemented
  backend" rather than naming the service. That is what makes the containment
  criterion literally true (below).
- **`provider_for` matches every kind exhaustively** even though `is_active()`
  has already excluded `none` and the reserved ones, so a kind that graduates is
  a compile error here rather than a silent `None`.
- **The registry test also asserts the probe does not print the location** (it
  configures `"Reykjavík"` and asserts the notes do not contain it), since
  `doctor` output is the surface most likely to grow a "helpful" location line.

## Verification

- `cargo nextest run -p thegn-svc weather registry conformance` — **29/29 pass**
  (the 4 new weather tests, the new registry test, and every pre-existing
  registry/conformance test, including `assert_report_invariants` over batches
  that now contain a weather report).
- `cargo nextest run -p thegn-svc` (whole crate, lib + all integration binaries)
  — **569/569 pass, 11 skipped**.
- `cargo clippy -p thegn-svc --all-targets -- -D warnings` — **clean** (lib and
  test targets). See the note below on how this was run.
- `test/async-trait-ratchet.txt` **unchanged** and still empty — the trait
  returns `thegn_core::seam::BoxFuture`, no `async fn` and no `#[allow]`.
- **Vendor containment holds.** `grep -rn "wttr" crates/thegn-svc/src/` outside
  `weather/` returns two hits, both the _module path_
  `crate::weather::wttr_in::PROVIDER_ID` (the probe id and its assertion) — no
  base URL, no query parameter, no User-Agent, no vendor string of any kind.
- `nix fmt` applied; the pre-commit treefmt hook passed on the commit.
- Nothing in this chunk touches the network: the seam's one round trip is
  exercised through the pure URL builder and the error classification, matching
  the probe contract.

## One note for whoever runs the final gate

**`just quick thegn-svc` is still red on the same pre-existing lint chunks 1 and
2 both flagged** — `clippy::manual_ok_err` at
`crates/thegn-core/src/sandbox_cpucap.rs:297`. `cargo clippy -p thegn-svc` runs
the clippy driver over workspace path dependencies too, so `thegn-core` fails
first and `thegn-svc` is never reached. Confirmed untouched by this branch
(`git diff --name-only main...HEAD` lists no `sandbox_cpucap.rs`).

To verify this chunk anyway I applied the one-line fix (`v.parse().ok()`)
locally, ran `cargo clippy -p thegn-svc --all-targets -- -D warnings` to
completion — clean — and **reverted the file**; `git status` confirms it is
unmodified and it is not in the commit. Left alone for the same reason chunks 1
and 2 left it: it is outside the chunk's file set. It is a one-line fix and
three chunks have now paid a verification detour for it, so it is worth folding
into chunk 5 or into whatever runs `just ci`.

## Handoff notes for chunks 4–5

- **Chunk 4 builds the provider with**
  `thegn_svc::weather::provider_for(&cfg.weather, cfg.weather.resolved_units(locale))`
  and treats `None` as "weather is off" — do not re-derive the condition, and do
  not call `fetch()` without going through the factory.
- **`fetch()` already stamps `snapshot.provider`**, so the cache key is
  `thegn_core::weather::cache_key(&snapshot.provider, &cfg.weather.location,
snapshot.units)` with no literal at the call site.
- **`WeatherError::is_transient()` is the connectivity signal**: only `Network`
  is transient. A `Parse` or an `Api` must NOT flip the app to "offline" — that
  is the whole reason the classification is split this way, so don't collapse
  the arms when wiring the connectivity holder.
- **A `tracing` event about a failed fetch may carry `err.class()` and the
  provider id, and nothing else.** `WeatherError`'s `Display` is already safe
  (pinned by test), but a field like `url = %url` built at the call site would
  re-introduce the leak the seam removed.
- **Chunk 5's openspec sync:** the seam now reports under
  `openspec/specs/provider-seams`' conformance rules, and `KNOWN_SEAMS` carries
  `"weather"` — if that spec enumerates seams in prose, it needs the same row.
