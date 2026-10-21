## Implementation

- [x] Add typed sticky framing failures and independent hard caps.
- [x] Carry scan/body/consume offsets and bound compaction.
- [x] Share bounded reads and buffered-frame yield between all three consumers.
- [x] Run actual decoder/reader adversarial tests.
- [x] Run LSP/bridge/agent close integration tests in svc.
- [x] Revise near-cap sliding-window append after primary review; deterministic move-count regression passes.
- [x] Repeat focused svc validation after the primary revision (13 passed, including late concurrent LSP closure).
- [x] Run scoped clippy and independent adversarial review.
      Closed by the 2026-09-14 local-main landing (candidate ea807922, receipt 407e7d77; docs/audits/local-main-landing-2026-09-14.md); THE-310 is Done.
- [x] Complete coordinated landing.
      Closed by the 2026-09-14 local-main landing (candidate ea807922, receipt 407e7d77; docs/audits/local-main-landing-2026-09-14.md); THE-310 is Done.
