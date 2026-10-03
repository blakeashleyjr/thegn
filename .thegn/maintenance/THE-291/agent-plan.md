# THE-291 plan

Scope: thegn-metrics only. GPU helper commands (nvidia-smi --version, nvidia-smi query, ioreg) leave the
sampler's synchronous path.

- gpu_exec.rs: run_bounded (deadline, stdout cap, own process group, killpg + reap, status check, typed failure).
- gpu_monitor.rs: GpuMonitor. sysfs read inline (cheap). Helper backends: discovery + sample run on one short-lived
  background thread, one in flight (flag held until child reaped), latest-good cache, health (Pending/Ok/Stale/Absent),
  exponential backoff on failure, long retry for absence. sample() never blocks, never fabricates zero.
- sample.rs uses GpuMonitor in place of GpuProbe+last_gpu. No new config keys / API.
  Tests: fake sh scripts: startup hang, sample hang, infinite output, nonzero, malformed, grandchild kill, backoff.
  Out of scope: Windows job objects (THE-274), snapshot stale field, host hydrate.
