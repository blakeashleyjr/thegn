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

Split by what can be *measured* and what needs a human looking at a terminal.
Everything below marked done was run on the box and its evidence recorded here;
the rest are visual/interactive and cannot be driven headlessly.

- [x] 2.1 `cargo run -p thegn-host --example waker_spike` — one tick/second,
      ~0% CPU between ticks, instant key echo. **Gate for everything below.**
      Measured: 10 ticks in 11s, every one attributed `(waker)` rather than a
      poll timeout, and **0.0 ms of CPU across a 10 s window** — the event model
      holds. (Key echo is the interactive half; see 2.2.)
- [ ] 2.2 `cargo run` (bare `thegn`): first frame renders (chrome + pane),
      pwsh prompt appears, typing echoes. **Needs a human.** It does start and
      emit a frame headlessly (34–36 KB of escape output per run), but "renders
      correctly" and "typing echoes" are judgements a pipe cannot make.
- [x] 2.3 Idle CPU ~0% in Task Manager with the compositor idle.
      Measured with `examples/idle_cpu_windows` (release, 14-worktree fixture,
      45 s settle, 8 s window): **0.0367 cores**, against Linux's ~0.056 on the
      same fixture — Windows idles *below* Linux, not above it. `idle_ratio`
      0.955, `renders_per_s` 0.0 with every wake going to a render *skip*, and
      `render_busy_ratio` 0.004; the busiest thread accounts for 0.012 cores and
      the hot source is the 2 s refresh ticker, which is the designed wake.
      This supersedes the withdrawn "~1.6× Linux" figure for good.

      Measure it **only** through that harness, which launches the binary in a
      real ConPTY. Do NOT sample a run whose stdout/stderr are redirected to
      files: stdin is then not a console, the loop does not block in
      `poll_input(None)`, and the process burns ~0.25 cores. That number
      measures the redirect, not thegn — it was produced and discarded once
      already while closing this item.
- [ ] 2.4 Resize the WT window hard (drag-storm): no tearing, no panic,
      layout recomputes. **Needs a human** — and it is one of the two items
      most likely to find something, since ConPTY reflows on resize rather than
      signalling.
- [ ] 2.5 Ctrl+C inside a pane interrupts the pane child (not thegn); pane
      exit (`exit`) closes/replaces the pane (EOF reaches pty_drain).
      **ANSWERED, and it FAILS** — this no longer needs a human, it needs a fix.
      It was flagged here as the likeliest item to find something, and it did.

      Measured by `pane::tests::ctrl_c_interrupts_the_pane_child` (`#[ignore]`d
      as the reproduction; run with `--run-ignored all`) and narrowed by
      `examples/ctrl_c_windows`. The child survives Ctrl-C. The control matters:
      plain typing **does** reach it — a `Read-Host` echoes the text straight
      back — so the write path, ConPTY's input handling and the child's stdin
      are all working. What does not happen is the interrupt. Neither the raw
      `0x03` thegn's key encoder produces, nor the win32-input-mode key record
      that ConPTY's own `ESC[?9001h` handshake asks for, nor both together
      interrupts either PowerShell or `cmd`. So no `CTRL_C_EVENT` reaches the
      child at all, the encoding is not the problem, and changing it is not the
      fix. `portable-pty` was checked and does not pass `CREATE_NEW_PROCESS_GROUP`
      (which would disable Ctrl-C by itself), so that is ruled out too.

      Still unmeasured, and tracked separately from the above: the second half —
      whether `exit` in a pane closes/replaces it (EOF reaching `pty_drain`).
- [x] 2.6 StderrGuard: `THEGN_LOG=info`, force a background warn (e.g. break
      a config path) — frame stays clean, line lands in thegn-stderr.log.
      Measured with a deliberately malformed `config.toml`: the warn
      (`thegn_core::msg config: parse error: TOML parse error at line 1,
      column 6`) reached the log sink, **zero** log text appeared anywhere in
      the 34 KB frame stream, and the terminal's raw stderr received **0 bytes**
      — the guard took all of it.
- [x] 2.7 conhost.exe launch refused with the Windows Terminal pointer.
      Verified verbatim: `thegn requires a console that supports VT sequences —
      run it inside Windows Terminal (https://aka.ms/terminal); legacy
      conhost.exe is not supported`. **The item's wording is now stale**: since
      `7924d4a2` the gate asks the console whether it accepts
      `ENABLE_VIRTUAL_TERMINAL_PROCESSING` rather than reading `WT_SESSION`, so
      a *modern* conhost is correctly accepted and only a genuinely non-VT
      console is refused. Both directions were exercised — a redirected
      (non-console) stdout is refused with the message above, and a plain
      VT-capable console starts and renders.
- [x] 2.8 `thegn daemon` two-terminal race: second exits 0 "already running";
      daemon-backed pane opens over the pipe.
      Race half measured: the second `thegn daemon` on the same profile exits
      **0** and logs `daemon already running on \.\pipe\thegn-d3198077f879db64`
      while the first keeps serving. The pane-over-pipe half needs the TUI and
      is folded into 2.2.
- [ ] 2.9 Unicode/border glyphs render (sidebar tree, pin strip, logotype) —
      no ASCII fallback in WT. **Needs a human.** Note the caps are read, not
      assumed: `thegn doctor` prints what the console reported, so compare the
      frame against that rather than against an expectation.

## 3. CI

- [ ] 3.1 One `[ci-windows]` dispatch green with the phase-4 tree
      (workspace check + ipc + platform kernel tests).
