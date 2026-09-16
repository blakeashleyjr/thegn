# Maintenance batch 02 review checkpoint

**September 14 update:** the reviewed candidate is now merged into local main.
All 99 previously blocked tests pass. Earlier read-only/failed-validation holds
below are historical; see [landing receipt](local-main-landing-2026-09-14.md).
THE-154 native platform/containment follow-ups remain open.

This is a private review candidate, **not a canonical-main landing**. The checkout's
Git metadata remains read-only. Batch-01 native ownership/socket test failures
remain outstanding and are not waived by this batch's focused passes.

All ten selected issues now have implemented candidates with primary and
independent source review. THE-484 includes the requested authority revisions;
THE-154 scoped lifecycle repair is approved at `53502768`. Final assembled validation passed; native execution/containment and
canonical landing remain separate unfinished gates.

| Issue   | Implementation and revisions                                                                                                                | Evidence so far                                                                                                                                                                  |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| THE-628 | Preserve process snapshots, deterministic ties and sampled selection/action identity                                                        | Actual host: 79 monitor and 3 model-equality tests passed; independent source review passed. Final OS signal identity race is not solved.                                        |
| THE-286 | Bounded streaming Kitty parser; fix the escape-pair text-cap edge                                                                           | Actual host: 14 tests passed; independent actual-source review/tests passed.                                                                                                     |
| THE-310 | Bounded amortized framing; refuse near-cap repeated compaction; synchronize late requests with terminal closure                             | Final assembled service: 32 LSP tests and four fake-LSP integration tests passed; independent decoder and closure review passed.                                                 |
| THE-471 | Bounded calendar display projection, preserved semantic data and matching width/fallback rules                                              | Actual host: 6 detail and 4 reminder tests passed; independent review passed.                                                                                                    |
| THE-343 | Set-only clipboard parser, receipt-generation barriers and bounded FIFO retry; fix stale-prefix retries and nested control-string admission | Final combined host: 13 query/parser, 14 backlog and 9 writer tests passed; independent review accepted.                                                                         |
| THE-377 | Typed bounded command compilation, inert argv/POSIX quoting, borrowed input, explicit migration and pre-write validation                    | Final compiler 9/9 and combined config-write 15/15 passed; independent review accepted. Transport tests execute projections locally; remote terminal execution remains separate. |
| THE-545 | Fresh author/viewer proof bound to selected PR/repository/head and verified local execution; recheck before side effects                    | Core proof/transport 5/5; host proof 5/5, PR-driver 20/20, review 5/5 and CI hold passed; service ladder 7/7. Selected-number and unproven-route gaps now hold.                  |
| THE-483 | Checked independent refresh cadences, duration schema/write bounds and security-preserving load diagnostics                                 | Core 32/32 plus existing config regressions 293/293 passed. Review fixed dropped environment security overrides and alias bypass. Small load benchmark +0.21% is within noise.   |
| THE-484 | Remaining signed duration/epoch consumers and authoritative provider-age quarantine                                                         | Reviewed at 3f3d73c7; 144 core, 28 service and 26 proxy/media scoped tests passed. Unknown age/authority quarantines; no live provider actions.                                  |
| THE-154 | Bounded resident I/O admission, owned cancellation and truthful lifecycle outcomes                                                          | Reviewed at 53502768; final assembled plugin tests 48/48 passed, plus host plugin integration. Native Windows and full descendant containment remain open.                       |

The first assembled host graph compiled in 3m52s. Its selected run passed
**165/165 tests**, including the monitor, model equality, parser, writer, calendar,
render-plan and four sandbox-floor cases. These overlap the table's module
counts; do not add them together. A separate five-test platform ratchet initially
found the Unix-only workload in a general module. Moving it into the platform
seam resolved the finding; the source ratchet rerun passed 5/5. The second combined graph compiled the relocated test successfully.

The second combined host/core/service build includes THE-545 and THE-483, the final
clipboard revision, and the merged duration/command pre-write guards. The sole
config_write conflict was resolved using one prior document read and one next
string; both validators run before any mkdir/write, and that exact validated
string is persisted. The independent reviewer accepted this resolution.

THE-627's full build_model workload also passed for 1/8/32 real private worktrees.
See `full-hydration-workload-2026-09-13.md` for all measurements and limits.
Repeated unrelated devcontainer executable probing is now THE-633. BTOP's layout
and sampling references and the queued THE-630/631/632 monitor improvements are
documented in `btop-parity-and-performance-2026-09-13.md`.

Raw checkpoint results: `/tmp/thegn-audit-batch02-host-results.json`,
`/tmp/thegn-audit-batch02-host-tests.log`,
`/tmp/thegn-audit-batch02-host-build.log`, and
`/tmp/thegn-audit-batch02-platform-ratchets-final.log`.
Final assembled batch-02 validation is recorded below. Canonical landing remains
unfinished and is not authorized by the focused passes alone.

## Combined checkpoint 2

The host/core/service all-target test build at `244882ba` (plus the updated
`lsp_client` integration smoke caller) completed in 3m36s. The shared decoder now
returns fallible results; the smoke test checks successful admission and decode
explicitly. All four actual fake-LSP integration tests passed.

Copied binaries were run with a fresh private XDG environment per test. The host
passed 207 selected tests plus seven disjoint cadence/panic/platform-ratchet
tests (214 total). This includes all five PR-authorship fixtures, 20 PR-driver
tests, five durable-review tests, the CI author hold, final OSC parser/backlog
revisions, 79 monitor tests and three model-equality tests. These overlap the
earlier 165-test checkpoint; they are not additive evidence.

