## Implementation

- [x] Define versioned metric boundaries, interval and sample counts.
- [x] Add bounded successful writer-completion accounting for async/sync paths.
- [x] Preserve earliest pending input and include dispatch in busy accounting.
- [x] Attribute hydration child work and tag resync measurements.
- [x] Follow log replacement, missing paths and detected truncation safely.

## Verification

- [x] Run isolated timing/writer tests with controlled clocks/sinks.
- [ ] Run complete host rollup/cohort integration tests.
- [x] Run log append/rotation/truncation/partial-record/starvation fixtures.
- [ ] Keep render-plan invariants and compositor equivalence tests green.
- [ ] Record controlled performance evidence and causal limits.
- [x] Run formatting and relevant noncompiling source gates.
- [ ] Run integrated host typecheck with the coordinated branch batch.
- [ ] Complete independent adversarial review and coordinated integration gate.
