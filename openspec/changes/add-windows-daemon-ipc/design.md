# Design — native Windows phase 2 (daemon IPC)

## Decisions

- **Seam lives in thegn-svc** (`src/ipc.rs`): both the svc-side control client
  and the host-side daemon need it; svc already owns tokio + axum + sha2.
- **Pipe name = hash of the socket path**, not a sanitized path: always valid,
  collision-resistant, length-bounded, and inherits per-state-dir isolation
  for free. Classification is by the `\\.\pipe\` prefix so `DaemonRow.endpoint`
  needs no schema/marker.
- **Windows lock**: `first_pipe_instance(true)` + `reject_remote_clients(true)`;
  `ACCESS_DENIED` ⇒ `AlreadyRunning`. No stale-endpoint recovery on Windows —
  pipes die with the process (simpler than unix, which keeps the historical
  connect-probe + unlink).
- **Accept loop**: named pipes have no `accept()`; `IpcListener` holds a
  pre-created next instance, `connect().await`s it, hands it out, and
  re-creates the successor _before_ returning — a client arriving between
  hand-off and the next accept never sees file-not-found. Hidden behind
  `accept_stream()` + an `axum::serve::Listener` impl, so the daemon loop is
  untouched.
- **`ERROR_PIPE_BUSY` backoff** (bounded, ~127ms worst case) on connect: all
  instances busy means the server is alive and mid-hand-off; unknown-name
  errors (daemon gone) surface immediately so `ensure_daemon`'s health-retry
  loop keeps its timing.
- **Relay stays unix-only.** Its socket is bind-mounted into sealed _Linux_
  containers; on a native Windows host that substrate doesn't exist, so the
  Phase-1 `Unsupported` stub is the correct permanent shape (revisit only if
  WSL-hosted sandboxes ever become a backend). Remote attach from/to Windows
  rides the already-cross-platform TCP serve path.

## Testing

- Pure derivation/classification + unix bind-lock/round-trip tests run on
  Linux CI (the 95%-core gate doesn't apply — svc — but the module carries its
  own tests).
- `cfg(windows)` twins of the bind-lock/round-trip tests run in the opt-in
  windows CI job against real pipes (`cargo test -p thegn-svc --lib ipc`).
