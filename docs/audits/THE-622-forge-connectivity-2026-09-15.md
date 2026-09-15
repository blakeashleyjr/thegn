# THE-622 forge connectivity evidence — 2026-09-15

This candidate records the reviewed test-only THE-622 evidence after merging canonical source metadata commit `060afad525a14547336e8d2170803a66af877812`. The candidate keeps the earlier `harden-live-diagnostics-and-forge` delivery history unchanged. The `verify-native-forge-connectivity` task checklist records source review, native controls, counterfactual inspection, and corrected gates complete; reviewed local-main landing remains pending.

The five scoped Rust files are byte-identical between this 622 candidate and current06:

- `crates/thegn-svc/src/forge/native.rs`
- `crates/thegn-svc/src/forge/native_global_connectivity_tests.rs`
- `crates/thegn-svc/src/forge/native_regression_tests.rs`
- `crates/thegn-host/tests/static_cli_support/mod.rs`
- `test/support/owned_test_child.rs`

Their hashes and the shared custody/source comparisons are preserved in the raw source checkpoint and review files. No THE603, THE602, THE639, or THE640 code is imported.

The positive focused current06 native receipt records 121 passed and 8220 skipped in 13.963 seconds. The additional THE622 parallel run records 20 passed, one ignored, zero failed in 0.49 seconds with four test threads. The corrected current06 gate receipt records native 16 passed/3365 skipped, strict Clippy exit 0, source ratchets exit 0, and OpenSpec 164 passed/0 failed. The original current06 Clippy log is retained: it failed with two style checks before the correction; the corrected receipt does not rewrite that history or claim the original lint was clean.

The exact counterfactual is preserved with its build JSONL/log, native log, receipt, depfile, and source reviews. It is based on `62289bb730b1b1a188761ef4b0a3c4b8723f914d` and deletes only the global `report_success` call from `GhCircuit::record_success`. Its build exited 0 after 159 seconds. The depfile names the private clone as `CARGO_MANIFEST_DIR` and includes the affected forge source. Cargo emitted local-package artifacts with `fresh:false`; the local fresh compiler-artifact record, exact depfile, source hash, pinned binary hash, and intended semantic failure together provide normal provenance for this review.

The counterfactual selector exited 101 as intended. The child reached the first errors-only reachable case and failed `reachable errors-only must publish global success` with `Offline` observed instead of `Online`. The parent then reported the expected SDK fixture failure after reaping the child. The later matrix, complete receipt, and strict normal-success `TempDir::close` path were unreachable; this failure does not claim those paths.

The pinned binaries remain outside this candidate by design. The manifest records their exact source paths and hashes, while the raw depfile and receipts preserve the provenance needed to review them. No provider, credential, shell, Cargo, native, UI, benchmark, or mutable-target operation was performed while preparing this candidate. Landing remains pending root review and final canonical fast-forward/update.
