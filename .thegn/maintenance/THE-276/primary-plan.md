# Primary review + greenlight — THE-276

Reviewing row 632. **APPROVED. Implement steps 1–5 of your "smallest complete
fix" as written, in the order of your implementation plan.** No escalated
questions, and the plan resolves the two things I most wanted resolved.

## What you got right, and must not lose in implementation

- **You verified the 257-frame deadlock instead of taking the issue's word for
  it**, and confirmed both defects independently remain. That was the load-bearing
  check.
- **You found six connect sites, not four.** The issue named four; you found six
  across Unix, TCP and HTTP(S) for both event feeds and pane attach. A fix applied
  to four of six is worse than no fix, because it makes the gap look closed.
  **Route every one through the single configured helper**, and say in your report
  how many you changed.
- **The core/svc split is exactly right**: the size constant and the pure byte
  validation in `thegn-core`, the tungstenite `WebSocketConfig` in `thegn-svc`.
  `thegn-core` is substrate-free — no transport type may cross that line to express
  a cap.
- **One shared decode operation called by BOTH bootstrap and pump.** This is what
  makes "one frame per message" a rule rather than a place it happens to hold, and
  it is why subscriptions and pane attach get the same guarantee for free. Do not
  let the two paths grow separate validation.
- **The bootstrap reordering is the actual deadlock fix**: validate Hello, check
  `proto`, publish the stream, start the pump, and forward only the validated
  Hello. Removing the bulk `first` vector and its awaited send loop is the point —
  a bounded channel filled before a receiver exists cannot be rescued by making
  the channel bigger.

## Two implementation notes

1. **Confirm tungstenite 0.29's size semantics before relying on
   `MAX_WIRE_PAYLOAD + 5`.** You flagged this yourself — good. `max_frame_size`
   and `max_message_size` do not always mean the same thing across versions, and
   whether the limit covers header bytes matters at exactly the boundary your
   tests will probe. Check the version in `Cargo.lock`, not the latest docs, and
   state what you found.
2. **The cursor decoder must keep the arbitrary-chunk streaming contract.** Your
   "compact no more than once per bounded input batch, reset when fully consumed"
   is the right shape. Add a test that feeds one frame **one byte at a time** —
   that is the case a cursor rewrite breaks, and it is not in your listed set.

## Confirmed constraints

- `thegn-core` is gated at **95% lines**; the new decoder logic needs its unit
  tests in the same change.
- **Deterministic constructed byte inputs only.** No real WebSocket server, no
  network, no sleeps. Your existing local-fixture test for protocol-version
  refusal may stay as it is — do not extend that pattern to the new cases.
- **Typed, specific errors.** Empty, truncated, over-cap declared payload,
  trailing bytes, and a second frame must each be distinguishable and must name
  the observation (e.g. "trailing 14 bytes after one complete frame"), not the
  category. A transport refusal that says only "invalid frame" turns a one-line
  diagnosis into an investigation.
- Oversized input must be **refused before buffering or large allocation** — the
  bound is worthless if the allocation already happened.
- THE-273 and THE-184 are related only. **Nothing from either enters this diff.**
  Daemon handling, the pane actor and other control surfaces stay unchanged.

## Validation

Attempt `nix develop --command cargo check -p thegn-core -p thegn-svc
--all-targets` and narrow `cargo nextest run -p thegn-core control_wire` /
`-p thegn-svc control`. **The pipeline sandbox mounts `/nix/store` read-only, so
this usually fails outright** — say exactly that and stop if it does. The primary
runs clippy, the full workspace nextest and smoke centrally.

Never report a verdict for code you could not compile; state what you could not
run.
