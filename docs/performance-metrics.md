# Runtime performance metric boundaries

Rollups with **metric_version=2** retain older numeric keys for consumers.
Histogram percentiles remain lower bucket edges, not exact times; zero also
represents 0–1 µs samples. Always use the sample count. **interval_seconds**
reports the actual interval, which can exceed configured cadence because
diagnostics add no timer or wake.

| Fields | Boundary and coverage |
| --- | --- |
| render_p*_us, frame_samples | Host composition through encoding/submission; synchronous rendering includes sink I/O. |
| flush_p*_us | Legacy name for encoding/submission; not async flush time. |
| input_p*_us, input_samples | Earliest pending host observation to next frame submission. A sample covers a coalesced cohort, not necessarily an application response. |
| input_events, input_coalesced_events | Dispatched key/mouse/paste events and those joining a pending cohort. Preemption observations are not extra dispatches. |
| writer_queue_p99_us, writer_write_p99_us | Queue wait before a successful frame write and duration in sink write+flush. |
| writer_completion_p99_us, writer_frames | Submission to successful sink completion and its sample count. |
| writer_input_p99_us, writer_input_samples | Submitted cohort's earliest observation to successful sink completion. |
| writer_failed_frames | Timed writes that failed, excluded from completion histograms. OOB and frames discarded after fatal errors are not successful frames. |
| writer_metrics_deferred | Metrics lock was contended. Samples remain for a later rollup, so writer windows can span multiple loop intervals. |
| writer_deferred_rollups | Successful collection retained samples through this many earlier missed rollups. Writer counts are completion-cohort samples, never rates over interval_seconds. |
| idle_ratio | Loop wall-time idle fraction; active time includes synchronous dispatch. This is not process CPU utilization. |
| cpu_hydrate_ms, hydrate_calls | Calling-thread CPU for complete build_model invocations, including commit-list resends. |
| cpu_hydrate_child_ms, hydrate_child_calls | Separate scoped panel/Git/semantic worker CPU, including panel prefetch. Never child-process CPU. |
| resync_samples, resync_p99_us | Count and cost of baseline reset/full-screen re-emission, independent of incremental/full composition classification. |

Writer aggregation is bounded and drained with try_lock: no success wake,
completion queue or profiler timer. Sink completion may represent a local
terminal or relay. It does not acknowledge physical paint or an application's
response. Hardware/terminal/reader buffering before host observation remains
outside coverage. Input without display damage can wait for unrelated frames.

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
