# Rolling bug remediation — September 14, 2026

This round continues the previously landed audit candidate from local main
`1ef8228f`. Scope is bugs, performance and resiliency; no new features.

## Reviewed source and acceptance

| Issue   | Change and reviewed checkpoint                                                      | Evidence / current limit                                                                                                                                                                 |
| ------- | ----------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| THE-483 | Shared actual ticker worker and hostile-interval thread fixtures, `c6f9a9ec`        | All three actual host fixtures passed; independent source review accepted.                                                                                                               |
| THE-634 | Own-only admission for direct headless review handoffs, `02876bcc` and `415a7e78`   | All eight new tests passed, including late authority changes and non-UTF-8 alias refusal. Parent THE-545 generation binding remains open.                                                |
| THE-154 | Portable resident lifecycle fixtures and native Windows CI selection, `58457da2`    | Eight new plus 48 existing Linux plugin tests and two ratchets passed; Windows full service/tests crosscheck passed. Native Windows/macOS and full process-tree containment remain open. |
| THE-591 | Already-landed atomic final outcome persistence                                     | Independent review accepted 14 atomic DB tests, eight persistence tests, speculative-land regressions and actual CLI proof.                                                              |
| THE-595 | Already-landed read-only discovery and selected-only snapshots                      | Strengthened native CLI harness passed 106 commands against main `1ef8228f`, covering target/source refs, indexes, contents and DB state.                                                |
| THE-597 | Already-landed owned gate workspace locking/materialization                         | Independent review accepted actual native adversarial gate/path fixtures and private CLI proof; unsupported non-Unix backend remains fail-closed.                                        |
| THE-600 | Registry corruption after admission test, `db2dbb55`                                | Actual pre-destroy rendezvous with a second SQLite writer; independent review accepted and all 50 cleanup tests passed.                                                                  |
| THE-593 | Result refinalization stale-selection tests, `7ad070c6`                             | Independent review accepted and actual stale-selection/clear fixtures passed; identical-value ABA remains outside value-based comparison guarantees.                                     |
| THE-594 | Actual projection/sync registry cleanup tests, `ceb4b5da`                           | Independent review accepted actual registry refusal, custody restoration and same-selection positive cleanup; all 50 cleanup tests passed.                                               |
| THE-606 | Already-landed canonical history admission                                          | Native canonical-history fixtures and the complete 8,354-test workspace run passed; private CLI proof is retained.                                                                       |
| THE-377 | Correct command examples and justified per-entry environment exclusions, `b22ec481` | All three previously failing contracts and the complete workspace pass; reopened issue can close after landing.                                                                          |
| THE-635 | Apply plain/color policy to structured fields, `4e212d09`                           | Actual plain/TTY/JSON formatter and isolated production-sink regressions pass; no payload stripping.                                                                                     |
| THE-636 | Own local watchdog worktree, shell and state, `c3acc7a4`                            | All watchdog cases pass, including actual single-swap behavior and explicit outer fixture cleanup; native Unix evidence.                                                                 |
| THE-637 | Pin private Git branch and identity, `6c077905`                                     | Both previously failing divergence/conflict tests and the service Git suite pass with isolated Git configuration.                                                                        |

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

Final source/delivery ratchets, formatting and 157 strict OpenSpec validations
passed at the assembled source checkpoint. The full workspace runs use a per-test
wrapper giving each executable private XDG state and isolated Git configuration,
while preserving configured test selection. Final results appear below.

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

## Remaining full-gate fixture repairs

The second full run executed all 8,354 selected tests: 8,348 passed and six
failed, with 26 configured skips. THE-635's real formatter/sink regressions and
all new ticker, admission, cleanup and plugin fixtures passed. The six failures
were three THE-377 command-example/environment-contract checks, THE-636's
watchdog fixture selecting Podman before private cleanup failed, and THE-637's
Git fixtures assuming global branch/identity defaults.

