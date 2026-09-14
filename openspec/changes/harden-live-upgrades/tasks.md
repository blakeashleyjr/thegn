# THE-613

- [x] Stage an isolated release/profiling build and validate supported inputs.
- [x] Confirm installation; refuse observable or ambiguous old process users.
- [x] Serialize the upgrade, back up state, and atomically replace the binary.
- [x] Preserve migration authority and report launch/recovery limitations.
- [x] Run private regression tests and independent adversarial review.
- [ ] Perform operator-controlled real controller startup and migration verification.

Source verification: `just test-live` passed 20/20 private tests; root and Peirce
reviewed production source, and root reviewed all tests. `test/dev-tui-plan.sh`,
strict OpenSpec validation and diff whitespace checks passed. The read-only plan
against the existing main installation selected its configured migration pin.
No real Cargo build, live replacement, migration or controller shutdown was run.

Delivery validation retains the pre-existing main failure: the active
`preserve-config-validation-severity` change is missing from the base ledger.
This change adds its own reciprocal THE-613 entries without masking that error.
