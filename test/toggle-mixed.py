#!/usr/bin/env python3
"""Regression: showing one chrome surface while the OTHER is hidden must land
the shown surface in its template slot, NOT as a ~50% add_tiled_pane split.

The visibility controller (statusbar) restores a shown pane with
`next_swap_layout()`, which only matches the 5-pane base template. With a
sibling surface suppressed (4 panes) nothing matches, so a naive show left the
surface jammed half-way into the center — the "panel split half-way" bug. The
fix un-suppresses BOTH surfaces, relayouts at the full 5 panes, then re-hides
whichever should stay hidden.

This drives the *mixed* state that `panel-resize.py` never exercises (it only
toggles from both-visible) and asserts geometry, not just plugin presence.

Isolated sandbox zellij (private socket dir + cache); never touches a real
session. Mirrors test/panel-resize.py's setup.
"""
import os, pty, signal, struct, subprocess, sys, tempfile, termios, fcntl, time, shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SZ = os.path.join(ROOT, "target", "release", "superzej")
CONFIG = os.path.join(ROOT, "config", "zellij.kdl")
SESSION = f"sz-toggle-{os.getpid()}"

FAILED = []


def ok(msg):
    print(f"  ✓ {msg}")


def bad(msg):
    print(f"  ✗ {msg}")
    FAILED.append(msg)


def check(cond, msg):
    ok(msg) if cond else bad(msg)


def act(*args, timeout=10):
    env = dict(os.environ, ZELLIJ_SESSION_NAME=SESSION, ZELLIJ_SOCKET_DIR=SANDBOX_RUN)
    return subprocess.run(["zellij", "action", *args], env=env,
                          capture_output=True, text=True, timeout=timeout).stdout


def focused_tab_block(d=None):
    d = d if d is not None else act("dump-layout")
    blocks, cur = [], None
    for line in d.splitlines():
        stripped = line.lstrip()
        indent = len(line) - len(stripped)
        if indent == 4 and stripped.startswith("tab "):
            cur = [line]; blocks.append(cur)
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


def surface_size(name, block=None):
    """The `size="N%"` on the pane wrapping the named plugin (None if hidden)."""
    block = block if block is not None else focused_tab_block()
    prev = None
    for line in block.splitlines():
        if f"superzej/{name}.wasm" in line:
            # the `pane size="..."` line is the wrapper opened just above
            if prev and 'size="' in prev:
                return prev.split('size="', 1)[1].split('"', 1)[0]
            return None
        prev = line.strip()
    return None


print("== setup ==")
if not (os.path.exists(SZ) and shutil.which("zellij")):
    print("SKIP: need target/release/superzej and zellij"); sys.exit(0)

for f in (".panel_state", ".sidebar_state"):
    try: os.remove(os.path.expanduser(f"~/.superzej/{f}"))
    except FileNotFoundError: pass

tmphome = tempfile.mkdtemp()
state = os.path.join(tmphome, "state")
SANDBOX_RUN = os.path.join(tmphome, "run")
SANDBOX_CACHE = os.path.join(tmphome, "cache")
os.makedirs(os.path.join(SANDBOX_CACHE, "zellij")); os.makedirs(SANDBOX_RUN)
for _src in (os.path.expanduser("~/.superzej/cache/zellij/permissions.kdl"),
             os.path.expanduser("~/.cache/zellij/permissions.kdl")):
    if os.path.exists(_src):
        shutil.copy(_src, os.path.join(SANDBOX_CACHE, "zellij", "permissions.kdl")); break

repo = os.path.join(tmphome, f"repo-{os.getpid()}")
os.makedirs(repo)
subprocess.run(["git", "-C", repo, "init", "-q"], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.email=t@e", "-c", "user.name=t",
                "commit", "-q", "--allow-empty", "-m", "init"], check=True)

pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.environ["XDG_STATE_HOME"] = state
    os.environ["ZELLIJ_SOCKET_DIR"] = SANDBOX_RUN
    os.environ["XDG_CACHE_HOME"] = SANDBOX_CACHE
    os.environ["PATH"] = os.path.join(ROOT, "target", "release") + os.pathsep + os.environ["PATH"]
    os.execvp("zellij", ["zellij", "--config", CONFIG, "--session", SESSION])
os.set_blocking(fd, False)


def drain():
    try:
        while True:
            if not os.read(fd, 65536): break
    except (OSError, BlockingIOError): pass


def resize(rows, cols, settle=2.5):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    time.sleep(settle); drain()


def key(seq, wait=2.0):
    os.write(fd, seq); time.sleep(wait); drain()


def cleanup():
    subprocess.run(["zellij", "delete-session", SESSION, "--force"], capture_output=True,
                   env=dict(os.environ, ZELLIJ_SOCKET_DIR=SANDBOX_RUN))
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass
    shutil.rmtree(tmphome, ignore_errors=True)
    for f in (".panel_state", ".sidebar_state"):
        try: os.remove(os.path.expanduser(f"~/.superzej/{f}"))
        except FileNotFoundError: pass


# A comfortable 140 cols throughout, so width auto-hide is NOT in play — this
# isolates the manual-toggle mixed-state path.
resize(45, 140, settle=0.0)
try:
    time.sleep(5); drain()

    print("== spawn (both visible) ==")
    check(len(chrome_plugins()) == 4, f"full chrome at spawn ({chrome_plugins()})")

    # Drive into the mixed state: hide panel, then hide sidebar (both gone).
    print("== hide panel, then hide sidebar (both hidden) ==")
    key(b"\x1b\x10")   # Ctrl+Alt+p: hide panel
    key(b"\x1b\x13")   # Ctrl+Alt+s: hide sidebar
    cp = chrome_plugins()
    check("panel" not in cp and "sidebar" not in cp, f"both surfaces hidden ({cp})")

    # The bug: showing the sidebar while the panel is still hidden ran
    # next_swap_layout() at 4 panes -> no template match -> sidebar stuck at 50%.
    print("== show sidebar while panel stays hidden ==")
    key(b"\x1b\x13")   # Ctrl+Alt+s: show sidebar
    b = focused_tab_block()
    cp = chrome_plugins(b)
    sz = surface_size("sidebar", b)
    check("sidebar" in cp, f"sidebar shown ({cp})")
    check("panel" not in cp, f"panel stays hidden ({cp})")
    check(sz == "12%", f"sidebar restored to its 12% slot, not a 50% split (got size={sz})")
    check('size="50%"' not in b, "no 50/50 split with the center")

    # And bringing the panel back must restore the full template cleanly.
    print("== show panel: full template restored ==")
    key(b"\x1b\x10")   # Ctrl+Alt+p: show panel
    b = focused_tab_block()
    check(len(chrome_plugins(b)) == 4, f"full chrome restored ({chrome_plugins(b)})")
    check(surface_size("sidebar", b) == "12%" and surface_size("panel", b) == "27%",
          f"both surfaces back in their slots "
          f"(sidebar={surface_size('sidebar', b)}, panel={surface_size('panel', b)})")
finally:
    cleanup()

print()
if FAILED:
    print(f"FAIL ({len(FAILED)}):")
    for f in FAILED:
        print(f"  - {f}")
    sys.exit(1)
print("PASS")
