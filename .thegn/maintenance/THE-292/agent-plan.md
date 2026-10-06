# THE-292 plan

Scope: command collector in crates/thegn-host/src/metrics.rs (issue paths are stale: not thegn-metrics).
Approach: route collect_command through the THE-291 gpu_exec::run_bounded (exported as thegn_metrics::run_command_bounded): total deadline, caps, group kill, reap, no reader thread. Supervisor exits when the receiver is gone.
Tests: stdout-closed-then-sleep, descendant-held stdout, nonzero exit (existing: timeout, cap, missing).
Out of scope: latest-wins channel (touches run.rs), concurrent targets / cycle budget (design), Windows tree containment (THE-274).
