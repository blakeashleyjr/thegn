# Independent final workspace receipt review

September 14, 2026. **The completed full-workspace receipt is internally consistent and passes the configured test coverage checks.** No Cargo or test execution was performed by this reviewer; the audit parsed existing raw logs, checked the JSON receipt and read the current justfile/source delta.

| Check                | Independently verified result                                                                                |
| -------------------- | ------------------------------------------------------------------------------------------------------------ |
| Full3 source receipt | `5b698dc4`                                                                                                   |
| Raw execution        | 8,354 PASS lines, 8,354 unique test names, zero FAIL lines                                                   |
| Final summary        | 8,354 tests run, 8,354 passed, 26 configured skips; 185.563 seconds execution                                |
| Receipt list         | Exactly matches the raw unique PASS set; no duplicate names                                                  |
| Full2 comparison     | 8,348 unique passes plus six unique failures; same 8,354 selected names as full3                             |
| Former failures      | All six receipt-listed failures now appear as full3 passes                                                   |
| Python preflight     | `test/live_test.py`: 21 tests, OK, 2.180 seconds; `test/build_metadata_test.py`: 3 tests, OK, 22.092 seconds |

The second log prints each failed test again after its final summary. It therefore has 12 FAIL lines representing **six** distinct failed tests, not 12 failures. The comparison deduplicated names and did not use line count as a substitute for test identity.

Receipt: `/tmp/thegn-rolling-full-workspace3-results-20260914.json`.
Raw log: `/tmp/thegn-rolling-full-workspace3-20260914.log`.
Computed raw-log SHA-256 matches the receipt:
`7166604b1d90122adf27a546457e289f17efec27cc82f7d80d40ba8d8ed78cfb`.
Comparison log: `/tmp/thegn-rolling-full-workspace2-20260914.log`.
Preflight log: `/tmp/thegn-rolling-final-preflight-20260914.log`.

## Configured `just test` coverage

The current recipe requires `contract-ratchets`, `test-live`, `test-build-metadata`, then workspace nextest. All five tests selected by its three contract expressions are present as actual full3 passes:

- `thegn-core::plugin_api_wire snapshot_file_is_named_for_the_current_version`
- `thegn-core::plugin_api_wire wire_schema_matches_the_committed_snapshot`
- `thegn-svc::control_schema control_wire_matches_the_committed_snapshot`
- `thegn-svc::control_schema removed_browser_drive_is_absent_from_generated_contract_inputs`
- `thegn-core capability::tests::ratchet_pins_surface_gaps`

This verifies the intended 2 + 2 + 1 contract coverage without inflating the workspace count by counting those names again. The Python preflight log identifies the exact two configured commands and records their separate successful summaries. It is fixture evidence, not a live application restart or readiness claim. The 26 configured skips remain skips; ignored platform/optional tests are not claimed passed. This audit verifies the coverage represented by the recorded commands, not a claim that a single literal `just test` invocation produced every artifact.

## Final revision boundary

Current candidate `6c7bcdc06fdaa8bafe5e2f2640a70d2c7c7cec4e` adds only the independently reviewed ten-file test lint repair to `5b698dc4`: statement-scoped expectations for private synchronous fixture commands, best-effort cleanup diagnostics, a value-equivalent configuration initializer, an equivalent singleton comparison and a short-circuit-preserving let-chain. Its production behavior is unchanged.

The **completed** full3 pass belongs to the `5b698dc4` production-identical checkpoint. The root-coordinated focused native rerun of the exact post-lint fixture source remains **pending at this review** and must not be represented as already passed. The full3 receipt is neither a post-lint execution receipt nor evidence of every separate CI target.

No receipt discrepancy or new landing blocker was found beyond completing the already-required final lint/post-lint gates and recording local landing. THE-545's actual dispatch credential/account-generation binding and THE-154's native Windows/macOS execution and complete escaped-descendant containment remain open; this workspace pass does not close them.
