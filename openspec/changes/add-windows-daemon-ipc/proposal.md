# Add native Windows support, phase 2: daemon IPC over named pipes

## Summary

Phase 1 (`add-windows-native-compile`) stubbed the pane daemon and control
client on Windows because their IPC + single-instance lock were a unix-domain
socket. This change ports both to a platform seam — `thegn_svc::ipc` — so
`thegn daemon`, `thegn serve`, daemon-backed panes, and every control-client
verb work identically over a unix socket (unix) or a named pipe (Windows),
and the Phase-1 stubs are deleted.

Mechanics:

- **`IpcEndpoint`**: classifies a stored/configured "socket path" — a
  `\\.\pipe\…` string is used as-is; any other path is a unix socket on unix
  and is _derived into_ a pipe name on Windows
  (`\\.\pipe\thegn-<hex(sha256(path))[..16]>`). Derivation + classification
  are pure and unit-tested on Linux, so per-state-dir daemon isolation (the
  "tests run inside a live thegn" gotcha) carries over unchanged.
- **`IpcListener::bind_exclusive`** — the endpoint IS the lock, preserving the
  daemon's exact bind-race semantics: unix keeps connect-probe + stale-file
  unlink + `AddrInUse` ⇒ `AlreadyRunning`; Windows uses
  `first_pipe_instance(true)` + `reject_remote_clients(true)` where
  `ACCESS_DENIED` ⇒ `AlreadyRunning`, and pipes die with the process so the
  stale-file case vanishes. Implements `axum::serve::Listener` (pre-created
  next-instance accept loop hidden inside), so the daemon's serve loop is
  unchanged.
- **`IpcStream`** (`AsyncRead`+`AsyncWrite`): what `send_request` (hyper) and
  the warm-attach WebSocket ride on either platform. `ERROR_PIPE_BUSY` gets a
  short bounded backoff (a live server is about to create the next instance).
- **`DaemonRow.endpoint`** stores the endpoint's string form (path / pipe
  name) — no schema change; discovery round-trips by prefix classification.

The sealed-sandbox model relay (`relay.rs`) intentionally stays unix-only: its
consumers are Linux containers that bind-mount the socket, which do not exist
on a native Windows host — there is nothing to relay for. Remote attach
from/to Windows rides the TCP serve path (HTTP/WS + gRPC), which was already
cross-platform.

## Impact

- tasks.md AX 735.
- Crates: `thegn-svc` (new `ipc` module; control client rewired, stubs
  removed), `thegn-host` (`daemon/mod.rs` binds through the seam; windows
  `run()` stub removed; `pid_alive` ungated).
- CI: the opt-in `windows` job now also runs the ipc module tests (real-pipe
  round-trip + bind-lock, `cfg(windows)`).
