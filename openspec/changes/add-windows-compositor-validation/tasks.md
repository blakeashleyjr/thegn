# Tasks — native Windows phase 4 (compositor readiness on Windows Terminal)

## 1. Code readiness (done on Linux, cross-checked)

- [x] 1.1 `resolve_pane_shell`: Windows arm delegates to `util::shell()`
      (pwsh → powershell → `%COMSPEC%`); unix probe chain untouched.
- [x] 1.2 `shell_argv_from`: POSIX `-i`/`-l` flags only for POSIX flavors
      (via `shellinv::flavor_of`); pwsh/cmd get a bare argv. Unit tests.
- [x] 1.3 termcaps: `WT_SESSION` ⇒ Full Unicode (locale-var check bypassed),
      undercurl, sync_output. Unit tests.
- [x] 1.4 Conhost gate: `modern_terminal_evidence` (pure, tested) + the
      `cfg(windows)` startup bail in main.rs pointing at Windows Terminal.
- [x] 1.5 Path separators: `util::basename` splits `/` and `\` (tested);
      worktree-path basenames consolidated through it (run.rs ×4, pty_drain,
      share::label_for, account::infer_provider + `.exe` strip). Git-relative
      '/'-splits audited and deliberately kept.
- [x] 1.6 `examples/waker_spike.rs` — the poll_input(None)+waker proof with
      documented pass/fail; cross-checks for windows-gnu.
- [x] 1.7 CONTRIBUTING.md "Windows (native)" section.

## 2. On-machine validation checklist (Blake's Windows box, Windows Terminal)

- [ ] 2.1 `cargo run -p thegn-host --example waker_spike` — one tick/second,
      ~0% CPU between ticks, instant key echo. **Gate for everything below.**
- [ ] 2.2 `cargo run` (bare `thegn`): first frame renders (chrome + pane),
      pwsh prompt appears, typing echoes.
- [ ] 2.3 Idle CPU ~0% in Task Manager with the compositor idle.
- [ ] 2.4 Resize the WT window hard (drag-storm): no tearing, no panic,
      layout recomputes.
- [ ] 2.5 Ctrl+C inside a pane interrupts the pane child (not thegn); pane
      exit (`exit`) closes/replaces the pane (EOF reaches pty_drain).
- [ ] 2.6 StderrGuard: `THEGN_LOG=info`, force a background warn (e.g. break
      a config path) — frame stays clean, line lands in thegn-stderr.log.
- [ ] 2.7 conhost.exe launch refused with the Windows Terminal pointer.
- [ ] 2.8 `thegn daemon` two-terminal race: second exits 0 "already running";
      daemon-backed pane opens over the pipe.
- [ ] 2.9 Unicode/border glyphs render (sidebar tree, pin strip, logotype) —
      no ASCII fallback in WT.

## 3. CI

- [ ] 3.1 One `[ci-windows]` dispatch green with the phase-4 tree
      (workspace check + ipc + platform kernel tests).
