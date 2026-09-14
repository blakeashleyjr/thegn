# Design

The process sampler is the sole publisher of `ProcSnapshot`. Model hydration
owns Git/DB data and cannot replace that snapshot. Move the current snapshot
into the incoming model immediately before the authoritative `model = next_model`
swap; do not re-seed it from stale arbitrary model copies. Existing config
`procs_disabled` remains hydration-owned and still controls display/sampling.

Apply PID tie-breaks inside both top-N CPU and RSS admission comparisons. Applying
them only when sorting visible rows is too late to stabilize cutoff membership.
The union remains bounded to at most twice the configured retained count. Flat
and tree views use the same deterministic PID secondary ordering.

Use sampled `(pid, start_time)` identity when a passive refresh rebuilds the
process rows. Preserve the selected process if still retained; otherwise retain
and clamp the old index. Existing explicit sort/filter/navigation behavior stays
unchanged. Existing cursor-follow policy brings the selection into view, while a
wheel-scrolled viewport keeps its position independently. Actual CPU/rank changes
still reorder the list; this does not introduce rank freezing or smoothing.

Capture the same sampled identity in pending signal confirmation and TERM→KILL
escalation state. A confirmation whose identity vanished or changed in the latest
displayed snapshot is refused and requires another selection. This is a sampled
identity guard, not an OS process handle: it does not eliminate PID reuse between
the latest sample and the signal syscall, nor second-resolution birth-time
collisions. No new claim of race-free signaling is made.

Sampling stays on its existing background worker and cadence; no event-loop I/O,
poll timeout, additional timer, or render-plan behavior changes. Adjacent costs
(dynamic table widths, hidden worker half-tick sleeps, unnecessary other-tab row
builds) remain separate follow-ups.
