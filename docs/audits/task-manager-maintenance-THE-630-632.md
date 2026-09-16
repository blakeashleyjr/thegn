# Task manager maintenance: THE-630 / THE-631 / THE-632

Historical base: 7014a496. Current-main port base: 6884c3f0. This is an
implementation/review checkpoint, not a completion receipt; the current-main
port remains unaccepted pending primary/adversarial review, native gates and
paired performance evidence. THE-633 provider probing is already landed on
current main and is outside this existing THE-630/631/632 port.

## THE-630 implemented boundary

`proc_worker.rs` replaces the detached 500ms polling process thread with a
fallible worker, indefinite condition-variable park, monotonic enabled deadline,
one replaceable publication and generation/reset fencing. The worker owns the
existing `ProcSampler`; CPU baseline reset never bypasses its two-second minimum.
A parked acknowledgement follows any previous scan, wake attempt and reset.
The consumer rejects stale generations and Drop cancels without requiring a send.

`proc_worker_custody.rs` reserves the process-wide slot before spawning. The
successful handle is immediately guarded; reservation unwind recovers custody
without granting poisoned-slot admission. Retirement retains a token across an
outside-lock exact join. The slot survives run-future cancellation and startup
unwind; no application owner Drop detaches a live handle. A watch receipt alone
cannot release a thread that has not returned. Shutdown uses the same deadline
as the existing resident supervisor, requests both cancellations first, and
reports Held/Failed separately from Settled.

Failures publish a fixed receipt before opaque panic payload destruction and
attempt exactly one guarded terminal notification so pre-sample failure can be
seen by the compositor. Wake failure is terminal; it cannot strand a silently
replaced pending slot. Exact-join failure publishes before releasing retirement
into a terminal Failed slot that retains the exceptional join-error payload.
It refuses replacement without invoking arbitrary Drop on a cleanup caller.

At the UI boundary, hidden demand is revoked before unrelated loop work and
again before final take. Pane/daemon attribution is published before positive
admission; the first immediate sample therefore uses the available inputs.
Accepted publication revisions travel with the snapshot across hydration.

## Historical evidence retained

The worker/custody harness receipts are historical source-only evidence from the
older candidate, not current-main acceptance:

- **13/13** in `/tmp/thegn-630-worker-harness-tests.log`;
- revision-2 receipts in `/tmp/thegn-630-worker-harness-revision2-tests.log` and
  `/tmp/thegn-process-allocation-harness-tests.log`;
- **14/14 worker/custody** and **1/1 allocator scope** in
  `/tmp/thegn-630-worker-harness-revision3-tests.log`;
- baseline source manifest at
  `/tmp/thegn-maintenance-03-process-baseline-20260914/manifest.json`.

These harnesses imported production source with cached debug dependencies and
inert observer/QoS/perf hooks. They did not prove a current-main host build,
coordinated host tests, release measurements or native UI behavior. The earlier
scoped OpenSpec/delivery validation belongs to that historical candidate.

## Current port HOLD and pending evidence

The current-main source port is based on `6884c3f0` and includes the reviewed
render change from `cdffef53`. That hash identifies source provenance for the
copied layer change, not a current-main acceptance commit. The port has no
compiler or host acceptance receipt. The separate frozen candidate's release
build/performance evidence path is
`/tmp/thegn-layer-host-build-20260916-1m4i32rq`; its result must be paired with
the matching baseline before any regression decision. Retained stage-B results
from `f2a39b40` are at
`/tmp/thegn-frame-stage-resume-20260916-gvm3bab7/candidate-f2a39b40-frame-stage-release-tests`
and its primary/independent analysis files; they describe the older candidate
and are not current-port acceptance.

Native host tests, scoped strict source/lint gates, owned Muse frames, paired
refresh/render measurements, final primary/adversarial review and local-main
landing remain pending. The performance HOLD and no-queue-admission boundary
remain unchanged.

## Renderer stage status

At the historical checkpoint, root owned the separate Processes-only
fixed-column renderer and sanitized Unicode clipping/selection tests. The
current-main port now stages that existing renderer and its tests; native
execution, paired performance, Muse frames and final acceptance remain
pending.

## THE-631 implementation and paired measurement revision

`monitor/invalidation.rs` caches rows only for the active list. The process key
contains the entire accepted publication revision plus sort/direction/tree/filter;
no snapshot clone or PID-only key is used. Geometry/navigation can rebuild the
body without sorting rows. A Process body with unchanged inputs returns no
repaint. Existing history coverage text can still advance independently without
rebuilding its body or rows. Graph tabs retain the previous refresh/time cadence.

Disk inputs receive a semantic revision at the authoritative hydration swap,
comparing both size and timestamp maps by borrow. Saturation disables that cache
rather than reusing a wrapped revision. Cached rows retain their measurement
stamps so displayed ages advance independently of ordering and labels. Equal
size/name rows use the full path as the final deterministic ordering key; a
reverse-insertion fixture covers identical basenames.

`FrameModel::take_process_publication` is the real compositor's sole live
process assignment and reconciles the gate before final take. Hydration's
`carry_monitor_state_from` is the other production assignment (initialization
excepted). The new host fixture queues an actual worker publication across a
pause, proves admission rejects it, and then drives paused filtering/navigation
and verifies the sampled identity in signal confirmation. No second frozen
snapshot is introduced. Eight host invalidation/boundary fixtures and a disk
revision/exhaustion fixture await coordinated host execution.

The measurement revision adds a test-only System-forwarding allocator with
constant, non-dropping thread-local counters. Its scoped calling-thread counts
exclude fixture mutation, result-vector recording and JSON serialization;
requested bytes count allocation/reallocation calls, not live heap size. Actual
process/disk/body builder entries have test-only counters. Both baseline and
candidate must receive identical instrumentation before comparison. Independent
allocator/provenance source review approved this revision. An extra renderer-only
ignored selector avoids repeating sampler windows for renderer-only revisions.

After metrics-factory extraction and measurement hooks, the isolated actual-source
worker/custody harness still passed13/13 and the actual counter isolation/unwind
fixture passed1/1. Logs: `/tmp/thegn-630-worker-harness-revision2-tests.log` and
`/tmp/thegn-process-allocation-harness-tests.log`. These use cached debug
artifacts and inert observer/QoS/perf harness hooks; they are not release or full
host evidence. The blocking-payload regression now runs cleanup in an owned
helper, releases the controlled destructor gate and joins before asserting,
so a regression cannot hang the watchdog caller before its deadline check.

The final source-only checkpoint passes **14/14 worker/custody tests** and
**1/1 allocator scope test**. Log:
`/tmp/thegn-630-worker-harness-revision3-tests.log`. The additional regression
unwinds an assertion while the actual fixture collector is blocked and proves
its first-field cleanup guard releases the owned gate and retires the exact
thread. A test-only 32-slot reservation retains private custody before spawn;
cleanup has a ten-second deadline and never joins an unfinished handle. Failure
to settle retains that slot until test-process exit instead of detaching it.
This private test mechanism does not consume or relax production admission.

The paired common workload SHA256 remains
`61f2e5d025f61c9c640ad14ca15ffc921aa77666f7cd50e9272e9659bb60421c`.
This metadata records provenance only; it does not claim current-port host
execution or release acceptance.
