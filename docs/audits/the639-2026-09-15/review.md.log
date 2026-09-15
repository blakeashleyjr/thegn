# THE-639 standalone evidence review — 2026-09-15

Verdict: **source and evidence package accepted for root landing review**.

The standalone candidate is `8a83b1ec37366ba329a5752a5b20e7715fc24695`, based
on `3f365bfe56a7577dd8e051c56f5f0a4e81ade02e`. The source-equivalence receipt
proves that all four changed Rust paths match the source used by the actual
full05 native run at `33c0db71a3efd91a5637be47fd2d353fd628056a`:
`agent.rs`, `devcontainer_provider.rs`, `devcontainer_startup.rs`, and
`platform/devcontainer_startup_tests.rs`. No Rust path was changed while
packaging this evidence.

The original full05 receipt is preserved byte-for-byte. Its SHA-256 is
`c473ac0ace3701da34b0cc845ff1c99f32b6cfc9a62e7c93cbfd1664e85ad310` (the
receipt hash ends in `310`), and its native log SHA-256 is
`303b96ce47969ced77bf3d54598d98f70812bd861ed7e1262dbf30f68a08f25b`.
The receipt records 8,465 tests run, 8,465 passed, 26 skipped, and all 23
THE-639 positive selectors passed. This is reused existing evidence; this
review performed no build or execution.

The three isolated counterfactuals are retained with their patches, receipts,
build/admission/quota metadata, logs, and independent reviews:

* Mutant 1 injects the preclaim race. Its first semantic failure is the
  `Point::BeforeSpawnClaim` assertion at test line 1144: expected zero spawn,
  observed one. Explicit fixture settlement at line 1143 precedes the
  failure. The after-claim case and later assertions were unreached.
* Mutant 2 tests displaced custody on post-insert unwind. The injected panic
  is caught, `fixture.finish()` at line 758 returns, and the HELD/poison checks
  pass. The first failure is retired ownership being `None` at line 761.
  Registry, snapshot, destructor, and Busy assertions are unreached. This is
  an isolated production mutation under test; it does not justify an
  unconditional shipping fallback mutation claim.
* Mutant 3 injects child exit immediately after successful consuming wait.
  The first zero-stderr positive case passes. The bounded 2 MiB case reaches
  `HeldUnknown(Deadline)` and fails at line 646 after `fixture.finish()` at
  line 639. Its later session, snapshot, starts-file, second-finish, and clean
  assertions are unreached.

The package preserves the original delivery append for THE-639 and the
existing THE-640 entry. Remaining gates belong to root: offline delivery/
OpenSpec validation, final adversarial review, and any required landing checks.
No provider action, performance claim, canonical edit, Linear write, or new
native/build result is included here.
