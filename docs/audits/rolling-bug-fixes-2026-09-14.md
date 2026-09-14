# Rolling bug remediation — September 14, 2026

This round continues the previously landed audit candidate from local main
`1ef8228f`. Scope is bugs, performance and resiliency; no new features.

## Reviewed source and acceptance

| Issue   | Change and reviewed checkpoint                                                    | Evidence / current limit                                                                                                                                                                 |
| ------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| THE-483 | Shared actual ticker worker and hostile-interval thread fixtures, `c6f9a9ec`      | All three actual host fixtures passed; independent source review accepted.                                                                                                               |
| THE-634 | Own-only admission for direct headless review handoffs, `02876bcc` and `415a7e78` | All eight new tests passed, including late authority changes and non-UTF-8 alias refusal. Parent THE-545 generation binding remains open.                                                |
| THE-154 | Portable resident lifecycle fixtures and native Windows CI selection, `58457da2`  | Eight new plus 48 existing Linux plugin tests and two ratchets passed; Windows full service/tests crosscheck passed. Native Windows/macOS and full process-tree containment remain open. |
| THE-591 | Already-landed atomic final outcome persistence                                   | Independent review accepted 14 atomic DB tests, eight persistence tests, speculative-land regressions and actual CLI proof.                                                              |
| THE-595 | Already-landed read-only discovery and selected-only snapshots                    | Strengthened native CLI harness passed 106 commands against main `1ef8228f`, covering target/source refs, indexes, contents and DB state.                                                |
| THE-597 | Already-landed owned gate workspace locking/materialization                       | Independent review accepted actual native adversarial gate/path fixtures and private CLI proof; unsupported non-Unix backend remains fail-closed.                                        |
| THE-600 | Registry corruption after admission test, `db2dbb55`                              | Actual pre-destroy rendezvous with a second SQLite writer; independent source review accepted, final compiled gate pending.                                                              |
| THE-593 | Result refinalization stale-selection tests, `7ad070c6`                           | Independent source review accepted; identical-value ABA remains explicitly outside value-based comparison guarantees. Final compiled gate pending.                                       |
| THE-594 | Actual projection/sync registry cleanup tests, `ceb4b5da`                         | Independent source review accepted positive/negative production cleanup paths; final compiled gate pending.                                                                              |
| THE-606 | Already-landed canonical history admission                                        | Existing native history fixtures passed; current full configured workspace gate is still required.                                                                                       |

## Coordinated verification

At source checkpoint `38107f9d`, focused execution passed 299 host tests and
44 core tests. The service receipt contains 58 passes (56 plugin tests and two
ratchets), plus one intentionally ignored exact-reexec helper, excluded from the
regression count. Binary hashes are recorded in
`/tmp/thegn-rolling-binary-receipt-20260914.json`. Independent review identified and
resolved the missing Issues fixture classification and non-UTF-8 path aliasing;
actual compilation additionally caught and corrected two test-only defects.

The normal CLI source `1ef8228f` is pinned by SHA-256 in
`/tmp/thegn-native-cli-20260914.json`. Its strengthened actual CLI test evidence is
`/tmp/thegn-integrate-native-tw5m0z40/evidence`, including all 106 commands, output,
private effective configuration and success receipt. The test only manipulates
its owned temporary Git/XDG/DB fixtures.

Final source/delivery ratchets, formatting and 156 strict OpenSpec validations
passed after assembling the cleanup tests. The full configured `just test` gate
is running with a per-test wrapper that gives each executable private XDG state
and isolated Git configuration, while preserving the configured test selection.
Final full-gate, scoped clippy and local landing receipts will be recorded here.

No live restart, external push, live queue cleanup or provider dispatch is part
of this fixture validation. Main's existing user justfile comments are preserved.

## Full workspace findings and revisions

The first full workspace run selected 8,354 tests and 26 configured skips. It
stopped fail-fast after 2,291 passes and one failure, leaving 6,062 tests unrun.
The failure exposed THE-635: plain log prefixes did not disable the outer field
writer's ANSI styling. Restoring NO_COLOR merely masked the defect. The fix at
`4e212d09` applies the selected policy to the actual field writer and tests plain
file/redirected stderr, terminal color and JSON with an explicitly color-capable
outer layer. Independent review approved the fix. The final workspace rerun
uses the same selection with `--no-fail-fast`.

Runner review also found HOME-dependent fixture gaps. Twelve reviewed read-only
path/planning fixtures preserve inherited HOME unchanged. One old cache test
could create real home directories or return without assertions; `b88ff9b6`
replaces that with the shared production helper operating on an owned temporary
home path. Its assertions cover cold directory creation, writable child mounts,
and no changes without a read-only parent. Independent review caught and fixed
a Windows separator comparison. No HOME environment value is repurposed.

The newly assembled cleanup subset passed all 50 tests in
`/tmp/thegn-rolling-cleanup-20260914-results.json`, including the actual late-hook
registry mutation, result-only refinalization, attached projection/sync refusal,
and the same-selection positive cleanup control after resource custody release.
