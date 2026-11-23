# Prove macOS process-group drain for bounded Git probes

## Why

On macOS, `killpg(pgid, 0)` can continue to observe a process group while its
leader is an unreaped zombie. The bounded Git identity probe must keep that
leader pinned until it is safe to signal the group, so a successful short Git
command can otherwise look like it has descendants holding stdout open.

## What Changes

- Determine macOS process-group liveness from libproc member states, treating
  zombie-only groups as drained and uncertain observations as live. Ignore the
  unreaped leader only when owned `waitid(WNOWAIT)` proves that exact PID exited.
- Keep the leader pinned through drain proof and preserve bounded termination,
  reap, pipe, and fail-closed behavior.
- Add deterministic Darwin state coverage and portable supervisor regressions.

## Impact

The process supervisor can now prove a successful macOS Git probe drained
without weakening the caller's output or time bounds. No Git guard is bypassed
or lengthened.
