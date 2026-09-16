# Design

A single reserved process-wide slot owns the exact OS thread independently of
run-future cancellation, early errors and unwinding. Reservation Drop installs a
successfully spawned handle even with a poisoned lock; poison refuses new
admission. Finished handles retire under a token but join outside locks. A
completion receipt does not prove `JoinHandle::is_finished`; a shared application
cleanup deadline yields Held while custody persists. No reaper thread or hidden
timer is introduced. Opaque panic payloads cannot withhold failure publication
or strand the retirement token. An exceptional join-error payload remains in one terminal Failed custody slot;
new admission is refused without invoking arbitrary Drop on cleanup callers.

The worker parks on a condition variable without a timeout when hidden. Enabled
starts retain the existing two-second minimum across resets. A generation and
reset epoch revoke in-flight results and preserve rapid hide/reopen resets.
One unpublished snapshot is replaced in place; only empty-to-nonempty wakes.
Pause acknowledgement follows prior collection, wake and reset completion.
Wake errors/panics revoke future publication. Fixed failure receipts precede
payload destruction and one guarded terminal wake makes early failures visible.

The compositor revokes hidden demand at the next loop entry and before final
take. Attribution inputs are published before positive admission. Hydration
moves the snapshot and its publication revision together. Existing resident and
sampler shutdown requests start before either wait and share the same deadline.

Subsequent stages cache process rows by the accepted publication revision and
view inputs, and worktree disk ordering by semantic data revision. Graph window
progression and displayed measurement age remain independent. Processes alone
opt into viewport-derived fixed columns; sanitized cell text is both measured
and clipped. Selection and signal authority continue to use sampled PID/birth.

Performance evidence uses the same release host-test workload and optimized
collector dependencies before/after: acknowledged 120s hidden/visible/paused
windows, worker counters separated from consumer timers, contextual CPU/RSS,
and fixed-input actual monitor refresh/render timings. This excludes the full
compositor/terminal. Owned Muse frame verification separately inspects UI output.
