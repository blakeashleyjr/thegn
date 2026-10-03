# THE-291 — primary instructions (batch 10, codex restart)

A previous agent implemented THE-291 (commit 22c019dd6: crates/thegn-metrics gpu_exec.rs bounded runner,
gpu_monitor.rs GpuMonitor, gpu.rs, sample.rs) and was mid-way through review fixes when it died; its
UNCOMMITTED working-tree changes may be partial. First `git diff` to see them; keep what is correct,
finish the rest. Issue (data): isolate and bound GPU helper probes (nvidia-smi / ioreg) from the global
refresh scheduler — deadlines, output caps, process-tree kill+reap, single in-flight probe, backoff,
last-good cache, never fabricate zero.

Apply ALL of these review findings:

- B1 BLOCKER: test/platform-cfg-metrics-ratchet.txt pin for gpu.rs became stale (only #[cfg(test)] left).
  Restore the macOS live test in gpu.rs under #[cfg(target_os = "macos")] (GpuMonitor::new(),
  sample(true), wait for idle, assert Live and util <= 100) so the pin stays valid.
- M2 (perf, important): a MISSING helper (spawn ENOENT / not_found) must be cached Absent for the process
  lifetime (as before this change), or detected by a PATH stat walk with no spawn. Keep the 10-minute
  retry only for a present-but-failing `--version`. No new idle spawn source on non-NVIDIA machines. Test it.
- M1: clean-exit sweep must never killpg after reaping the leader (pgid reuse). Observe exit with
  waitid(WEXITED|WNOWAIT) (libc::waitid where nix lacks it; see gate_child_exited in
  crates/thegn-host/src/platform/sound_process.rs), killpg only if members remain, THEN child.wait().
- M3: add a fake-sh test for the try_ioreg=true discovery branch (Spawn/Malformed -> Absent, Timeout -> Failing).
- M4: success-path test deadlines 2–5 s (short deadlines only in hang tests); relax the 150 ms sample bound to ~1 s.
- L1: replace the detached reader thread with a poll(2)-based read on the probe thread against the deadline.
- L2: a guard owning the Child that kills the tree and waits on Drop unless disarmed; document process-exit-mid-probe.
- L3: distinct failure reason for a probe-thread panic.
- L4: gate test-only helpers (probes_started, wait_idle -> all(test, unix); test_support::gone -> linux).
- L5: fix thermal.rs intra-doc link and comment that still reference GpuProbe.
  Do not touch crates/gtui-query, gtui-app or thegn-host. Platform #[cfg] only in pinned files.
