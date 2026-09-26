# Primary coordination brief — THE-283

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

`ControlClient` stores only the address and calls `reqwest::Client::new()` per
request, so every remote HTTPS call repeats DNS + TCP + TLS. The file's
"one connection per request" rationale applied to the local daemon socket and no
longer holds for remote origins. It also means redirect/timeout/body-limit
policy is set per callsite instead of once.

## Primary decisions

- **One cloneable, policy-configured client per effective control config**,
  reused across unary calls. `reqwest::Client` is already internally
  `Arc`-shared — clone it, do not wrap it in another layer.
- **Centralize the policy** that THE-280 and THE-273 established (both landed):
  no-redirect, connect/request deadlines, body limits. Find those and reuse
  them; do not invent a second policy or relax either. A callsite must not be
  able to differ.
- **Rebuild only on relevant config change** (endpoint/TLS/proxy). A theme edit
  must not drop the connection pool.
- **Leave the local Unix/TCP daemon transport alone** unless you can show it
  benefits; if you do change it, justify it explicitly. The local path is not
  what this issue is about.

## Tests

Prove connection reuse for repeated same-origin requests (the crate's existing
HTTP test seam — no real network). Prove policy cannot differ by callsite.
Prove a config reload replaces the client without leaking in-flight work.

## Scope

`crates/thegn-svc/src/control/client.rs` and its tests.
