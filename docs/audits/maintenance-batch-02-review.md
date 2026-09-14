# Maintenance batch 02 review checkpoint

This is a private review candidate, **not a canonical-main landing**. The checkout's
Git metadata remains read-only. Batch-01 native ownership/socket test failures
remain outstanding and are not waived by this batch's focused passes.

Eight of the selected ten issues have implemented candidates with primary and
independent source review. THE-484 and THE-154 remain under implementation.

| Issue | Implementation and revisions | Evidence so far |
| --- | --- | --- |
| THE-628 | Preserve process snapshots, deterministic ties and sampled selection/action identity | Actual host: 79 monitor and 3 model-equality tests passed; independent source review passed. Final OS signal identity race is not solved. |
| THE-286 | Bounded streaming Kitty parser; fix the escape-pair text-cap edge | Actual host: 14 tests passed; independent actual-source review/tests passed. |
| THE-310 | Bounded amortized framing; refuse near-cap repeated compaction; synchronize late requests with terminal closure | Actual service: 13 tests passed; independent decoder and closure review passed. |
| THE-471 | Bounded calendar display projection, preserved semantic data and matching width/fallback rules | Actual host: 6 detail and 4 reminder tests passed; independent review passed. |
| THE-343 | Set-only clipboard parser, receipt-generation barriers and bounded FIFO retry; fix stale-prefix retries and nested control-string admission | Earlier host writer/backlog/parser tests passed; final parser 6/6 independent harness passed; final combined graph pending. |
| THE-377 | Typed bounded command compilation, inert argv/POSIX quoting, borrowed input, explicit migration and pre-write validation | Final compiler 9/9 and config-write filter 16/16 passed; independent review accepted. Transport tests execute projections locally; remote terminal execution remains separate. |
| THE-545 | Fresh author/viewer proof bound to selected PR/repository/head and verified local execution; recheck before side effects | Core proof/parser tests 5/5 passed. Review caught mismatched selected/fetched PR numbers and unproven remote execution routes; both now hold. Host/service tests pending. |
| THE-483 | Checked independent refresh cadences, duration schema/write bounds and security-preserving load diagnostics | Core 32/32 plus existing config regressions 293/293 passed. Review fixed dropped environment security overrides and alias bypass. Small load benchmark +0.21% is within noise. |
| THE-484 | Remaining signed duration/epoch consumers and authoritative provider-age quarantine | In progress; unknown ages must never authorize deletion. No provider actions are run by tests. |
| THE-154 | Bounded resident I/O admission, owned cancellation and truthful lifecycle outcomes | In progress. Full native process-tree containment is outstanding; settled child/pipes must not be labeled a certified tree reap. |

The first assembled host graph compiled in 3m52s. Its selected run passed
**165/165 tests**, including the monitor, model equality, parser, writer, calendar,
render-plan and four sandbox-floor cases. These overlap the table's module
counts; do not add them together. A separate five-test platform ratchet initially
found the Unix-only workload in a general module. Moving it into the platform
seam resolved the finding; the source ratchet rerun passed 5/5. Final compilation
of the relocated test remains part of the next graph.

The next combined host/core/service build includes THE-545 and THE-483, the final
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
Full batch-02 validation, the two remaining implementations, and landing must
remain explicit unfinished gates until actually completed.
