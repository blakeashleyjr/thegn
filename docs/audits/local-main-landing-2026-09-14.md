# Local main landing — September 14, 2026

The reviewed candidate `ea807922f760474e4e35b92cb361ea3bb2186762` was fast-forwarded
into local `main` after the user enabled unrestricted execution and requested a
retry. The prior canonical baseline was `f4c1355bf2dce35b78502e1fe2023ab6054e19ac`.
No push, live application restart, provider action or database migration occurred.

## Cleared validation and review gates

The exact 97 previously failed host tests and two previously failed service tests
all passed against the final candidate's copied binaries. Independent review
confirmed exact old/new test-name set equality, zero missing or duplicate tests,
exit status zero and one passed test in every per-test log. This includes all
30 previously inferred history/downstream failures and the signed Git/GPG fixture.
The earlier failures are historical evidence, not active landing blockers.

All 48 service plugin tests also passed in the newly unrestricted environment.
These runs supplement the recorded assembled 249 host, 120 core, 110 service and
four LSP tests, and the post-lint 27 host/eight core/48 plugin rerun. Counts overlap
and must not be added as unique coverage. Scoped clippy, formatting, source and
delivery ratchets, and 156 strict specification validations had already passed.

Primary and independent adversarial reviewers approved scoped local landing.
THE-154 remains open for native Windows execution, non-Linux runtime evidence and
complete escaped-descendant containment. The implementation retains the explicit
unproven-tree outcome; landing does not claim those guarantees or close that scope.
BTOP follow-ups THE-630–633 remain queued for later maintenance work.

## Worktree preservation

Git fast-forwarded with autostash and successfully reapplied the user's justfile
edit. Its wrapper override is incorporated in main; both explanatory comment
lines remain verbatim as an uncommitted user edit. All untracked audit files were
preserved. The original patch is retained in the audit artifact directory.

Evidence: `remediation-2026-09-13-artifacts/native-unblocked-validation.tar.gz`,
its per-suite JSON results, independent triage receipt and Git landing log.
