# Add native Windows support, phase 3: Job Objects process lifecycle

## Summary

Phase 1's platform seam scoped Windows process-tree kills to the direct child
(`TerminateProcess(pid)`), so a cancelled `cargo test` could leave its test
binaries running. This change upgrades the seam to **kill-on-close Job
Objects** and unifies the spawn shape on both platforms behind one call:

- **`platform::spawn_grouped(&mut Command) -> (Child, GroupHandle)`** — unix
  puts the child in its own pgid (exactly today's `process_group(0)`);
  Windows spawns, creates a Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and assigns the child. Best-effort:
  a failed job assignment degrades to the direct-child handle rather than
  failing the spawn.
- **`GroupHandle::terminate()`** — `killpg(SIGTERM)` / `TerminateJobObject`.
  Cloneable (`Arc` on Windows) so watchdog threads and the task registry share
  it. On Windows, dropping the **last** handle also reaps the tree — orphan
  hygiene beyond unix pgids: a thegn that dies mid-merge takes its spawned
  trees with it.

Call sites rewired: the task/test runner (`task.rs` registry now stores
`(generation, GroupHandle)`; cancel/supersede/watchdog terminate through it)
and the merge-queue agent watchdog (`merge_driver.rs`). Semantics note
documented at both: Windows termination is hard (no SIGTERM window), so
child-side cleanup that unix relied on does not run — acceptable for test
runners and superseded agents, which are cancel-and-discard paths.

Drive-by parity fixes surfaced by a warning sweep of the windows target:
`profile.rs` singleton lock (`acquire_singleton`/`instance_running`) is now
cross-platform via std's `File::try_lock` (the Windows "no singleton
detection" stub is gone), and stale `cfg` gaps in `merge_driver`/
`desktop_notify`/`GitLock` were tightened so the windows-gnu workspace check
is **warning-free**.

## Impact

- tasks.md AX 731.
- Crates: `thegn-host` (`platform/{unix,windows}.rs`, `task.rs`,
  `merge_driver.rs`), `thegn-core` (`profile.rs` singleton ungated,
  `util.rs` GitLock field note).
- CI: the windows job runs the Job Object semantics tests on a real kernel
  (terminate-reaps-tree, drop-reaps-tree).
