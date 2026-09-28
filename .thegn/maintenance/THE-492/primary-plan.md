# Primary decisions — THE-492 (unblocks row 662)

Good investigation, and it found the thing that would have made a three-file fix
cosmetic: **six host call paths bypass the broker**, the raw `Option<String>` is
`Debug`/`Clone` and gets copied through core `ResolveOpts` into the MPD endpoint,
and `secret_scan` does not know the key at all. A password routed through a broker
that six callers skip is not routed.

Four decisions.

## DECISION 1 — scope expansion APPROVED beyond the three-file brief

All six bypassing call paths are in scope and must go through the broker.
**Enumerate them in your plan and state which you changed**; if one genuinely
cannot be routed, name it and why rather than leaving it silently unrouted.

Also in scope, because they are the same leak:

- The raw `Option<String>` being `Debug`/`Clone`-able and copied through
  `ResolveOpts`. A secret that derives `Debug` will end up in a log line
  eventually. Give it a type that cannot print itself.
- The `MPD_HOST=password@host` parsing in the leaf. A password hiding inside a URL
  is the same exposure as a password in a config field, and a scanner that only
  knows the field misses it entirely.

## DECISION 2 — a typed missing/unavailable broker result is APPROVED

You need it for fail-closed behaviour and it is additive. "Broker has no secret
for this account" and "broker could not be reached" are different facts and must
stay different: the first is a configuration answer, the second is infrastructure.
Conflating them turns an outage into a wrong credential decision.

**Fail closed on both** — never fall back to a plaintext value because the broker
was unavailable.

## DECISION 3 — `secret_scan` must learn BOTH forms

`media.mpd.password` **and** the `MPD_HOST=password@host` spelling. And note your
own finding that `literal_refs` excludes it — fix that too, or the scanner reports
clean on a config that is not.

The point of the scanner is that an operator **finds out** they left a plaintext
secret. A migration that makes the old form invisible is worse than leaving it in
place, because it converts a detectable problem into an undetectable one.

## DECISION 4 — THE-486 is OUT of scope, and THE-490 is NOT landed

- **THE-486's shared-scanner/migration work: do not do it.** If the scanner wants a
  shared shape to serve both, report that as a finding for THE-486 to pick up.
- **THE-490 is Backlog, not landed** — I screened it this batch and excluded it as
  too large. So there is no THE-490 behaviour to preserve; preserve the endpoint
  validation that exists **today**, and do not implement THE-490's bounding,
  deadlines or framing rewrite here. If you find yourself writing a protocol
  reader, you have left this lane.

## Constraints

- **Never log or interpolate the secret** — not in an error, not a debug line, not
  a test assertion. A refusal names the **component**, never the value. Add a test
  that the rendered error does not contain the secret.
- **Existing configs must keep working, or fail loudly.** Decide which, say which,
  and test it. Silently ignoring a previously-honoured password breaks someone's
  MPD connection with no diagnostic.
- Config-key changes trip the config-example, env-overlay and strict-validation
  gates.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the crate-wide nextest and the ratchets
centrally.

Never report a verdict for code you could not compile; state what you could not
run.
