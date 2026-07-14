# Add native Windows support, phase 1: the workspace compiles for `cfg(windows)`

## Summary

thegn's foundations are already cross-platform — termwiz init goes through
`new_terminal(caps)`, PTYs through `portable_pty::native_pty_system()` (ConPTY
on Windows), fs-watchers through `notify`, and `thegn_core::util` already
branches paths (`USERPROFILE`/`APPDATA`/`LOCALAPPDATA`) and shell detection
(pwsh → powershell → `%COMSPEC%`) for Windows. But the workspace does not
compile for a Windows target: `nix`/`libc` are unconditional deps of the host,
and ~30 call sites use unix signals, `exec()`, raw fds, process groups, and
unix-domain sockets inline.

This change makes `cargo check --workspace --target x86_64-pc-windows-gnu`
green (the fast Linux-side gate, added to `just check-cross`) and
`cargo check --workspace` green on GitHub-hosted `windows-latest` (the msvc
truth gate, an opt-in CI job) — with **zero unix behavior change**. It does so
by introducing two platform seams and routing every OS-conditional call site
through them:

- **`thegn_core::shellinv`** (pure, coverage-gated): shell-invocation argv
  building — POSIX `-c`/`-lc` vs pwsh `-NoProfile -Command` vs cmd `/C` — used
  by every _local_ pane/pin/custom-action spawn. Call sites that target a
  remote or sandboxed **Linux** substrate keep their literal `sh -lc` and are
  annotated as such.
- **`thegn-host::platform`** (`unix.rs` / `windows.rs`): stderr redirect
  (dup2 / `SetStdHandle`), pid liveness (kill-0 / `OpenProcess`), best-effort
  termination (SIGTERM / `TerminateProcess`), process groups (pgid / Phase-3
  Job Objects), shutdown signals (SIGTERM+SIGHUP / console ctrl events), and
  transient-tty-errno classification. Nothing outside `platform/` writes
  `#[cfg(windows)]` beyond trivial one-line chmod gates.

Features whose substrate is inherently unix **stub with a clear error** on
Windows in this phase: the pane daemon + control client over unix sockets
(named-pipe port is the next change), the sealed-sandbox model relay (its
consumers are Linux containers), the merge-queue headless agent (POSIX
`sh_quote` templating), and the SIGUSR2 flamegraph profiler.

## Impact

- tasks.md: starts the native-Windows track (new group; see roadmap link in
  the change folder). Compile-only — no runtime claims until the compositor
  validation change.
- Crates: `thegn-core` (shellinv, startup symlink helper, `detached()`,
  windows `shell()` fix), `thegn-svc` (control client/ACP transport gating),
  `thegn-host` (dep gating, `platform/` seam, call-site rewiring), root
  `Cargo.toml` (windows-sys), CI (`windows` opt-in job), justfile
  (workspace windows-gnu cross-check).
- Pinned god-files only shrink (run.rs −55 lines, sandbox.rs −9).
