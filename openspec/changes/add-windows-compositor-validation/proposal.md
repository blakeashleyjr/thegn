# Add native Windows support, phase 4: compositor readiness on Windows Terminal

## Summary

Phases 1–3 made the workspace compile and the process substrate (IPC, Job
Objects) native. This change makes the **interactive compositor** correct on
Windows Terminal — every code-side gap found by auditing the launch → pane →
render path — and ships the tooling for the on-machine validation pass:

- **Pane shells**: `resolve_pane_shell` no longer bottoms out at `/bin/sh` on
  Windows — it delegates to the platform resolver (pwsh → powershell →
  `%COMSPEC%`); `shell_argv_from` stops handing POSIX `-i`/`-l` flags to
  pwsh/cmd (bare argv; login mode doesn't exist there).
- **Capability detection**: Windows Terminal sets no POSIX locale vars, so the
  locale check was demoting it to ASCII glyphs — `WT_SESSION` now yields Full
  Unicode, undercurl, and DECSET-2026 synchronized output (all WT ≥ 1.18).
- **Conhost gate**: per the scope decision, legacy conhost.exe is refused at
  startup with a clear pointer to Windows Terminal
  (`termcaps::modern_terminal_evidence`, pure + tested; the `cfg(windows)`
  gate lives in `main.rs`).
- **Path separators**: `util::basename` splits both `/` and `\`; every
  worktree-absolute-path basename derivation (tab titles, search labels,
  approval overlays, attention toasts, share labels, provider inference incl.
  `.exe` stripping) routes through it. Git-relative paths (always `/`, even
  on Windows) deliberately keep their `'/'` splits.
- **Waker spike** (`examples/waker_spike.rs`): a runnable proof of the 0%-idle
  event model — blocking `poll_input(None)` woken by an off-thread
  `TerminalWaker` — with documented pass/fail. This is the riskiest unknown on
  the termwiz Windows backend and MUST pass on a real machine before the
  compositor is trusted there.
- **CONTRIBUTING.md** gains the "Windows (native)" section: rustup + VS Build
  Tools, bare cargo, the spike, state paths, and what's intentionally absent.

The remaining validation items are interactive-by-nature (resize storms, ^C
passthrough, EOF-on-exit, StderrGuard fidelity under a panicking thread) and
are enumerated as the on-machine checklist in tasks.md — they need Blake's
Windows box or a dispatch of the windows CI job; no further code is assumed.

## Impact

- tasks.md AX 736 (+ 734 closed by the pane-shell work).
- Crates: `thegn-host` (panes, main conhost gate, run/pty_drain basenames,
  example), `thegn-core` (termcaps, util::basename, account, share).
- run.rs shrinks again (basename consolidation) — ratchet updated.
