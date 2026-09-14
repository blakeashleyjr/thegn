## ADDED Requirements

### Requirement: Resident plugin admission and shutdown preserve bounded ownership

The resident runtime SHALL bound outgoing serialized frames, queued frame count
and queued bytes before admission. A caller SHALL NOT perform OS pipe I/O or wait
for a child while sending a resident callback, response or request. Close control
SHALL remain available when data admission is full.

The application SHALL retain lifecycle task and process/pipe custody across
reloads, cancellation and shutdown. Shutdown SHALL request all sessions before
waiting against one shared absolute deadline outside the compositor loop.
Receipts SHALL distinguish leader reaping, pipe settlement and unproven tree
containment. An unresolved owned resource or owner join failure SHALL NOT be
reported as successful cleanup. Final release SHALL only request termination
through still-owned identity and SHALL NOT block the UI waiting for reaping.

#### Scenario: Plugin stops reading stdin
- **WHEN** a resident plugin stops consuming stdin and its admission queue fills
- **THEN** subsequent sends fail with a bounded admission error
- **AND** close reaches the owner without acquiring a pipe writer mutex

#### Scenario: Plugin closes stdout but stays alive
- **WHEN** stdout reaches EOF while the leader remains alive
- **THEN** the session closes admission and starts bounded owned cleanup
- **AND** an unbounded child wait cannot prevent termination admission

#### Scenario: Shutdown waiter is cancelled
- **WHEN** a caller cancels a shutdown wait while the owner is still running
- **THEN** the lifecycle JoinHandle remains in supervisor-owned state
- **AND** later shutdown waiters can observe that same owner

#### Scenario: Natural exit includes a final provider reply
- **WHEN** a plugin writes a complete provider reply immediately before exiting
- **THEN** the bounded output drain routes the reply to its originating bridge
- **AND** late responses cannot satisfy a replacement session's correlation id

#### Scenario: Windows cancellation precedes synchronous I/O
- **WHEN** a cancellation request arrives after the pipe worker checks its close latch but before its I/O syscall
- **THEN** shutdown repeats cancellation against that still-owned thread until completion or the deadline
- **AND** ERROR_NOT_FOUND alone is not treated as settlement
