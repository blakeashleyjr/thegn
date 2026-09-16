# THE-628 process refresh investigation and candidate fix

Primary root cause: `run.rs` takes and restores independent stats, containers,
metrics and dispatches around `model = next_model`, but omitted `model.procs`.
Hydration builds an empty process snapshot. The next stats-driven monitor rebuild
therefore renders “sampling…” and clamps selection before the independent 2s
process sampler fills the table again. This is a publication ownership bug,
not evidence that process sampling should run more frequently.

Secondary deterministic findings: `ProcSampler` used unstable top-N CPU/RSS
partitioning without a secondary identity ordering, so a tied cutoff depends on
OS process-map enumeration order. The view retained numeric selection across
rank changes. Candidate changes preserve the sampler snapshot only at the
actual hydration swap, deterministically choose tied retained processes and
view rows, and preserve selected `(pid, start_time)` on passive refresh.

The primary reviewer approved this plan before implementation, requiring sampled
identity also bind pending signal confirmation and TERM→KILL escalation. The
candidate refuses a confirmation whose sampled identity disappeared/changed.
It does not establish an OS process handle or eliminate between-sample PID-reuse
races; birth time is sysinfo's second-resolution sample. No signal was sent to a
live process during investigation/tests. The confirmation refusal fixture uses
PID0, which the platform rejects before OS signaling even if the identity guard
regresses.

Validation completed in an isolated rustc harness importing the exact production
`thegn-metrics/src/procs.rs` and `monitor/procs_view.rs`, linked to cached real
sysinfo: **13 passed**. It does not mock process admission or view implementations.
No process enumeration is invoked by these tests. Retained membership tests
shuffle 100 tied entries across the top32 cutoff; flat/tree tests vary source
order, sort key and direction. A controlled counterfactual removing only the
new secondary comparisons fails both tie regressions (**0 passed, 2 failed**).
Logs: `/tmp/thegn-process-refresh-harness-tests.log` and
`/tmp/thegn-process-refresh-before-tests.log`.

Eight additional host monitor fixtures cover repeated hydration/sample
interleaving, passive rank changes, manual scrolling, disappearance and PID
reuse, signal prompt identity and escalation, explicit-sort reset, and paused
refresh. They passed in the assembled host checkpoint: all 79 monitor tests and
three model-equality tests passed on September 13. Rendering remains pure, the existing idle render
gate is unchanged, and no new sampling wake/timer was introduced. No visual
end-to-end run or live perceived-smoothness claim is made yet.

Independent BTOP review corroborated keeping the 2s collection cadence and
separating input redraw from collection. Adjacent follow-ups: hidden sampler
half-tick thread wakes, rebuilding unrelated tab rows on every status refresh,
and content-sized table column jitter. They are excluded from this focused fix.

Primary source review, independent adversarial review and the focused host
checks are complete. Combined final gates and canonical landing remain pending. Canonical Git metadata is read-only in this session;
changes are isolated in the private remediation clone.
