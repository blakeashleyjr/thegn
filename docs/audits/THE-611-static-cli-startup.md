# THE-611 static CLI startup boundary

The reviewed implementation is included in combined `62335026`; its original source base was `7014a496`. Current parser and actual CLI fixtures have passed, and two old-binary counterfactuals demonstrate the startup-order defect. Strict lint and source/documentation checks are complete; reviewed implementation landed on local main at `c0d3d860a22db0a7fddafb3d338bf40b25248ade`.

`command_intent` exhaustively borrows all59 Command variants and each nested Config/API/Automations action. Only six StaticIntent forms return before migration/profile reroot. Existing helpers retain path spelling, schema source, API tables, grouped completion generation, basename and best-effort stdout. Schema derivation still constructs its declared defaults; the repaired boundary is no effective-config load/install or fabricated handler Config.

Parser fixtures call actual Cli::try_parse_from for all55 configured variants plus the four families and all nested variants; they assert parsed identity and exact intent. Five shells, two registration modes and both global argument positions are exercised without calling handlers.

The six actual-binary test functions (four portable plus two Unix) cover nine output forms, malformed/missing/FIFO inputs, invalid overrides, both profile selectors, exact private filesystem canaries, path spelling/default path, executable alias, help/version, EPIPE and configured loader controls. They never inherit stdin or PATH tools. The config-get control returns drawer.height="17", distinct from default"35%". No configured edit/setup/provider/daemon action is executed. FIFO negative controls remain source-traced observations, not proof that their configured action ran.

Private subprocess custody is reserved before spawn. File output has an8MiB accepted limit; success has a10-second deadline, configured FIFO observation500ms, termination a10-second polling budget. Drop tries bounded500ms cleanup; an unreaped child and its filesystem remain in one of32 static test custody slots. No detached thread, raw-PID fallback, or unbounded wait is used. Outer test-process custody is required for pathological system calls/retained failure. Private env paths are not an OS sandbox or universal no-home-access guarantee.

Primary and independent final source/fixture reviews are positive. Combined focused and full native runs include the parser and actual CLI fixtures. THE-505/592 checked admission, THE-607 worker loading, THE-612 source projections and existing dynamic TAB behavior are not closed by this repair.

Independent fixture review accepted the observation-error correction: any failed child observation permanently revokes further signal/wait syscalls and retains custody. A deterministic callback-count regression covers initial loss and loss after prior successful observation without spawning a child. The observation-error regression passed in the current native gates.

## Old-binary counterfactual boundaries

The retained old application pin is source `9a96dde4`; its reviewed relevant startup paths predate this repair. The actual copied integration test binary is a separate artifact. Its exact Unix FIFO selector fails on the first static config-path call at the owned deadline (exit 100, 10.272 seconds); later configured siblings are not reached in that mutant run.

The startup canary fails on the first config-path call with the CLI profile selector (exit 101, 0.46 seconds). The status-success assertion passes; the next empty-stderr assertion observes a reported two-path legacy migration and malformed-config fallback. The child was observed and reaped before output validation. The stdout comparison, filesystem snapshot, strict Fixture.close, later static rows and environment-profile iteration were not reached; ordinary TempDir unwinding is not a strict directory-cleanup receipt. The modern positive fixture covers the complete assertions. The earlier stale unit-artifact incident remains explicitly invalid in the retained evidence.

## Segmented native evidence and remaining gates

[The evidence manifest](maintenance-04-2026-09-15/manifest.json) preserves original receipts, logs and independent reviews byte-for-byte. Combined `62335026` focused acceptance passed 68/68. Its full configured native run completed 8377 tests: 8376 passed, one host-key-literal ratchet failed, and 26 configured tests were skipped. Test-only `a4468a8e` replaces the precedence fixture's HostKeyAlias argument with distinct ConnectTimeout values; its fresh six-test gate passes all five precedence tests and the formerly failing ratchet. These are overlapping, segmented gates, not a single clean full-a446 run.

Source ratchets, 159 strict OpenSpec items, treefmt and non-Rust lint passed. Final metadata ratchets/OpenSpec checks and strict offline workspace/all-target Clippy also exited zero; Clippy took 13m58s, with inherited dependency warnings retained in the raw log. [The final gate receipt](maintenance-04-2026-09-15/thegn-maintenance04-final-gates-receipt-20260915.json.raw) records exact commands, source and log hashes. Reviewed implementation landed on local main at `c0d3d860a22db0a7fddafb3d338bf40b25248ade`. [The landing readback](maintenance-04-2026-09-15/thegn-three-fix-local-main-readback-20260915.json.raw) verifies all 16 reviewed Rust hashes, the 40 pre-landing evidence payloads and preserved user files. Tracker closure/readback remains pending; no new performance acceptance is claimed.

## Future PR gate (not requested/executed)

- [ ] Before opening any future PR, run `just ci` per CLAUDE.md; this pre-PR gate does not replace the local landing checks above.
