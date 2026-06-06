#!/usr/bin/env python3
"""PTY smoke test for the cmd+k palette (`superzej menu`).

Launches the palette on a real pseudo-terminal in a fully sandboxed HOME/state,
types a Command-mode query, exercises navigation, then dismisses with Esc and
asserts a clean exit with the expected chrome rendered. Drives no zellij session,
so nav/tab sources simply come back empty — this isolates the TUI itself.

Usage: test/palette-smoke.py [path-to-superzej]   (default: target/debug/superzej)
"""
import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time

BIN = sys.argv[1] if len(sys.argv) > 1 else "target/debug/superzej"


def drain(fd, seconds):
    """Read whatever the child has emitted within `seconds`, answering the
    terminal-capability queries (Primary Device Attributes `ESC [ c` and the
    kitty-keyboard query `ESC [ ? u`) that crossterm blocks the first render on.
    A real terminal emulator replies instantly; a bare PTY must do it here."""
    buf = b""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
            if b"\x1b[?u" in chunk:
                os.write(fd, b"\x1b[?0u")  # no kitty-keyboard flags
            if b"\x1b[c" in chunk:
                os.write(fd, b"\x1b[?6c")  # VT102 device attributes
    return buf


def main():
    sandbox = tempfile.mkdtemp(prefix="sz-palette-smoke-")
    env = dict(os.environ)
    env.update(
        {
            "HOME": sandbox,
            "XDG_STATE_HOME": os.path.join(sandbox, "state"),
            "XDG_CONFIG_HOME": os.path.join(sandbox, "config"),
            "XDG_CACHE_HOME": os.path.join(sandbox, "cache"),
            "SUPERZEJ_DIR": os.path.join(sandbox, "sz"),
            "ZELLIJ_SOCKET_DIR": os.path.join(sandbox, "run"),
            "TERM": "xterm-256color",
        }
    )
    # Ensure we're not seen as inside a zellij session.
    for k in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID"):
        env.pop(k, None)

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    proc = subprocess.Popen(
        [BIN, "menu"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        close_fds=True,
    )
    os.close(slave)

    out = drain(master, 0.8)  # initial frame
    os.write(master, b">tog")  # Command mode, fuzzy "tog" -> Toggle …
    out += drain(master, 0.6)
    os.write(master, b"\x1b[B")  # Down arrow
    out += drain(master, 0.3)
    os.write(master, b"\x1b")  # Esc -> dismiss
    out += drain(master, 0.3)

    try:
        code = proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        print("FAIL: palette did not exit after Esc", file=sys.stderr)
        return 1
    finally:
        try:
            os.close(master)
        except OSError:
            pass

    text = out.decode("utf-8", "replace")
    if code != 0:
        print(f"FAIL: non-zero exit {code}", file=sys.stderr)
        return 1
    # The Command-mode pill and a matched command label should be on screen.
    if "CMD" not in text:
        print("FAIL: did not render the CMD mode pill", file=sys.stderr)
        print(repr(text[:2000]), file=sys.stderr)
        return 1
    if "Toggle" not in text:
        print("FAIL: fuzzy query '>tog' did not surface a Toggle command", file=sys.stderr)
        print(repr(text[:2000]), file=sys.stderr)
        return 1

    print("ok: palette rendered, matched '>tog', and exited cleanly on Esc")
    return 0


if __name__ == "__main__":
    sys.exit(main())
