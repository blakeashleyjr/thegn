# Chunk 3 — Weather provider seam + doctor probe (`thegn-svc`)

THE-46. Read `.thegn/pipeline/architect/design.md` §4.3, §5, §7 first, and
`crates/thegn-core/src/seam.rs`'s module doc (the four seam rules). The
closest working model in-tree is `crates/thegn-svc/src/calendar/`, especially
`ics_url.rs` — read it before writing anything.

Iterate with `just quick thegn-svc`.

## Scope

One object-safe provider seam with one implemented backend (`wttr_in`), its
error type, its `provider_for` factory, and its `thegn doctor` probe. Vendor
strings — the base URL, the `j1` query parameter, the User-Agent — appear in
**exactly one file**.

Code against chunk 1 (`thegn_core::weather::{WeatherSnapshot, Units,
decode_wttr_j1}`) and chunk 2 (`thegn_core::config_weather::WeatherConfig`);
their signatures are frozen in design §4.1/§4.2.

## Files

| File                                      | Action                                                       |
| ----------------------------------------- | ------------------------------------------------------------ |
| `crates/thegn-svc/src/weather/mod.rs`     | new                                                          |
| `crates/thegn-svc/src/weather/wttr_in.rs` | new                                                          |
| `crates/thegn-svc/src/weather/tests.rs`   | new (`#[path]`-included, the `calendar/tests.rs` convention) |
| `crates/thegn-svc/src/lib.rs`             | edit — `pub mod weather;`                                    |
| `crates/thegn-svc/src/seam/registry.rs`   | edit — `weather_probes()` + its call in `probes()`           |
| `crates/thegn-svc/src/conformance.rs`     | edit — `KNOWN_SEAMS += "weather"`                            |

No new dependency: `reqwest` and `serde_json` are already `thegn-svc` deps.

## Approach

### 1. `weather/mod.rs`

```rust
//! Weather sources.
//!
//! House seam pattern (`thegn_core::seam`): an object-safe trait whose async
//! op returns a `BoxFuture` (never `async fn` — `test/async-trait-ratchet.txt`),
//! an error type implementing `SeamError`, and a factory that returns `None`
//! for a deactivated or reserved kind. Read-only by construction: there is
//! nothing to write to a weather service.
```

- `WeatherError` per design §4.3, with `Display` + `std::error::Error` +
  `impl thegn_core::seam::SeamError`. Classification:
  `Network → Transient`, `NotConfigured → NotConfigured`,
  `Unsupported → Unsupported`, `Api → Other`, `Parse → Other`.
  **`Parse` is deliberately not transient** — a payload we cannot read is a
  provider change, not a blip, and reporting it as transient would wrongly
  flip the whole app to "offline" (the `CalendarError::is_transient` note
  makes the same argument about a missing `.ics`).
- `WeatherProvider` trait: `provider_id()` and `fetch()` only. No caps struct
  — there are no optional operations to gate. Say so in a doc comment so the
  omission reads as a decision.
- `provider_for(cfg, units) -> Option<Box<dyn WeatherProvider>>`: `None` for
  `!cfg.is_active()`, `provider = "none"`, or a reserved kind; otherwise
  `Some(Box::new(wttr_in::WttrInBackend::new(cfg, units)))`. Mirrors
  `calendar::backend_from_account`.

### 2. `weather/wttr_in.rs` — the only file that knows wttr.in exists

```rust
/// The service base. wttr.in is HTTPS-only and there is deliberately no
/// config key for this — a user-supplied provider URL is a different feature
/// with a different threat model.
const BASE: &str = "https://wttr.in/";
/// Refuse to buffer a body larger than this (the j1 payload is ~10 KiB).
const MAX_BODY: usize = 1 << 20;
```

`fetch()`:

1. Build the URL without hand-rolling any encoding:
   ```rust
   let mut u = reqwest::Url::parse(BASE).map_err(|e| WeatherError::Api(e.to_string()))?;
   if !self.location.is_empty() {
       u.path_segments_mut()
           .map_err(|_| WeatherError::Api("bad base url".into()))?
           .push(&self.location);          // percent-encodes for us
   }
   u.query_pairs_mut().append_pair("format", "j1");
   ```
   No `?m`/`?u` unit flag — the `j1` payload carries **both** unit systems and
   chunk 1's decode selects. Note that in a comment; it is why there is no
   conversion arithmetic anywhere in this feature.