THE-377 was reopened rather than retaining its earlier scoped closure. Valid
separate argv/safe-shell examples now document the actual fields, and precisely
two per-entry command keys have justified environment-override exclusions; no
schema walker or admission policy was weakened. THE-636 now owns an existing
worktree and private state, disables provider routing and runs the production
clean-shell path through an rc-free Unix adapter. Its platform fixture preserves
the original test identities and makes successful cleanup explicit. The exact
old private overlay residue was removed after checking mounts and helpers; its
receipt is `/tmp/thegn-owned-watchdog-fixture-cleanup-20260914.log`. THE-637 pins
bare main and conflicting-merge identity, checks conflict exit status and uses
TempDir custody. All three repairs received primary and independent review.
The third complete workspace run tests this assembled revision.

The first strict lint attempt also identified three direct println macros in
native reexec fixtures. Explicit owned stdout writes preserve the protocol frames
and flushes without lint suppression; independent review approved that revision.

## Complete workspace acceptance

At source `5b698dc4`, the third workspace run passed **8,354/8,354 tests**, with
26 configured skips, in 185.563 seconds. A parsed receipt verifies every selected
name matches the second run and all six previous failures now pass. The count is
Nextest's configured test count, not a claim that helper entries are independent
regressions. Source receipt: `/tmp/thegn-rolling-full-workspace3-results-20260914.json`.

The `just test` recipe's workspace stage was run as `cargo nextest run --workspace
--locked --no-fail-fast`. Its three contract selections (2 plugin schema, 2 control
schema, 1 surface ledger) passed in the original explicit preflight and are also
included in the final full run. The final `just test-live test-build-metadata`
invocation passed 21 live-build and 3 build-metadata tests. These logs together
cover the configured recipe; the workspace log alone is not the entire recipe.
No additional tests were skipped to obtain this result.

The final all-targets lint run subsequently reported 14 host-test findings and
one core-test finding: fixture diagnostics, equivalent initializer/control-flow
style, a cloned assertion slice and synchronous owned fixture command annotations.
Those test-only revisions require independent review, affected regression
execution and a successful strict lint rerun before landing. Their annotations
are local to verified off-compositor fixture statements, not runtime exemptions.

## Post-lint revision verification

The approved test-only lint repair is `6c7bcdc0`. All **139 selected affected
host/core tests passed**, in 10.497 seconds (7,335 tests filtered out of this
focused selection). This overlaps the full workspace result; counts are not
added. The full workspace source `5b698dc4` and this revision have identical
production behavior. Raw logs and the exact-name receipt retain both checkpoints.

All three subagents reviewed the final fixture changes across authors, and the
primary inspected every diff. The final full-run independent receipt verifies
8,354 unique passes, the same selected names as the previous run, all six prior
failures corrected, all three contract selections and both Python preflights.
Source/delivery ratchets and all 157 strict OpenSpec items passed after lint
cleanup. Strict Clippy passed for host/core/service with `--all-targets -- -D
warnings` at `6c7bcdc0`, in 2m31s. Full treefmt CI also passed. The primary approves
this reviewed candidate for the user-authorized local-main merge.

Scoped closure after landing is supported for THE-377, THE-483, THE-591,
THE-593, THE-594, THE-595, THE-597, THE-600, THE-606, THE-634, THE-635, THE-636
and THE-637. THE-545 remains open for actual credential/account-generation
binding; THE-154 remains open for native platform and complete process-tree
acceptance. THE-630 has a reviewed sampler investigation/plan; its implementation
has not landed. THE-631/632/633 remain queued performance follow-ups.

## Local-main landing

The reviewed candidate `0e726cb70e35f643126215aa982947a08942387e` was fast-forwarded
into canonical local main on September 14, 2026, from `1ef8228f`. Its final code
checkpoint is `6c7bcdc0`; the intervening commit records reviewed validation only.
The final follow-up commit reconciles documentation after this actual landing.

Saved byte hashes confirm the user's justfile comments and existing live-audit
files were preserved. The merge did not push a remote or restart the live build.
The complete workspace pass, affected post-lint pass, strict Clippy, source and
specification gates support the 13 scoped Linear closures listed above. Broader
THE-545/THE-154 acceptance and the queued BTOP performance work remain explicit.
Durable raw receipts, private CLI evidence, independent reviews and a verified
Git bundle are preserved in `docs/audits/remediation-2026-09-13-artifacts/`, with
a refreshed byte-count/SHA-256 manifest. Earlier failed-run artifacts remain
historical evidence rather than being relabeled as successful runs.
