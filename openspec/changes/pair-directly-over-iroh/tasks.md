# Tasks

- [ ] Add a direct Iroh control transport to `ControlClient` using explicit
      endpoint IDs and IP socket addresses only.
- [ ] Add a separate Iroh control listener that serves the real control router
      with token-only authentication and authenticated peer identity metadata.
- [ ] Expose the listener only through an explicit `thegn serve` opt-in and
      provide a direct pairing invitation flow that reuses existing scoped,
      expiring, single-use pairing codes.
- [ ] Document direct-only networking constraints, reconnect and revocation
      behavior, and how to return to existing HTTP pairing.
- [ ] Add protocol tests for valid redemption, peer ID preservation, scope
      checks, revocation, expiry, replay, wrong identity, and no implicit admin.
