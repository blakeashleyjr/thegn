## Implementation

- [x] Add borrowed exhaustive top-level and nested intent classifier with only static early dispatch.
- [x] Share existing schema/catalog/completion formatting helpers before migration and reroot.
- [x] Add actual argv parser fixtures for all59 top-level variants, all Config/API/Automations actions, aliases, completion modes/shells and global option positions.
- [x] Add owned actual CLI fixtures for missing/malformed/FIFO config, migration/profile canaries, output structure/path/alias/help/version, closed stdout and configured counterparts.

## Acceptance and landing

- [x] Primary and independent final source review including child custody and no effective-config loader path.
- [x] Compile and run parser tests, existing relevant completion/config/API tests and actual CLI fixtures through the root-owned bounded runner.
- [x] Retain parent-binary FIFO/startup-canary counterfactuals under the same owned fixture contract.
- [x] Verify native platform limits explicitly; Unix FIFO/pipe success does not establish native Windows acceptance.
- [x] Update THE-611 delivery evidence; retain THE-505/592/607/612 scope separately.
- [x] Run required scoped native tests, strict lint, ratchets, OpenSpec validation and delivery checks for this local-only landing.
- [ ] Review and land the complete THE-611 candidate on local main with its exact receipts.
- [ ] Before opening any future PR, run `just ci` per CLAUDE.md; this pre-PR gate does not replace the local landing checks above.

Current evidence is retained in [the shared manifest](../../../docs/audits/maintenance-04-2026-09-15/manifest.json). Native coverage is Linux, including Unix FIFO/pipe controls; it does not establish native Windows execution. Segmented full/focused/corrected results and counterfactual reach limits are documented in the issue audit. Strict Clippy and final metadata checks passed; reviewed local-main landing remains pending.
