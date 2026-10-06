# THE-652 plan

Scope: replace per-worktree std::thread::scope spawn in hydrate.rs glyph scan with a shared bounded pool (crates/thegn-host/src/scan_pool.rs).
Approach: lazy workers (cap clamp(ncpu/2,2,6)), exit on drain (no idle threads/timers), urgent=active worktree pushed to queue front, FIFO rest; panic/spawn failure => None => degraded row via merge_glyph_scan with all Err (keeps last-known, never clean). QoS Utility. No lock held across jobs/waits.
Tests: scan_pool unit tests (cap for 1/8/32/100, overlapping invocations share cap, panic, urgent order).
Out of scope: benchmarks, hydrate caching, per-repo coalescing.
