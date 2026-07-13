# Tasks — native Windows phase 2 (daemon IPC over named pipes)

## 1. IPC seam (`thegn_svc::ipc`)

- [x] 1.1 `pipe_name_for_path` (sha256-derived, `\\.\pipe\thegn-…`) +
      `IpcEndpoint::for_socket_path` prefix/platform classification — pure,
      unit-tested on Linux (determinism, isolation, round-trip).
- [x] 1.2 `IpcStream` enum (UnixStream / NamedPipeClient / NamedPipeServer)
      implementing `AsyncRead`+`AsyncWrite`; `connect()` with bounded
      `ERROR_PIPE_BUSY` backoff.
- [x] 1.3 `IpcListener::bind_exclusive` → `Bound | AlreadyRunning` with the
      daemon's exact lock semantics on both platforms; `accept_stream` with
      the pre-created next-instance pattern; `axum::serve::Listener` impl.
- [x] 1.4 Unix tests: bind-is-the-lock, stale-file recovery, byte round-trip.
      Windows tests (`cfg(windows)`): pipe bind-lock + round-trip + name
      freed on drop.

## 2. Rewiring

- [x] 2.1 `control/client.rs`: `ControlAddr::Unix` request + attach paths ride
      `ipc::connect`; `WsEither::Unix` → `WsEither::Ipc`; Phase-1 stubs gone.
- [x] 2.2 `daemon/mod.rs`: bind via `bind_exclusive`; windows `run()` stub
      removed; `DaemonRow.endpoint` stores `ep.display()`; `pid_alive`
      ungated (platform seam handles both).
- [x] 2.3 Decision recorded: sealed-sandbox relay stays unix-only (consumers
      are Linux containers); remote attach rides the TCP serve path.

## 3. Validation

- [x] 3.1 `cargo test -p thegn-svc --lib ipc` green on Linux.
- [x] 3.2 Native + windows-gnu cross-checks green for svc + host.
- [x] 3.3 Windows CI job runs the ipc tests on a real pipe (`[ci-windows]`).
- [ ] 3.4 Manual on a Windows box: `thegn daemon` two-terminal race (second
      exits 0), daemon-backed pane attach (phase-4 checklist).
