# Primary review + greenlight — THE-283

Reviewing row 575's investigation. **APPROVED to implement, with the scope
corrected below.**

## You caught a primary error. THE-273 is NOT landed.

My coordination brief said "THE-273 and THE-280 established the policy (both
landed)". That was wrong and you were right to refuse to proceed on it:

- **THE-280** (no-redirect) _is_ landed — `Policy::none` is there, as you found.
- **THE-273** (deadlines + response-body caps) is **Backlog**. Nothing to reuse.
  That is exactly why you see `Policy::none` and an unbounded `response.bytes()`.

Thank you for checking instead of inventing a policy to match the brief. Good
call.

## Corrected scope — connection reuse only

This lane is **connection pooling and one construction point**, nothing more:

1. Store one cloneable, policy-configured `reqwest::Client` per effective
   `HttpOrigin` config and reuse it across unary calls. `reqwest::Client` is
   internally `Arc`-shared — clone it; do not add another wrapper.
2. Carry over **exactly today's policy**: `Policy::none` and whatever timeouts
   are already set. Same behaviour, one place.
3. Rebuild only when endpoint/TLS/proxy config changes; a theme edit must not
   drop the pool.
4. Leave the Unix/TCP Hyper path and the WebSocket path alone.

## Explicitly OUT of scope — do not build THE-273 here

Do **not** add request deadlines, connect/idle timeouts, or a response-body cap.
They are THE-273's contract, they need typed error distinctions and a decision
about session-create idempotency (THE-272), and inventing defaults here would
pre-empt that design with numbers nobody reviewed.

What you **must** do instead: put the single construction point where THE-273
can later attach its policy in one edit, and say so in a comment naming
THE-273. Making that future change a one-line diff is this lane's real
contribution to it.

## On constructor fallibility

You asked whether the constructor should become fallible. Decide by what
`reqwest::Client::builder().build()` can actually fail on for this config (TLS
backend init, proxy parsing) and handle it where the config is already
validated. If making it fallible ripples into many call sites, keep construction
infallible with a documented fallback to a default client and record the
trade-off — do not thread a new error type through the CLI for this lane.

## Tests

Prove repeated same-origin requests reuse a connection (existing HTTP test seam,
no real network). Prove the redirect policy is identical to today and cannot
differ per callsite. Prove a config reload replaces the client without leaking
in-flight work.

## Scope

`crates/thegn-svc/src/control/client.rs` and its tests.
