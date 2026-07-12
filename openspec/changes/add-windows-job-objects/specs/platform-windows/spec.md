# Platform: native Windows

## MODIFIED Requirements

### Requirement: Process control routes through the platform seam

Pid liveness probes, best-effort termination, grouped spawns and tree kills,
stderr redirection, and shutdown-signal installation SHALL go through
`thegn-host`'s `platform` module. Grouped spawns use one shape on both
platforms — `spawn_grouped` returns the child plus a cloneable `GroupHandle` —
where unix keeps today's pgid semantics (`setpgid` + `killpg(SIGTERM)`) and
Windows assigns the child to a kill-on-close Job Object
(`TerminateJobObject` for explicit kills). On Windows, dropping the last
`GroupHandle` MUST also reap the tree (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`),
and a failed job assignment MUST degrade to direct-child termination rather
than failing the spawn. Termination on Windows is hard (no SIGTERM window);
call sites that rely on child-side cleanup are cancel-and-discard paths.

#### Scenario: Superseded test run is reaped whole

- **WHEN** a newer test run supersedes an in-flight `cargo test` (or its
  watchdog deadline passes) on native Windows
- **THEN** terminating the slot's `GroupHandle` kills the runner *and* every
  test binary it spawned, immediately

#### Scenario: Host death leaves no orphans

- **WHEN** the thegn process dies while a grouped child tree is running on
  native Windows
- **THEN** the job's kernel handles close with the process and the whole tree
  is reaped by KILL_ON_JOB_CLOSE

### Requirement: Unix-substrate features stub with explicit errors on Windows

Features whose substrate is inherently unix — the sealed-sandbox model relay
(its consumers are Linux containers that bind-mount the socket), the
merge-queue headless agent (POSIX `sh_quote` templating), the SIGUSR2
profiler, `thegn debug` exec-replace, and the ACP unix-socket transport —
SHALL return an explicit error (or logged warning, for best-effort paths) on
Windows rather than silently no-op or panic. The pane daemon, control client,
and the profile singleton lock are NOT in this set: the daemon IPC runs over
named pipes and the singleton lock uses std's cross-platform `File::try_lock`.

#### Scenario: Singleton detection on Windows

- **WHEN** a second `thegn` launches for a profile whose compositor is live on
  native Windows
- **THEN** `instance_running` reports the live instance via the held file
  lock, the same as on unix
