# Runtime performance metric boundaries

Rollups with **metric_version=2** retain older numeric keys for consumers.
Histogram percentiles remain lower bucket edges, not exact times; zero also
represents 0–1 µs samples. Always use the sample count. **interval_seconds**
reports the actual interval, which can exceed configured cadence because
diagnostics add no timer or wake.

`*_p95_us` keys (render, input, writer completion, writer input) are additive in
version 2. A window with fewer than 20 samples is sparse: its p95 is the maximum
and its p99 describes one or two samples, so the status dashboard shows the
count and `(sparse)` instead of a tail, and `no samples` only when the count is 0. Submitted-frame timing (render/input), successful sink completion
(`writer_*`), and physical paint are separate boundaries; physical paint is
never measured.

| Fields                                                            | Boundary and coverage                                                                                                                                            |
| ----------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| render_p50_us, render_p95_us, render_p99_us, frame_samples        | Host composition through encoding/submission; synchronous rendering includes sink I/O.                                                                           |
| flush_p\*\_us                                                     | Legacy name for encoding/submission; not async flush time.                                                                                                       |
| input_p50_us, input_p95_us, input_p99_us, input_samples           | Earliest pending host observation to next frame submission. A sample covers a coalesced cohort, not necessarily an application response.                         |
| input_events, input_coalesced_events                              | Dispatched key/mouse/paste events and those joining a pending cohort. Preemption observations are not extra dispatches.                                          |
| writer_queue_p99_us, writer_write_p99_us                          | Queue wait before a successful frame write and duration in sink write+flush.                                                                                     |
| writer_completion_p95_us, writer_completion_p99_us, writer_frames | Submission to successful sink completion and its sample count.                                                                                                   |
| writer_input_p95_us, writer_input_p99_us, writer_input_samples    | Submitted cohort's earliest observation to successful sink completion.                                                                                           |
| writer_failed_frames                                              | Timed writes that failed, excluded from completion histograms. OOB and frames discarded after fatal errors are not successful frames.                            |
| writer_metrics_deferred                                           | Metrics lock was contended. Samples remain for a later rollup, so writer windows can span multiple loop intervals.                                               |
| writer_deferred_rollups                                           | Successful collection retained samples through this many earlier missed rollups. Writer counts are completion-cohort samples, never rates over interval_seconds. |
| idle_ratio                                                        | Loop wall-time idle fraction; active time includes synchronous dispatch. This is not process CPU utilization.                                                    |
| cpu_hydrate_ms, hydrate_calls                                     | Calling-thread CPU for complete build_model invocations, including commit-list resends.                                                                          |
| cpu_hydrate_child_ms, hydrate_child_calls                         | Separate scoped panel/Git/semantic worker CPU, including panel prefetch. Never child-process CPU.                                                                |
| cpu_process_ms                                                    | Whole-process CPU across all threads.                                                                                                                            |
| cpu_loop_ms                                                       | CPU used by the event-loop thread.                                                                                                                               |
| cpu_scan_ms                                                       | CPU used by scan-pool jobs.                                                                                                                                      |
| cpu_children_reaped_ms                                            | Child-process CPU, counted only when each child is reaped; a long-lived child appears once, at exit.                                                             |
| cpu_unattributed_ms                                               | Process CPU outside the loop thread and measured off-loop spans, such as gix-internal and deadline threads; floored at 0.                                        |
| malloc_arena_max, malloc_arena_cap_applied                        | Requested glibc arena cap from `THEGN_MALLOC_ARENA_MAX` (default 2; 0 = uncapped); applied is false off glibc Linux or if refused.                               |
| malloc_trims_idle, malloc_trims_scan                              | Trims after 10 s idle and after a scan-pool burst drains; both share one 30 s limiter.                                                                           |
| malloc_scan_drains                                                | Drained scan bursts, including ones throttled by the shared limiter.                                                                                             |
| malloc_trims_released                                             | Trims that returned memory to the operating system.                                                                                                              |
| malloc_trim_bytes                                                 | Bytes released by trimming; macOS only, always 0 on Linux.                                                                                                       |
| malloc_trim_total_us, malloc_trim_max_us                          | Cumulative time spent in trim calls and the longest trim call.                                                                                                   |
| resync_samples, resync_p99_us                                     | Count and cost of baseline reset/full-screen re-emission, independent of incremental/full composition classification.                                            |

