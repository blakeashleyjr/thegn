# provider-native-exec

## ADDED Requirements

### Requirement: Exec sockets stay alive while idle

A Sprites exec socket SHALL send a WebSocket ping at least every 15 seconds
while the command runs, whether or not there is input, so an idle intermediary
does not close a quiet stream. A socket whose peer has answered a ping before
and then sends nothing for 45 seconds MUST be treated as dropped. A peer that
never answers pings MUST NOT be judged by silence.

#### Scenario: A quiet build keeps its stream

- **WHEN** a non-tty command prints nothing for minutes and the server answers
  pings
- **THEN** the client sends only keepalive pings and the session stays open

#### Scenario: A half-open socket is dropped

- **WHEN** the server answered a ping and then sends nothing for 45 seconds
- **THEN** the session ends without an exit status so the caller can reattach

### Requirement: Non-tty sprite-exec reattaches after a drop

When a non-tty `sprite-exec` stream ends without an exit status and the server
announced a session id, thegn SHALL reattach to that session with backoff
(1, 2, 4, 8 and 15 s, the budget renewed after a stream that lived 60 s),
asking the server to resume after the stdout and stderr payload bytes already
received. A reattach MUST NOT start the command again, and stdout MUST carry
only the remote's bytes; progress notices go to stderr.

#### Scenario: A dropped held run resumes

- **WHEN** a held `sprite-exec` stream drops mid-build and the session survives
- **THEN** thegn reattaches with `output_offset` equal to the bytes received,
  and the run ends with the remote's exit status

#### Scenario: Reattach is exhausted

- **WHEN** every reattach attempt fails
- **THEN** `sprite-exec` fails with `exec stream ended without an exit status`
  and the last reattach error
