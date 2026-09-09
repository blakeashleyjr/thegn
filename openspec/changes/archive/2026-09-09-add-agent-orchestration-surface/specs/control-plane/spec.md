# Control Plane

## ADDED Requirements

### Requirement: Sessions report an activity state and support conditional waits

The daemon SHALL track a per-session activity state — blocked, working, done,
idle — derived from output, attention signals, and process state, published
edge-triggered on the event feed. A `sessions.wait` capability SHALL block
until the session reaches a named condition (exited, idle, blocked, done, or
output matching a regex) or a caller-supplied timeout elapses, reporting which
fired. An idle wait MUST NOT fire on a just-spawned session that has never been
busy, an output-match wait MUST consider retained scrollback at registration
time, and no state transition may be lost between a waiter's registration and
its first level probe.

#### Scenario: Waiting for a worker to finish

- **WHEN** a caller waits on a session with a done condition and a timeout
- **THEN** the call returns when the session reports done, or returns
  unmatched when the timeout elapses first, and says which happened

#### Scenario: A fresh spawn does not satisfy an idle wait

- **WHEN** a caller waits for idle on a session that has produced no activity
  yet
- **THEN** the wait does not fire until the session has been busy and then
  gone quiet

#### Scenario: A blocked agent is observable

- **WHEN** a session's agent asks for input and the attention signal fires
- **THEN** the session's state reads blocked until user input clears it, and
  waiters on blocked are woken

### Requirement: Dead sessions leave readable tombstones

When a session's process exits, the daemon SHALL retain a tombstone — exit
code and final screen — so a late `sessions.wait` or `sessions.snapshot` still
answers instead of returning not-found. The tombstone MUST be recorded before
the session's exit is announced, and a waiter whose session dies mid-wait MUST
receive the exit code the tombstone holds.

#### Scenario: A late poller reads the corpse

- **WHEN** a caller snapshots a session that exited before the call
- **THEN** the response carries the final screen and exit code rather than a
  not-found error

#### Scenario: A mid-wait death reports its exit code

- **WHEN** a session dies while a caller is waiting on it
- **THEN** the wait resolves with the session's exit code, whichever condition
  was being waited on

### Requirement: Issue and dispatch orchestration are catalog capabilities

thegn SHALL expose orchestration operations as capability-catalog rows
projected across the control surfaces: listing and reading tracker issues
(read scope), updating and commenting on issues (write scope), listing the
agent-dispatch roster (read scope), recording and re-statusing dispatches
(write scope), and creating a worktree (git scope) — optionally from an issue
id, deriving the branch from the tracker's branch hint and linking the issue.
Each row MUST be gated by `required_scope`, implemented on each surface or
recorded as an explicit gap, and MUST work with no agent configured (the
operations are plain tracker, git, and roster reads/writes).

#### Scenario: A supervisor enumerates the board and the roster

- **WHEN** a caller with read scope lists issues filtered by status and lists
  dispatches
- **THEN** both return machine-readable rows from the tracker router and the
  durable roster, without spawning anything

#### Scenario: Creating a worktree from an issue

- **WHEN** a caller with git scope creates a worktree naming an issue id
- **THEN** the branch derives from the tracker's branch hint (with the naming
  fallback), the worktree is registered, and the issue is linked to it

#### Scenario: A write without write scope is refused

- **WHEN** a caller whose scope set lacks write invokes an issue update or a
  dispatch status change
- **THEN** the operation is refused naming the missing scope, on every surface
  that projects it