The `malloc_*` counters are cumulative since process start, not per interval.
Trimming is available only on glibc Linux and macOS.

Writer aggregation is bounded and drained with try_lock: no success wake,
completion queue or profiler timer. Sink completion may represent a local
terminal or relay. It does not acknowledge physical paint or an application's
response. Hardware/terminal/reader buffering before host observation remains
outside coverage. Input without display damage can wait for unrelated frames.

**Background admission.** The separate `sched admission` event reports
`sched_admitted`, `sched_refused`, `sched_unavailable`, `sched_started`,
`sched_finished`, `sched_in_flight`, `sched_queued`, `sched_wait_samples`,
`sched_wait_mean_us`, `sched_wait_max_us`, `sched_service_samples`,
`sched_service_mean_us`, and `sched_service_max_us`. These are cumulative since
process start, not per interval. Wait and service samples are collected only
while perf diagnostics are enabled; zero samples means timing was unobserved,
including when work completed with diagnostics off. `sched_refused` counts every
saturated submission; the older scheduler debug counter counts only
replaceable refusals. This ledger excludes direct `spawn_blocking` and
`std::thread` workers. Its fixed storage is eleven `AtomicU64`s (88 bytes),
with no labels, locks, or timer. At most `BG_PERMITS` boxed closures can be
queued or running; each carries its permit and, only while timing is enabled,
up to two `Instant`s across queued and running work. `sched_in_flight` is
admitted minus finished, and `sched_queued` is admitted minus started.
Finished includes panicked work.

**LSP diagnostics queue.** The separate `lsp queue` event reports
`lsp_queue_observed`, `lsp_queue_admitted`, `lsp_queue_replaced`,
`lsp_queue_evicted`, `lsp_queue_rejected_capacity`,
`lsp_queue_stale_rejected`, `lsp_queue_purged_on_retire`,
`lsp_queue_delivered`, `lsp_queue_pending_documents`,
`lsp_queue_pending_bytes`, `lsp_queue_ready_roots`,
`lsp_queue_active_streams`, `lsp_queue_backlog`, and
`lsp_queue_oldest_ms`. Counters are cumulative since process start and reset
only when the diagnostics bus is shut down; the gauges are instantaneous
values at the rollup. `lsp_queue_backlog` is `unobserved` when the bus reports
age as unavailable, including when timing was off or pending entries were
admitted before timing began; an unknown age is never presented as an empty
queue. `lsp_queue_observed=false` means the bus could not be read. This
observation runs only during an existing perf rollup; it adds no timer,
thread, or wake source.

Version 2 fixes idle dispatch omission and preserves earliest pending input
instead of overwriting it. Those corrections and expanded model-call coverage
must not be compared to old rollups as if instrumentation were unchanged.
CPU guards record on completion; long work can cross windows. The first-frame
marker is now “first frame submitted”.

The opt-in host test **perf_workloads::controlled_perf_workload**, run alone with
--ignored --nocapture --test-threads=1 and isolated XDG environment, measures
actual configuration/environment-resolution and surface diff/resync functions
against fixed synthetic inputs. It does not benchmark complete hydration or
establish a live regression. No cadence/cache/resync policy changes are included.

The benchmark protocol that defines timing boundaries, fixtures, sampling and thresholds for comparing runs is in
[performance-benchmark-protocol.md](performance-benchmark-protocol.md).

Which background workers exist, what wakes them and what bounds them is inventoried in
[runtime-workers.md](runtime-workers.md).
