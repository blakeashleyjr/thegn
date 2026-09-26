# PRIMARY AUTHORIZATION — you MAY run `cargo check` AND focused `cargo test`

The stage prompt says not to run Cargo because the primary centrally schedules
Rust validation. **The primary is granting a narrow exception**, because lanes
kept reporting `implementation-ready` for code that did not compile or whose own
new tests failed, and each round-trip costs far more than the checks would.

You are authorized to run **exactly these**, as many times as you need:

```
nix develop --command cargo check -p <crate> --all-targets
nix develop --command cargo test -p <crate> --lib <narrow-filter>
```

`--all-targets` on the check is required: the library frequently builds when the
**test** targets do not. Keep the test filter narrow (your module or your test
names) — it must not become a workspace run.

Still forbidden, and still the primary's job: `cargo build`, unfiltered
`cargo test`, `nextest --workspace`, `clippy`, `just lint`, `just test`,
`just ci`, and anything full-workspace. Do not run them.

**Your row is not finished until the check is clean AND the tests you added or
touched pass.** If you cannot get there inside your approved scope, report the
remaining failures verbatim as a blocker — that is a good outcome. Reporting
`implementation-ready` for code that does not build, or whose own tests fail,
is not.

Report what you actually ran. `rustfmt` passing is evidence of formatting only.

---

# Primary revision brief — THE-283 (round 2)

Row 587 filed two findings. **F-1 is accepted and is the primary's fault.**

## F-1 — ACCEPTED. The policy-free fallback is a security regression.

> `client.rs:1245-1252`: `reqwest::Client::new()` fallback is policy-free and
> can bypass `Policy::none()`, violating the no-redirect acceptance criterion.

The reviewer is right, and the primary invited this: my brief said "keep
construction infallible with a documented fallback to a default client". A bare
`Client::new()` drops `Policy::none()`, which is THE-280's **landed security
guarantee** — authenticated control requests must never follow a redirect. A
fallback that silently re-enables redirects is worse than a hard failure.

Required: **fail closed.** If the policy-configured client cannot be built, the
request fails with a typed error; there is no degraded client. My
"infallible with a fallback" instruction is withdrawn — take the fallibility
ripple if there is one, and if it reaches many call sites, report the count
rather than reintroducing a policy-free path.

If you keep any fallback at all, it must construct with the identical policy and
differ only in something provably irrelevant to security — and you must say what.

## F-2 — ACCEPTED as scoped

> `client.rs:1500-1554`: the replacement regression uses unrelated local clients
> rather than a real reload-owned replacement.

The reviewer also notes the config-reload owner/swap lifecycle **does not exist
on this branch**. So do not build one, and do not keep a test that implies you
tested it. Either:

- assert the property you can actually establish (two clients built from
  different effective configs are distinct, and in-flight work on the old one is
  unaffected), naming precisely that in the test name; or
- delete the misleading test and record the reload-lifecycle gap as an explicit
  follow-up.

A test whose name promises reload coverage it does not have is worse than no
test.

## Unchanged

Scope stays connection pooling plus one construction point. Deadlines and
response-body caps remain THE-273's (still Backlog) — do not add them. Leave the
local Unix/TCP and WebSocket transports alone.

Keep the reviewer's added local-vs-HttpOrigin transport regression.
