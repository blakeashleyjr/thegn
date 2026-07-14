# Platform: native Windows

## MODIFIED Requirements

### Requirement: Unix-substrate features stub with explicit errors on Windows

Features whose substrate is inherently unix — the sealed-sandbox model relay
(its consumers are Linux containers that bind-mount the socket), the
merge-queue headless agent (POSIX `sh_quote` templating), the SIGUSR2
profiler, `thegn debug` exec-replace, and the ACP unix-socket transport —
SHALL return an explicit error (or logged warning, for best-effort paths) on
Windows rather than silently no-op or panic. The pane daemon and control
client are NOT in this set: they run natively over named pipes.

#### Scenario: Sealed-sandbox relay on Windows

- **WHEN** a sealed-agent launch asks for the model relay on native Windows
- **THEN** relay spawn returns an `Unsupported` error naming Linux containers
  as the missing substrate, and the caller surfaces it

## ADDED Requirements

### Requirement: Daemon IPC rides one endpoint seam on both platforms

Local daemon IPC (the pane daemon's listener, the control client's requests,
and the warm-attach WebSocket) SHALL go through `thegn_svc::ipc`: unix-domain
sockets on unix, named pipes on Windows. The pipe name MUST be derived
deterministically from the per-state-dir socket path
(`\\.\pipe\thegn-<hex(sha256(path))[..16]>`) so per-`$XDG_STATE_HOME` daemon
isolation is preserved, and a stored `\\.\pipe\…` endpoint string MUST be
recognized as-is by classification (discovery round-trips with no schema
change).

#### Scenario: Daemon serves over a named pipe

- **WHEN** `thegn daemon` starts on native Windows
- **THEN** it binds `\\.\pipe\thegn-…` derived from its state dir, registers
  that name as its `DaemonRow.endpoint`, and control-client verbs and
  daemon-backed pane attaches connect through it

#### Scenario: Pipe names isolate state dirs

- **WHEN** two daemons start with different `XDG_STATE_HOME`s (e.g. a dev
  instance under `just start` beside the daily driver)
- **THEN** their pipe names differ and neither sees the other as
  "already running"

### Requirement: The IPC endpoint is the single-daemon lock on both platforms

`bind_exclusive` SHALL preserve the daemon's bind-race semantics everywhere:
whoever binds the endpoint is the daemon; a second binder learns
`AlreadyRunning` and exits 0. On unix this keeps the connect-probe +
stale-file unlink + `AddrInUse` mapping; on Windows the first pipe instance is
the lock (`ACCESS_DENIED` for the loser), created with
`reject_remote_clients`, and a dead daemon's pipe vanishes with its process
(no stale-endpoint recovery needed).

#### Scenario: Spawn race on Windows

- **WHEN** two `thegn daemon` processes race to start for the same state dir
- **THEN** exactly one owns the pipe and serves; the other observes
  `AlreadyRunning` and exits 0, and clients connect to the winner
