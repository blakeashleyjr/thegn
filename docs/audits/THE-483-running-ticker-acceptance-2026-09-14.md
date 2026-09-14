# THE-483 running shared ticker acceptance

The original remediation tested arithmetic and scheduling helpers but did not start the shared worker. This follow-up exercises its real production spawner and loop through owned clock/I/O boundaries.

## Implementation and review boundary

`hydrate_refresh_ticker` contains the existing production scheduling loop and its one OS-thread spawn. A private statically dispatched adapter moves ambient sampler, daemon, container, wall-clock, QoS and sleep effects out of that loop. Live startup retains its 500ms sleep, startup priming order, send order, best-effort initial sends, terminal-wake coalescing and unwind-only diagnostic. No new timer, dynamic dispatch or adapter allocation is added. Production callbacks retain existing channel-close behavior.

The fixture uses the same spawner and loop, actual Tokio refresh channel, a one-permit synchronous clock, acknowledged ticks, a finite wall deadline and retained thread handle. Its adapter opens no database, sampler, credentials or providers. Clock disconnection releases the worker on normal shutdown or assertion unwinding. Every fixture waits for completion and joins; its unwind path avoids a second panic if the worker itself failed.

The hostile matrix projects real programmatic Config values through the startup accessors for CI, PR queue, calendar global interval, calendar account override, usage and weather. Each family and all families receive both 2^61 and u64::MAX (14 workers). Later Pr/MainRefMoved at ticks40/80, HostHeal30/60, Model78, normal CI when another family is hostile, priming order, daemon cadence and aggregate wakes are observed. Two further workers verify disabled optional requests stay absent and normal startup/periodic requests coalesce. The bounded nonzero numeric guarantee remains covered by the existing exhaustive cadence primitive fixture; the short running-worker fixture establishes survival, not a simulated31-day expiry.

## Validation evidence

- Source extraction harness, debug: six tests pass (three actual new thread fixtures, two existing scheduler tests, existing panic-notification test).
- Same harness with optimized worker compilation: six tests pass.
- Original unchecked PR-queue multiplication restored only in a private harness: debug hostile-worker fixture fails with `attempt to multiply with overflow`, then observes the real output/ack channel close. The other five tests pass.
- The same original mutation in optimized mode survives and all six tests pass. Historical source already used `is_multiple_of`, whose zero-divisor behavior silently disables that periodic request; it does not panic. This counterfactual therefore demonstrates debug worker-death detection only. Existing debug/optimized nonzero conversion tests independently cover wrapped-zero arithmetic.
- Full actual host compilation at combined source38107f9d succeeded. The combined host receipt reports299/299 passed, including all three `hydrate::refresh_ticker::tests` fixtures. Parent also reports44 focused core time/calendar/atomic tests passed. Actual host receipt: `/tmp/thegn-rolling-host-20260914-results.json`.

The isolated harness extracts the actual worker function and cadence helper bodies, includes the actual repository fixture source, and links cached core/Tokio dependencies. Its minimal event enum excludes unrelated host variants; it is not a whole-host release build. Logs are `/tmp/thegn-ticker-{real,overflow}-worker-{debug,optimized}-{build,tests}.log`; reproducible extraction/build script is `/tmp/thegn-ticker-harness-build.py`. Production files were never mutated for the counterfactual.

Independent source review accepted the production ordering/priming/wake preservation after correcting a fixture omission for the legitimate Issues event at tick120. The actual host gate now passes. Final primary landing approval remains required.
