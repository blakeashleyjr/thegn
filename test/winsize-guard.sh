#!/usr/bin/env bash
# Two guards on the window-resize path, both regressions we actually shipped
# (2026-09-17: thegn strobed at ~20 full repaints/s until it was killed).
#
#   1. thegn must NEVER write its own tty's winsize. `Terminal::set_screen_size`
#      is a TIOCSWINSZ on the controlling terminal, and the kernel answers a
#      winsize change by SIGWINCHing the foreground process group — us. Racing a
#      compositor that is still re-tiling, we write back the size we just read
#      while the emulator asserts a newer one, and the two take turns. The
#      winsize belongs to the terminal emulator; `BufferedTerminal::resize` is
#      what updates our own surface. The trait impl in `frame_write.rs` is the
#      no-op stub the `Terminal` trait requires — a declaration, not a call.
#
#   2. Exactly ONE window-size adoption point in the event loop, plus the
#      startup seed. There used to be two hand-copied blocks (the SIGWINCH arm
#      and the winsize poll) and they had drifted: the poll skipped
#      `set_window_cols` + `sidebar_cols`, so a size adopted through it left the
#      sidebar's width clamp on the OLD width and the reconciliation block fired
#      a second chrome recompute + relayout a frame later — a ±1-column
#      ping-pong. Both paths now funnel through `adopt_window_size!`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# A call, as opposed to the trait-method declaration or a comment.
setsize_calls() {
  grep -F 'set_screen_size(' |
    grep -vE ':[0-9]+:[[:space:]]*//' |
    grep -vE 'fn set_screen_size\('
}

if [[ ${1:-} == --self-test ]]; then
  allowed=(
    'frame_write.rs:1:        fn set_screen_size(&mut self, _: ScreenSize) -> Result<()> {'
    'run.rs:1: // buf.terminal().set_screen_size(size)'
  )
  rejected=(
    'run.rs:1: buf.terminal().set_screen_size(size)'
    'run.rs:1: let _ = term.set_screen_size(ScreenSize { rows, cols })'
  )
  for line in "${allowed[@]}"; do
    if printf '%s\n' "$line" | setsize_calls >/dev/null; then
      echo "ERROR: winsize guard rejected permitted line: $line" >&2
      exit 1
    fi
  done
  for line in "${rejected[@]}"; do
    if ! printf '%s\n' "$line" | setsize_calls >/dev/null; then
      echo "ERROR: winsize guard accepted forbidden line: $line" >&2
      exit 1
    fi
  done
  echo "winsize guard: 4 regression cases passed"
  exit 0
fi

sites=$(grep -rIn 'set_screen_size(' crates --include='*.rs' || true)
if printf '%s\n' "$sites" | setsize_calls; then
  echo 'ERROR: set_screen_size writes our own tty winsize (TIOCSWINSZ) and SIGWINCHes us back — the emulator owns the winsize (CLAUDE.md)' >&2
  exit 1
fi

# The startup seed + the single `adopt_window_size!` body. A third means the
# adoption path has been forked again.
adopt=$(grep -cE '^[[:space:]]*layout::set_window_cols\(cols\);' crates/thegn-host/src/run.rs)
if [[ $adopt != 2 ]]; then
  echo "ERROR: expected exactly 2 layout::set_window_cols(cols) sites in run.rs (startup seed + adopt_window_size!), found $adopt" >&2
  echo '       every window-size adoption must funnel through adopt_window_size! or the two resize paths drift again' >&2
  exit 1
fi
