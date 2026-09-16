# Task manager maintenance: THE-630 / THE-631 / THE-632

This existing-maintenance bundle starts from main `6884c3f0`. Production changes
are in `453762af`; `b3892624` corrects one test fixture publication revision.
No collectors, settings, controls or other features are added. THE-633 and
THE-640 fixes already on main are preserved.

## Implementation and correctness

The process sampler is an owned worker with an indefinite hidden/paused park,
a two-second minimum between scan starts, generation/reset fencing and one
replaceable publication. Process-wide custody retains an unfinished OS-thread
handle across cancellation and unwind. Shutdown shares the resident supervisor's
deadline and distinguishes settled, failed and held outcomes; it never promises
to interrupt an OS collection. Failure publication precedes opaque panic-payload
destruction. Pane/daemon attribution is written before positive admission.

Process row caches use the accepted publication revision and complete
sort/direction/tree/filter inputs. Hydration transfers snapshots and revisions
together. Disk cache revisions cover both size and timestamp maps; cached ages
advance independently and exhausted revisions disable reuse. Unrelated graph
refreshes retain their cadence without rebuilding process or disk rows.

Processes alone use viewport-derived column widths and sanitized grapheme
clipping. Selection and signal confirmation preserve PID/birth-time identity.
Paused resize reflows geometry without consuming a new sample. Fixed rows avoid
duplicate painting, and layer remapping reuses equal default/color attributes.
Native fixtures cover control races, failure custody, invalidation, clipping,
selection and attribute equivalence.

## Current-source validation

Release build `453762af` completed in 2800.285 seconds. Its 68 selected controls
produced **67 passes and one failure**: the sort fixture changed RSS without
advancing the process publication revision. The failure is retained. The single
added line in `b3892624` models that publication boundary; all production and
common benchmark bytes remain identical. A separate debug host/test build
completed in 613.910 seconds, and **all 68 focused controls passed** across
21 groups. This is not a claim that the old release artifact passed 68 tests or
that the two binaries are equivalent.

Scoped debug Clippy exited zero with no errors. The stricter zero-warning
wrapper failed on five records: two copies of an unchanged-main `hydrate.rs`
collapsible-if warning, and three warnings in the immutable common benchmark
fixture (two intentional release-only assertions and a fixture initializer).
Primary and independent review accepted exactly that inventory by source/hash.
The raw failure remains unchanged; no suppressions, warning-free claim or lint
rerun is used. This is scoped host lint, not a full-workspace lint claim.

Owned Muse verification passed at 80x24, 160x48 and 240x72: paused stability,
selection/navigation, sort-arrow agreement, tree/filter transitions with an
actual exclusion witness, Unicode cell/column alignment, resume/reopen and
normal exit. Every owned app/pane/helper/daemon exited; host logs contained no
review-required entries. PNG, styled, text and trace evidence is retained.
This supports terminal-cell behavior, not arbitrary font shaping or other OSes.

## Release measurements and limitations

The declared CPU2 series ran warmup A/B followed by ABBAABBA renderer invocations,
then one matched sampler A/B pair with 120-second hidden/visible/paused windows.
It completed once in 12m28s, using 33.071 total CPU seconds. Workloads,
instrumentation and comparison thresholds were unchanged; no retry was used.

Hidden and paused scheduled sampler wakes each fell **240 to zero**; both
candidate windows had zero scans, publications and terminal wake attempts.
Visible sampling produced 60 completed/consumed samples in 120.058 seconds;
reopen produced unprimed then primed samples. Whole-test-process CPU seconds were
hidden 0.02 to 0.01, visible 6.44 to 6.37, paused 0.02 to 0.02. These counters
include fixture polling and all threads, have 10ms resolution, and do not prove
worker-only CPU cost or statistical significance.

All 24 renderer-series paired changed-frame medians improved (6.77-54.53%);
the extra sampler workload's six renderer medians also improved. Across the
four measured runs per side, the descriptive median of run summaries improved
for both median and p95 at all six shapes. These are not pooled quantiles.
Changed-frame allocation calls fell 21.47-33.62%. Unchanged Process refreshes
reported no dirty frame and performed zero process/disk/body rebuilds.
The unrelated CPU-tab workload reduced process and disk row builds from 500
each to zero while preserving its 500 graph-body builds.

Tail qualifications remain explicit: renderer pair 3 p95 increased at 64 rows
for 80/160/240 columns (+36.33%, +40.35%, +48.13%); pair 4 increased at 64/240
(+12.62%) and 400/80 (+11.13%). The sampler workload's extra 64/80 render p95
increased 27.58%. The prior frozen candidate's +24.56% renderer and +10.78%
lifecycle flags are also retained. No CPU/load cause is established. Primary
review accepts the consistent measured median/CPU/allocation improvements and
functional repair with this tail variability; it does not claim every p95
improved or provide a universal latency guarantee.

## Evidence and landing boundary

- Release source/build, source bridge, exact series and primary comparisons:
  `/tmp/thegn-current-port-build-20260916-sowgq_pv`.
- Corrected debug build, 68 controls, raw lint and separate warning disposition:
  `/tmp/thegn-final-debug-build-20260916-hr0ltkff`.
- Owned UI and exact cleanup: `/tmp/thegn-muse-procs-xnlah7xz`.
- Independent source review:
  `/tmp/thegn-existing-monitor-source-review-luna-20260916.md`.
- Independent fixture, lint and UI reviews:
  `/tmp/thegn-final-fixture-source-bridge-review-luna-20260916.md`,
  `/tmp/thegn-final-lint-disposition-review-luna-20260916.md`,
  `/tmp/thegn-final-ui-review-luna-20260916.md`.
- Earlier frozen measurements and retained adverse observations:
  `/tmp/thegn-layer-host-build-20260916-1m4i32rq` and
  `/tmp/thegn-frame-stage-resume-20260916-gvm3bab7`.

Landing uses native `merge add` then `merge land`, with the configured isolated
`XDG_STATE_HOME=/home/blake/.superzej/pipeline-state just test` gate, one build
job and one test worker. The receipt selected by
`/tmp/thegn-final-native-queue-latest-20260916` is authoritative for the final
full-workspace test result and landed main commit. The proposed delivery-ledger transition takes effect with successful native
landing; this source record does not preclaim a gate result. Existing historical receipts are retained; current
acceptance is based on the sources and measurements identified above.