2. `reqwest::Client::builder().timeout(cfg.timeout()).user_agent(...)`. Set a
   real UA (`concat!("thegn/", env!("CARGO_PKG_VERSION"))`) — reqwest sends
   none by default and some fronting CDNs reject that.
3. Status handling, in this order: `429` ⇒ `Api("rate limited")` (wttr.in
   throttles anonymous callers; the message must say so, since the recovery is
   "wait", not "check your config"); other non-2xx ⇒ `Api(format!("HTTP {}"))`;
   transport error ⇒ `Network`.
4. Guard `content_length()` and the buffered body against `MAX_BODY` before
   and after reading (the `ics_url.rs` two-step).
5. `thegn_core::weather::decode_wttr_j1(&body, self.units, thegn_core::util::now())`,
   mapping `DecodeError` ⇒ `WeatherError::Parse`.
6. Set `snapshot.provider = "wttr_in".into()` (the decode does not know its
   own provider).

**Never put the location into an error message or a `tracing` field.** It is
the one piece of user data this feature handles. Errors carry the status code
or the transport error only.

### 3. `seam/registry.rs`

```rust
/// The weather seam. Nothing is reported while `[weather] enabled = false` —
/// an unconfigured optional feature is not a doctor finding.
fn weather_probes(cfg: &Config) -> Vec<ProbeReport> { … }
```

- `!cfg.weather.enabled` ⇒ `vec![]`.
- `provider.is_reserved()` ⇒ `vec![ProbeReport::reserved("weather", kind)]`.
  Add a short comment that config cannot currently reach this arm (a reserved
  value warns and deserializes to `none` — design §6.3); it exists for shape
  parity with the other seams and for a programmatically-built `Config`.
- `WeatherProviderKind::None` ⇒
  `Unavailable("[weather] provider = \"none\" — nothing to fetch")`.
- `WttrIn` ⇒ `Ready`, with notes: `"keyless; not probed offline"` and the
  effective location (`"location: <as configured>"` or
  `"location: inferred from request IP"`). Probes are cheap by contract —
  **no network round trip.**

Call it from `probes()` beside `calendar_probes(cfg)`, and extend the module
doc's seam list.

### 4. `conformance.rs`

Add `"weather"` to `KNOWN_SEAMS`. Without this, every conformance assertion
fails the moment a weather probe is emitted, with a message that reads like an
unrelated regression.

## Tests (`weather/tests.rs` + registry tests)

Nothing here may hit the network.

1. `provider_for_is_none_unless_configured` — disabled, `none`, and each
   reserved kind all yield `None`; an enabled `wttr_in` yields `Some` with
   `provider_id() == "wttr_in"`.
2. `url_building_encodes_the_location` — expose the URL builder as a small
   `pub(crate) fn url_for(location: &str) -> Result<String, WeatherError>` so
   it is testable without a client. Assert: empty location ⇒
   `https://wttr.in/?format=j1`; `"New York"` ⇒ `%20`, not a raw space;
   `"São Paulo"` ⇒ percent-encoded UTF-8; a location that is a path traversal
   attempt (`"../x"`) is encoded, not interpreted.
3. `errors_classify_correctly` — one assertion per `WeatherError` variant
   against `SeamError::class()`, plus `Parse` is **not** transient and
   `Network` **is**.
4. `errors_never_carry_the_location` — construct the error paths and assert
   the rendered `Display` contains no location substring.
5. Registry: extend the existing registry tests so a config with weather
   enabled produces exactly one `"weather"` report that is `Ready`, a disabled
   one produces none, and `conformance::assert_report_invariants` passes over
   the whole batch.

## Done criteria

- `just quick thegn-svc` clean.
- `cargo nextest run -p thegn-svc weather` and
  `cargo nextest run -p thegn-svc registry` green.
- `test/async-trait-ratchet.txt` unchanged (the trait uses `BoxFuture`, not
  `async fn`).
- `grep -rn "wttr" crates/thegn-svc/src/` matches only `weather/wttr_in.rs`
  (and its tests) — vendor containment.
- Nothing outside the files listed above is modified.
