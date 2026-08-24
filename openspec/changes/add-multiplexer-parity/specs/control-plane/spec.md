# Control Plane

## ADDED Requirements

### Requirement: Sessions record server-side as asciicast

The daemon SHALL record a session's PTY output as an asciicast v2 file on
request — capability `sessions.record` (`Verb::RecordSession`, scope via
`required_scope`, surfaces HTTP/gRPC/CLI; deliberately not MCP or plugin in
v1) with start, stop and status operations, and CLI
`thegn session record <id> [--stop]`. Recording is owned by the session
actor, so it MUST continue while no client is attached and MUST stop
(finalizing the file) when the session exits. When no recording is active
the tee MUST cost a single null check per output event — no allocation.
Resizes are recorded as asciicast resize events. Files live under the
per-profile recordings directory (directory 0700, files 0600), bounded by a
configured `[recording] max_bytes` cap that finalizes the file rather than
filling the disk, and the control API returns recording status and path —
never file content. A session being recorded MUST show a recording indicator
in any attached UI.

#### Scenario: Recording survives detach

- **WHEN** a recording is started on a daemon session and every client
  detaches while the process keeps writing
- **THEN** the daemon keeps appending output events to the cast file, and the
  file finalizes when recording is stopped or the session exits

#### Scenario: Off means free

- **WHEN** no recording is active on a session
- **THEN** output handling performs only a null check — no timestamping, no
  allocation, no I/O

#### Scenario: Under-scoped record is refused

- **WHEN** a client whose token lacks the required scope calls
  `sessions.record`
- **THEN** the request is rejected and no file is created

#### Scenario: The size cap finalizes, not truncates

- **WHEN** a recording reaches `[recording] max_bytes`
- **THEN** the writer finalizes a valid cast file, recording status reports
  the cap was hit, and the session itself is unaffected

#### Scenario: Recording is visible at the keyboard

- **WHEN** a session is being recorded and a client is attached
- **THEN** the attached UI shows a recording indicator for that session
