## ADDED Requirements

### Requirement: Git identity probes are bounded and prove process-group drain

A local Git identity probe SHALL return captured stdout only after its leader is
reaped successfully, its stdout pipe reaches EOF, and its owned process group is
proven drained. On macOS, zombie-only process groups SHALL count as drained
while the leader remains pinned for safe signalling. Live descendants and
incomplete process-state observations SHALL remain owned by the bounded
supervisor and SHALL fail closed if they do not drain within its existing
limits.

#### Scenario: Short Git command exits with only a zombie leader

- **WHEN** a successful bounded Git identity command exits on macOS and no live
  process remains in its group
- **THEN** the supervisor proves the group drained, reads EOF, and returns its
  bounded stdout

#### Scenario: Git descendant keeps stdout open

- **WHEN** a process in the Git command's group remains live after the leader
  exits
- **THEN** the supervisor continues owning it within the existing deadline and
  does not return successful identity output until the group drains

#### Scenario: Darwin process state cannot be inspected

- **WHEN** libproc cannot provide complete process-group membership or state
- **THEN** the supervisor treats the group as live and retains its existing
  bounded termination and fail-closed behavior
