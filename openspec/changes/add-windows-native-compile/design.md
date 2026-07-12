# Design — native Windows phase 1 (compile)

## Decisions

### D1 — Two platform seams, matching the crate layering

- `crates/thegn-core/src/shellinv.rs`: **pure, no I/O** shell-dialect argv
  building (`ShellFlavor {Posix, Pwsh, Cmd}`, `flavor_of`, `run_argv`,
  `exec_argv`). Core is substrate-agnostic and coverage-gated, so the Windows
  arms are unit-tested on Linux CI. `util::shell()` resolves *which* shell;
  `shellinv` resolves *how to invoke it*.
- `crates/thegn-host/src/platform/{mod,unix,windows}.rs`: all OS-conditional
  syscalls. `mod.rs` holds the shared logic (e.g. opening the stderr log file)
  and re-exports the per-OS impl. Rule: nothing outside `platform/` writes a
  `#[cfg(windows)]` block except trivial one-line chmod/symlink gates.

### D2 — Thin cfg wrappers, thick portable logic

Anything decidable without a syscall compiles on all platforms and is tested on
Linux: shell-flavor dispatch, ErrorKind-based transient-IO classification (the
raw-errno tail is the only per-OS piece), pipe-name derivation (next change).

### D3 — Local vs remote shell invocations

Only **local** spawns route through `shellinv` (pins, tool drawer, custom
actions, pane-run, editor-open). Sites that build argvs for a remote or
sandboxed **Linux** environment (provider `run_exec`, sealed containers,
`GitLoc::sh_command` remotes) keep literal `/bin/sh -lc` — the target substrate
is known and is not the host — and carry the annotation
`// remote/sandbox target is Linux; POSIX sh is correct here`.

### D4 — Honest stubs, never silent breakage

Where the substrate is unix-only today, the Windows arm returns an explicit
error (`bail!`/`ErrorKind::Unsupported`) rather than a no-op: daemon IPC,
ACP unix transport, model relay, merge-queue agent (warns), `thegn debug`
exec-replace. Graceful no-ops are reserved for genuinely optional semantics
(chmod exec bits, malloc tuning off glibc).

### D5 — Windows semantics chosen

- **Stderr redirect**: `SetStdHandle(STD_ERROR_HANDLE, log)` + keep the `File`
  alive in the guard; Rust std resolves the std handle per write, so panics
  from any thread land in the log. CRT fd-2 writers are not rebound (thegn has
  no C code that writes stderr). Restore on drop.
- **Termination is hard**: `TerminateProcess` — no SIGTERM window. Call sites
  that assume graceful-cleanup-in-the-child are audited in the Job Objects
  change.
- **Process groups**: Phase 1 tracks the direct child only (`kill_tree` =
  `TerminateProcess(pid)`); Job Objects (whole-tree, kill-on-close) are the
  Phase-3 upgrade behind the same `set_process_group`/`kill_tree` seam.
- **Shutdown**: tokio `ctrl_c` + `ctrl_close` + `ctrl_shutdown` map to the
  SIGTERM/SIGHUP contract (set flag, pulse waker / notify).
- **`util::detached()`**: unix `process_group(0)` ↔ windows
  `CREATE_NO_WINDOW` — both are the platform's "never touch the caller's
  terminal" hygiene.

### D6 — CI gating

- **Fast Linux gate (every PR)**: `cargo check --workspace --target
  x86_64-pc-windows-gnu` in `just check-cross`, with the mingw-w64 cross-cc
  (`pkgsCross.mingwW64.stdenv.cc`) provided by the dev shell for the C build
  scripts in the graph (bundled sqlite, libgit2, stacker…). windows-gnu and
  windows-msvc share `cfg(windows)`, so this catches all gating regressions.
- **msvc truth gate (opt-in)**: `windows-latest` job (`[ci-windows]` marker or
  dispatch), bare rustup + `cargo check --workspace --locked` — no nix on
  Windows; this job doubles as the documented bare-cargo path.

## Alternatives considered

- *Gate `libc` loosely and use CRT `_dup2` for stderr*: rejected — ownership
  hand-off through `open_osfhandle` is easy to double-free, and Rust-side
  writers (the actual risk: panics) go through the std handle anyway.
- *One `#[cfg]` at every call site, no platform module*: rejected — that is how
  the codebase got 30 scattered unix-isms; the seam keeps the diff reviewable
  and gives Phase 3 (Job Objects) a single landing point.
