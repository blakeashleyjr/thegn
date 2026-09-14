# Full model hydration workload

The actual `hydrate::build_model` workload passed for **1, 8 and 32 private Git
worktrees**. This extends THE-627's earlier configuration/environment and surface
diff measurements. It does not establish a release regression or explain the
entire live-build slowdown.

The first assembled batch-02 host test graph compiled in 3m52s, at private
candidate `776e812e` plus the reviewed fixture corrections subsequently committed
in `67dc8805`. The copied executable is
`/tmp/thegn-batch02-test-binaries/thegn-host-checkpoint-1`. It is an **unoptimized
test build**. Each worktree has 64 tracked text files and one modified file.
Every sample asserts one active-panel change and the expected sidebar row count.
There are nine sequential samples per helper condition; the table reports the
median of samples 1–8, excluding sample 0. Each worktree count runs in a fresh
process. The helper-absent condition always runs first, so these are not
randomized causal comparisons.

| Worktrees | Helper present | First build ms | Later median ms | Parent CPU median ms | Scoped-child CPU median ms | Helper calls / 9 builds |
| --------- | -------------- | -------------- | --------------- | -------------------- | -------------------------- | ----------------------- |
| 1         | No             | 117.28         | 63.29           | 46.74                | 9.05                       | 0                       |
| 1         | Fixture        | 71.89          | 76.69           | 47.93                | 9.78                       | 9                       |
| 8         | No             | 157.66         | 79.99           | 62.35                | 9.87                       | 0                       |
| 8         | Fixture        | 90.81          | 91.22           | 63.22                | 10.04                      | 9                       |
| 32        | No             | 621.45         | 157.75          | 124.01               | 12.84                      | 0                       |
| 32        | Fixture        | 186.26         | 179.51          | 126.69               | 13.31                      | 9                       |

The larger worktree set increases parent-thread work in this fixture. Scoped
child CPU is accounted separately and excludes external subprocess CPU. The
first build can include cold caches; later builds run closely together and can
reuse the existing glyph cache, so they do not model five-second-spaced live
refreshes. Activity sampling still reads the host's process table; background
machine load and concurrent compilation can affect timings. No thermal or
whole-machine CPU attribution is made.

The counting helper confirms a specific source finding: unrelated model builds
run `devcontainer --version` once per build whenever that executable is on PATH.
The fixture helper immediately prints a version and records a private counter;
it neither starts a container nor contacts a daemon. This is tracked as
[THE-633](https://linear.app/blakeashley/issue/THE-633). Its eventual fix should
measure the same fixture before and after, preserving relevant probe invalidation.

Execution uses a cleared inherited environment, private XDG configuration/state
and THEGN_DIR, a private PATH containing only Git, a shell, and the optional
fixture helper, and fresh real Git repositories with signing disabled only for
fixture commits. Assertions disable placement/lifecycle/provider housekeeping
and verify that the database is in the fixture root. No live Thegn session,
plugin, provider resource, or Git checkout is changed. HOME is not repurposed;
generic read-only discovery is not claimed to be a fully isolated operating
system benchmark.

The Unix-only fixture now lives in
`crates/thegn-host/src/platform/perf_workloads_hydration.rs`, following the
repository's platform ratchet. Its final test name is
`platform::unix::perf_workloads_hydration::controlled_full_hydration_workload`.
The first executable used the earlier module name
`perf_workloads::hydration::controlled_full_hydration_workload`; relocation does
not change the workload, but the final assembled graph must still compile it.
Set THEGN_AUDIT_WORKTREES to 1, 8 or 32 and run this ignored test alone with
`--exact --ignored --nocapture --test-threads=1` in a fresh cleared environment.

Raw outputs are `/tmp/thegn-audit-full-hydration-{1,8,32}.log`. They contain all
samples, CPU measurements, helper counts and test outcomes. The canonical audit
artifact directory preserves copies with the remediation reports.
