# Primary review — THE-275, greenlight for implementation

Reviewing investigation row 681. Accepted: the bug is confirmed on this exact
branch (`pump_events` awaits only the broadcast receiver after Hello and never
polls socket input), the lane stayed out of THE-181 and THE-277, and you asked
the one question that actually needed me.

---

## 1. DECISION — tracing plus test-visible counters satisfies "metrics". Confirmed.

You are right that there is no control metrics exporter, and you are right not
to build one. Inventing an observability surface as a side effect of a socket
lifecycle fix is exactly the scope creep that makes a change unreviewable.

What I want instead:

- A counter of **currently active** event-stream connections and a counter of
  **rejected-at-cap** ones, held in whatever the control state already uses for
  shared state.
- Maintained by **RAII**, as you proposed — the decrement must be on `Drop`, not
  on a success path. A decrement that only runs on the clean exit is precisely
  the leak this issue is about, one level up.
- Structured tracing on admit and reject.
- Readable from tests, so the connect/close stability assertion is a real
  assertion and not an eyeball check.

That closes the criterion honestly. If an exporter arrives later it reads these.

## 2. DECISION — the read half must not become a write surface

The event stream is **read-only**. Selecting on inbound frames is required to
notice Close, EOF and errors, and that is the _only_ reason to read them.

- Handle Close, EOF and protocol/transport errors by exiting promptly.
- Answer Ping with Pong per the WebSocket contract.
- **Discard every inbound data frame.** Client data must never reach the
  subscription, the filter, or anything else. A test should assert that sending
  a text frame changes nothing observable — otherwise the next person to touch
  this reasonably assumes the channel is bidirectional.

## 3. DECISION — the cap is per-endpoint, and it holds lanes open, not closed

Bound concurrent event-stream connections. Two constraints:

- A connection refused at the cap must be refused **cleanly and observably** —
  a close with a reason, plus the rejected counter — not dropped silently.
- **Slow consumers must not stall global publication.** This already matters:
  the broadcast is bounded and the existing lag path exists for it. Whatever you
  add must keep a slow or wedged subscriber from becoming everyone's problem;
  dropping that subscriber is an acceptable answer, blocking the publisher is
  not.

Pick the cap's default conservatively and put the reasoning in a comment.

## 4. SCOPE — explicitly not yours

- **THE-277** (strict first-Hello handshake). Separate issue; do not tighten the
  handshake here.
- **THE-181** (reverse-tunnel caps). **Live lane in this same batch**, owns
  `crates/thegn-svc/src/revtunnel/`. Stay out of that directory entirely. The
  two changes will look similar — resist any urge to factor a shared admission
  helper across them in this change; that is a follow-up once both have landed
  and the shape is known.
- THE-167, THE-273, THE-203.

## 5. Tests I will hold you to

- **Close immediately after Hello releases the task, the fd and the broadcast
  receiver** — without another event having to arrive. This is the bug; it needs
  the test that names it.
- Repeated connect/close leaves the active counter at its starting value. Run
  enough iterations that a leak is unambiguous.
- An idle connection answers Ping with Pong and does not pin a dead peer.
- An inbound data frame is ignored (§2).
- A connection at the cap is refused cleanly and increments the rejected counter.
- A slow consumer does not stall publication to a healthy one.

A loopback WebSocket harness is the right shape for these, as you proposed.

## 6. Verification, and the honest limit

Attempt `cargo check -p thegn-svc --all-targets` and narrow filters for the
control module. **The pipeline sandbox mounts the Nix store read-only, so
`nix develop` will very likely fail outright** — if it does, say so in your
report and stop. Expected; you are not judged on it.

I own compilation, clippy, the suite and the land gate, centrally across the
batch. **Never report `implementation-ready` for code you could not compile.**
