## Implementation
- [x] Add borrowed exhaustive top-level and nested intent classifier with only static early dispatch.
- [x] Share existing schema/catalog/completion formatting helpers before migration and reroot.
- [x] Add actual argv parser fixtures for all59 top-level variants, all Config/API/Automations actions, aliases, completion modes/shells and global option positions.
- [x] Add owned actual CLI fixtures for missing/malformed/FIFO config, migration/profile canaries, output structure/path/alias/help/version, closed stdout and configured counterparts.

## Acceptance (pending execution)
- [ ] Primary and independent final source review including child custody and no effective-config loader path.
- [ ] Compile and run parser tests, existing relevant completion/config/API tests and actual CLI fixtures through the root-owned bounded runner.
- [ ] Retain parent-binary FIFO/startup-canary counterfactuals under the same owned fixture contract.
- [ ] Verify native platform limits explicitly; Unix FIFO/pipe success does not establish native Windows acceptance.
- [ ] Update THE-611 delivery evidence; retain THE-505/592/607/612 scope separately.
- [ ] Run required scoped native tests, strict lint, ratchets, OpenSpec validation and delivery checks for this local-only landing.
- [ ] Before opening any future PR, run `just ci` per CLAUDE.md; this pre-PR gate does not replace the local landing checks above.