The core passed 566 configuration, duration and PR-proof tests. The service
passed 39 forge-ladder and LSP tests. Ten disjoint custom-command compiler and query-transport tests also passed,
bringing this core selection to 576. The compiler fixtures execute transport
projections locally; no service custom-command test module exists. This verifies the combined
duration and command pre-write guards, including the conflict resolution.

Review of THE-484 requested present Fly custody, unambiguous account/lifetime
policy before inventory or cleanup, and exact nonempty recorded VPS identity.
THE-154 review is challenging cancellation custody, final-response ordering,
stale-generation routing, task registration during shutdown and lost Unix
child identity. Both drafts were pending at checkpoint 2; the subsequent reviewed revisions are recorded below.

Evidence is under `/tmp/thegn-batch02-{host-checkpoint2,host-extra2,core-checkpoint2,svc-checkpoint2,lsp-integration}-results.json`
and corresponding logs. Batch-01 native socket/ownership failures remain open.

## Provider-expiry revision

THE-484 source checkpoint `2dbe30aa` and evidence update `3f3d73c7` are now
merged into the private review branch. Primary and independent source reviews
approved present Fly custody, exact single-machine inventory proof, ambiguity
quarantine before any provider access or ledger cleanup, and nonempty exact
recorded VPS identity. The author passed 118 focused core tests, 26 additional
core lease/heal/config tests, 28 service tests, 26 proxy/media tests and the
actual-source ambiguity-action fixture. Shared primitive fixtures pass in debug
and optimized builds. These scoped counts overlap some earlier selections.

Final combined host compilation/testing passed with THE-154; see the final assembled checkpoint below. Provider
inventory/read-to-delete generation races and unknown historical ownership
remain explicit limitations; quarantine retains resources for reconciliation.
No live provider action was executed.

## Resident lifecycle final source review

THE-154 final revision `53502768` is merged into private candidate `430b5743`.
Independent adversarial review approved bounded admission/serialization, retained
cancellation custody, provider response routing, Linux pidfd notification, and
truthful lifecycle outcomes. Final revisions prohibit numeric waits after child
identity loss, reserve the wire delimiter without geometric allocation growth,
and terminate the exact still-owned leader after either success or ESRCH from
the original process-group signal. Other group errors remain errors.

The native service checkpoint passed 46 tests before the final two regressions.
Final actual-source platform fixtures passed 4/4; the bounded JSON regression
passed separately. Windows service `--tests` cross-check passed, and the exact
final Unix source plus tests typechecked for Darwin in an isolated harness.
These are not native Windows/macOS execution evidence or proof of complete
escaped-descendant containment. See `THE-154-independent-review.md`.

The environment's UnixStream write fails with EPERM, explaining the observed
Tokio SIGCHLD notification timeout here. The Linux pidfd path passed a fixture
with no wake during a 60 ms idle observation and notification on child exit.
That narrow fixture is not a whole-application idle CPU benchmark.

## Final assembled checkpoint

At `430b5743`, the combined host/core/service test build passed in 5m38s.
The selected isolated runs passed 221 host, 120 core and 110 service tests,
plus all four fake-LSP integration tests. The service selection includes all
48 plugin tests with the final allocation/identity/group-result regressions.
Host coverage includes 79 monitor tests, 19 monitor-pipeline tests, three model
equality tests, 14 plugin handlers, both plugin lifecycle/registry tests,
eight provider-reaper admission/retirement tests, and PR/CI/cadence regressions.
The core selection covers the shared time policy, backoff, hostile usage reset
values, configuration duration/write policy and command/author proof guards.
These counts overlap previous checkpoints and must not be added to them.

Scoped lint found eight style errors across diagnostic eviction, repository-origin
parsing, lifecycle/authority conditionals and bounded-buffer clamping. It also
required a narrow expected-lint annotation for an existing blocking child wait
on the dedicated background reaper thread. These edits preserve behavior and
received independent review; the final lint result and focused post-edit tests
are recorded separately below.

The final host selection also passed 28 disjoint frame-writer, PTY-drain and
platform-ratchet tests, bringing this host checkpoint to **249 passed**.

The reviewed lint cleanup is committed as `a727b430`. Scoped `cargo clippy
--offline --locked` for host/core/service/proxy/media/metrics passed with
`-D warnings` in 1m20s. The vendored termwiz dependency still reports 13
pre-existing lifetime-style warnings under Cargo's dependency lint cap; no
workspace lint suppression was added. Source ratchets, delivery schema and
10 delivery fixtures, and all 156 strict OpenSpec validations passed.

After the lint-only revision `a727b430`, the host/core/service unit-test binaries
rebuilt successfully in 4m00s. Focused isolated reruns passed **27 host, eight
core and all 48 service plugin tests**. These cover the exact modified reaper,
clipboard, review-authority, diagnostics, repository-proof and resident lifecycle
paths. They overlap prior checkpoint counts. Raw evidence is named
`thegn-batch02-{host,core,svc}-postlint-final-*`.

All feasible scoped implementation/review gates are complete. **Canonical-main
landing is neither performed nor approved:** Git metadata is read-only, the
99 batch-01 environment-affected failures still need native reruns, and full
THE-154 native non-Linux execution/escaped-tree containment remains open. No
issue was marked Done to conceal those limits. The BTOP-derived follow-ups
THE-630–633 remain queued for a later maintenance batch.
