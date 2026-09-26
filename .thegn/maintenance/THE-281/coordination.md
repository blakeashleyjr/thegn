# Primary coordination brief — THE-281

## PRIMARY AUTHORIZATION — you MAY run `cargo check` and focused `cargo test`

The stage prompt says not to run Cargo because the primary centrally schedules
Rust validation. **The primary grants a narrow exception**, because the previous
batch repeatedly returned `implementation-ready` for code that did not compile,
and each round-trip costs far more than the checks would.

Authorized, as often as you need:

```
nix develop --command cargo check -p <crate> --all-targets
nix develop --command cargo test -p <crate> --lib <narrow-filter>
```

`--all-targets` is required — the library often builds when the **test** targets
do not. Keep the test filter narrow; it must not become a workspace run.

Still forbidden, and still the primary's job: `cargo build`, unfiltered
`cargo test`, `nextest --workspace`, clippy, `just lint`, `just test`, `just ci`,
and anything full-workspace.

**Your row is not finished until the check is clean and the tests you added or
touched pass.** If you cannot get there inside your approved scope, report the
remaining failures verbatim as a blocker — that is a good outcome. Reporting
`implementation-ready` for code that does not build is not. `rustfmt` passing is
evidence of formatting only.

## Line numbers in the issue are STALE

Every citation comes from audit commit `299fc13`, not current `main`. The
previous batch found three issues whose headline defect was already fixed and
two whose file inventory was wrong. **Re-verify each citation on this branch and
report already-met criteria as met, with evidence, rather than re-fixing them.**

---

## The defect

`crates/thegn-svc/src/control/client.rs` builds a Host/authority for WebSocket
upgrades by taking `Url::host_str()` and bracketing any host containing `:`.
For IPv6, `host_str()` is **already bracketed**, so this produces `[[::1]]:8443`
— invalid. Unary HTTP works (it uses the URL directly), so only event/attach
upgrades break.

## Primary decisions

- **Derive the authority from the typed URL, not string heuristics.** Use
  `Url::host()` (the `Host` enum) or the http crate's authority type and format
  IPv6 through it. A `contains(':')` test is the bug; do not reimplement it more
  carefully.
- **Port rule:** append the port only when it is explicit / non-default for the
  scheme. `wss`/`https` default 443, `ws`/`http` default 80. Do not emit
  `:443` for a default-port wss origin.
- Unary and WebSocket transports must target the **same** endpoint; add an
  assertion covering that, since the divergence is what hid this.

## Tests

`https://[::1]:8443`, `wss://[::1]` (default port), IPv4 with and without port,
DNS with and without port, and a scoped/zone-id IPv6 form if the URL type
accepts one. Assert the exact authority string.

## Scope

`crates/thegn-svc/src/control/client.rs` and its tests. Do not change the
control protocol or add a dependency.
