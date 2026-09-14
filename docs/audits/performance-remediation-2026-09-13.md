# Performance and log-tail remediation evidence

This implements THE-625/THE-626 under THE-614 and prepares a bounded investigation
for THE-627. It changes no running process, terminal input, profiler state,
configuration or live database. The approved build-script fix was copied from the
integrating worktree to avoid repeated linked-worktree recompiles.

Changes are bounded diagnostics: explicit submission/sink-completion boundaries,
sample and cohort counts, busy-loop dispatch coverage, hydration parent/child CPU,
phase timings on existing hydration records, and an independent resync cost.
Legacy numeric fields remain with metric_version=2 and documented coverage
corrections in [performance-metrics.md](../performance-metrics.md). Writer drains
never wait for the metrics lock; successful deferred drains report missed-rollup
counts, so they do not imply a single loop interval.

The log follower now handles rename/recreate, temporarily missing paths, detected
truncation and partial records. Retired-file draining is bounded to 64 KiB;
remaining old content can be omitted rather than starve the new file. Partial
records have a 1 MiB bound with an explicit truncation suffix. Copytruncate with
regrowth past the cursor between observations remains inherently ambiguous.

Completed verification before integration:

- Scoped Cargo svc log tests: 8 passed, 0 failed, before the final initial-partial
  record refinement. No live/mock service tests matched the filter.
- Exact-source isolated boundary harness: 20 passed, 0 failed after all follower
  refinements; production writer, timing and follower
  modules, plus actual histogram/error-classifier source excerpts. The harness
  stubs only profiler enablement as disabled; timing tests pass explicit stamps.
  It covers sink completion, failure/OOB exclusion, bounded queue, nonblocking
  metric drain and carryover, active/poll/rollup clocks, partial lines and rotation.
  Results and harness are retained in /tmp/thegn-performance-helper-tests-20260913/.
- Strict OpenSpec validation, rustfmt, idle-poll guard (including its 9 regression
  cases), ignored-result ratchet, brand guard, stale-doc guard and diff whitespace
  checks passed.

Pending central integration verification: full host typecheck/tests, the added
rollup/cohort count tests, render-plan invariants, compositor equivalence tests,
and the ignored controlled workload. Those are deliberately combined with the
other audit branches to avoid repeated host compilation.

The opt-in controlled workload has fixed 1/8/32 environment-resolution rows and
80×24, 160×48, 240×72 terminal surfaces. It runs actual config/environment and
diff/resync functions, verifies reconstructed cells, and reports sample counts
and medians. It does not invoke build_model, the live UI, a daemon, a remote
provider or a live benchmark. Its output must be attached after central execution;
no timings or speedup are claimed before that run.

THE-627 remains open for **full hydration attribution**. Calling build_model in a
fixture also discovers optional providers and enters housekeeping through process
globals; safely isolating that complete path needs a deliberate injection seam,
not merely a private DB argument. The scoped fixture and added phase/child CPU
coverage do not explain the original median 1,062 ms model-build wall time or prove
a regression. No cadence, cache policy, rendering algorithm or resync policy was
changed without that evidence.
