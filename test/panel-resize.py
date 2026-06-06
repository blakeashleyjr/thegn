#!/usr/bin/env python3
"""End-to-end test for dynamic small-screen handling of the chrome surfaces.

Verifies the fix for the auto-hide thrash: the sidebar/panel decide their
narrow-terminal auto-collapse from the TOTAL terminal width (not their own
pane width), so they:

  1. SPAWN visible on a normal terminal (the bug auto-hid them on load).
  2. Fold the panel first as the terminal narrows, then the sidebar.
  3. Restore to the full template when the terminal widens again.
  4. Still honor the manual Ctrl+Alt+p / Ctrl+Alt+s toggles.

Drives a real zellij client on a pty (layouts/plugins need a connected
client) and resizes it via TIOCSWINSZ (which signals SIGWINCH to zellij).
Thresholds under test: panel folds < 100 cols, sidebar folds < 64 cols.
"""
import os
import pty
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import fcntl
import time
import shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SZ = os.path.join(ROOT, "target", "release", "superzej")
CONFIG = os.path.join(ROOT, "config", "zellij.kdl")
SESSION = f"sz-resize-{os.getpid()}"

FAILED = []


def ok(msg):
    print(f"  ✓ {msg}")


def bad(msg):
    print(f"  ✗ {msg}")
    FAILED.append(msg)


def check(cond, msg):
    ok(msg) if cond else bad(msg)


def act(*args, timeout=10):
    # ZELLIJ_SOCKET_DIR pins this to the sandbox session namespace — it can
    # never reach a real (system or live-superzej) session.
    env = dict(os.environ, ZELLIJ_SESSION_NAME=SESSION,
               ZELLIJ_SOCKET_DIR=SANDBOX_RUN)
    r = subprocess.run(["zellij", "action", *args], env=env,
                       capture_output=True, text=True, timeout=timeout)
    return r.stdout


def dump():
    return act("dump-layout")


def focused_tab_block(d=None):
    d = d if d is not None else dump()
    blocks, cur = [], None
    for line in d.splitlines():
        stripped = line.lstrip()
        indent = len(line) - len(stripped)
        if indent == 4 and stripped.startswith("tab "):
            cur = [line]
            blocks.append(cur)
        elif indent == 4 and stripped and cur is not None:
            cur = None
        elif cur is not None:
            cur.append(line)
    for b in blocks:
        if "focus=true" in b[0]:
            return "\n".join(b)
    return ""


def chrome_plugins(block=None):
    block = block if block is not None else focused_tab_block()
    return [p for p in ("sidebar", "tabbar", "panel", "statusbar")
            if f"superzej/{p}.wasm" in block]


# ── setup ────────────────────────────────────────────────────────────────
print("== setup ==")
if not (os.path.exists(SZ) and shutil.which("zellij")):
    print("SKIP: need target/release/superzej and zellij")
    sys.exit(0)

# Visibility state (.panel_state/.sidebar_state) lives under
# ${SUPERZEJ_DIR:-$HOME/.superzej}. We point SUPERZEJ_DIR at the sandbox below
# (it doesn't affect the `~`-rooted plugin urls), so this never reads or writes
# your real ~/.superzej.
tmphome = tempfile.mkdtemp()
state = os.path.join(tmphome, "state")
# Fully isolated zellij: a private socket dir (session namespace) and cache,
# both under the throwaway tmp tree. This harness can NEVER see or disturb a
# real (system or live-superzej) session — the root-cause fix for the time a
# cache-wipe took down a live session.
SANDBOX_RUN = os.path.join(tmphome, "run")
SANDBOX_CACHE = os.path.join(tmphome, "cache")
os.makedirs(os.path.join(SANDBOX_CACHE, "zellij"))
os.makedirs(SANDBOX_RUN)
# Pre-grant the plugins' permissions in the sandbox cache (the first-load prompt
# renders in fixed panes and is un-approvable). Copy from wherever superzej or a
# prior zellij last wrote them.
for _src in (os.path.expanduser("~/.superzej/cache/zellij/permissions.kdl"),
             os.path.expanduser("~/.cache/zellij/permissions.kdl")):
    if os.path.exists(_src):
        shutil.copy(_src, os.path.join(SANDBOX_CACHE, "zellij", "permissions.kdl"))
        break

repo = os.path.join(tmphome, f"resize-{os.getpid()}")
os.makedirs(repo)
subprocess.run(["git", "-C", repo, "init", "-q"], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.email=t@e", "-c",
                "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"],
               check=True)

pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.environ["XDG_STATE_HOME"] = state
    os.environ["ZELLIJ_SOCKET_DIR"] = SANDBOX_RUN
    os.environ["XDG_CACHE_HOME"] = SANDBOX_CACHE
    os.environ["SUPERZEJ_DIR"] = tmphome  # state files (.panel_state, …) stay in the sandbox
    # Drop inherited zellij env so this never nests into / leaks from a live
    # session when the harness is run from inside one.
    for _v in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID"):
        os.environ.pop(_v, None)
    os.environ["PATH"] = os.path.join(ROOT, "target", "release") + os.pathsep + os.environ["PATH"]
    os.execvp("zellij", ["zellij", "--config", CONFIG, "--session", SESSION])
os.set_blocking(fd, False)


def resize(rows, cols, settle=2.5):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    # The kernel signals SIGWINCH to the pty's foreground group (the zellij
    # client) on the size change; give the relayout + PaneUpdate time to land.
    time.sleep(settle)
    drain()


def drain():
    try:
        while True:
            if not os.read(fd, 65536):
                break
    except (OSError, BlockingIOError):
        pass


def key(seq, wait=1.5):
    os.write(fd, seq)
    time.sleep(wait)
    drain()


def cleanup():
    subprocess.run(["zellij", "delete-session", SESSION, "--force"],
                   capture_output=True,
                   env=dict(os.environ, ZELLIJ_SOCKET_DIR=SANDBOX_RUN))
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    shutil.rmtree(tmphome, ignore_errors=True)  # takes the sandboxed state with it


# Start at a normal-but-not-huge width: 120 cols. Old (buggy) code hid both
# surfaces on load here (panel own-width 27%~32 < 35; sidebar 12%~14 < 15).
resize(45, 120, settle=0.0)
try:
    time.sleep(5)
    drain()

    print("== spawn on a 120-col terminal ==")
    cp = chrome_plugins()
    check("sidebar" in cp, f"sidebar spawned visible at 120 cols ({cp})")
    check("panel" in cp, f"panel spawned visible at 120 cols ({cp})")
    check(len(cp) == 4, f"full chrome present at 120 cols ({cp})")

    print("== narrow to 90 cols: panel folds, sidebar stays ==")
    resize(45, 90)
    cp = chrome_plugins()
    check("panel" not in cp, f"panel folded at 90 cols ({cp})")
    check("sidebar" in cp, f"sidebar still visible at 90 cols ({cp})")

    print("== narrow to 70 cols: both fold ==")
    resize(45, 70)
    cp = chrome_plugins()
    check("panel" not in cp, f"panel folded at 70 cols ({cp})")
    check("sidebar" not in cp, f"sidebar folded at 70 cols ({cp})")

    print("== widen to 140 cols: full chrome restored ==")
    resize(45, 140)
    cp = chrome_plugins()
    check(len(cp) == 4, f"full chrome restored at 140 cols ({cp})")
    check("panel" in cp and "sidebar" in cp,
          f"both surfaces back in their slots ({cp})")

    print("== no thrash: layout stable across repeated PaneUpdates ==")
    b1 = focused_tab_block()
    time.sleep(3)
    b2 = focused_tab_block()
    check(chrome_plugins(b1) == chrome_plugins(b2) == ["sidebar", "tabbar", "panel", "statusbar"],
          "chrome stays put when idle (no hide/show thrash)")

    print("== manual toggle still works at full width ==")
    key(b"\x1b\x10", wait=2)   # Ctrl+Alt+p: hide panel
    cp = chrome_plugins()
    check("panel" not in cp, f"Ctrl+Alt+p hid the panel ({cp})")
    key(b"\x1b\x10", wait=2)   # show again
    cp = chrome_plugins()
    check("panel" in cp and len(cp) == 4,
          f"Ctrl+Alt+p restored the panel to its slot ({cp})")

    key(b"\x1b\x13", wait=2)   # Ctrl+Alt+s: hide sidebar
    cp = chrome_plugins()
    check("sidebar" not in cp, f"Ctrl+Alt+s hid the sidebar ({cp})")
    key(b"\x1b\x13", wait=2)   # show again
    cp = chrome_plugins()
    check("sidebar" in cp and len(cp) == 4,
          f"Ctrl+Alt+s restored the sidebar to its slot ({cp})")
finally:
    cleanup()

print()
if FAILED:
    print(f"FAIL ({len(FAILED)}):")
    for f in FAILED:
        print(f"  - {f}")
    sys.exit(1)
print("PASS")
