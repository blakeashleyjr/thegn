#!/usr/bin/env python3
"""Repro + regression for the "native resize breaks the chrome" bugs.

After a user drags a pane border (native zellij resize), the tab's layout is
"modified" and no swap_tiled_layout variant matches it any more. Symptoms:
  A. Ctrl+Alt+p / Ctrl+Alt+s toggles no longer restore the surface to its slot.
  B. the tabbar slowly oscillates (the center column width flip-flops as the
     statusbar's reconcile() thrashes).

This drives a real `zellij action resize` (the headless equivalent of the mouse
drag), then asserts the toggle still restores and the geometry is stable.

Fully isolated sandbox (private socket/cache/state + SUPERZEJ_DIR) — mirrors
test/panel-resize.py; never touches a real session.
"""
import os, pty, signal, struct, subprocess, sys, tempfile, termios, fcntl, time, shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SZ = os.path.join(ROOT, "target", "release", "superzej")
CONFIG = os.path.join(ROOT, "config", "zellij.kdl")
SESSION = f"sz-resize-repro-{os.getpid()}"
FAILED = []


def ok(m): print(f"  ✓ {m}")
def bad(m): print(f"  ✗ {m}"); FAILED.append(m)
def check(c, m): ok(m) if c else bad(m)


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
    block = block if block is not None else focused_tab_block()
    prev = None
    for line in block.splitlines():
        if f"superzej/{name}.wasm" in line:
            if prev and 'size="' in prev:
                return prev.split('size="', 1)[1].split('"', 1)[0]
            return None
        prev = line.strip()
    return None


print("== setup ==")
if not (os.path.exists(SZ) and shutil.which("zellij")):
    print("SKIP: need target/release/superzej and zellij"); sys.exit(0)

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
    os.environ["SUPERZEJ_DIR"] = tmphome
    for _v in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID"):
        os.environ.pop(_v, None)
    os.environ["PATH"] = os.path.join(ROOT, "target", "release") + os.pathsep + os.environ["PATH"]
    os.execvp("zellij", ["zellij", "--config", CONFIG, "--session", SESSION])
os.set_blocking(fd, False)


def drain():
    try:
        while True:
            if not os.read(fd, 65536): break
    except (OSError, BlockingIOError): pass


def resize_term(rows, cols, settle=2.5):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    time.sleep(settle); drain()


def key(seq, wait=2.0):
    os.write(fd, seq); time.sleep(wait); drain()


def mouse_drag(c1, r, c2, wait=2.5):
    """Inject an SGR mouse press→move→release (a native pane-border drag)."""
    os.write(fd, f"\x1b[<0;{c1};{r}M".encode())  # left press
    time.sleep(0.2)
    step = 1 if c2 >= c1 else -1
    for c in range(c1, c2 + step, step):         # drag (button held = +32 motion bit)
        os.write(fd, f"\x1b[<32;{c};{r}M".encode()); time.sleep(0.05)
    os.write(fd, f"\x1b[<0;{c2};{r}m".encode())   # release
    time.sleep(wait); drain()


def cleanup():
    subprocess.run(["zellij", "delete-session", SESSION, "--force"], capture_output=True,
                   env=dict(os.environ, ZELLIJ_SOCKET_DIR=SANDBOX_RUN))
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass
    shutil.rmtree(tmphome, ignore_errors=True)


resize_term(45, 140, settle=0.0)
try:
    time.sleep(5); drain()
    print("== baseline (full chrome at 140 cols) ==")
    check(len(chrome_plugins()) == 4, f"full chrome at spawn ({chrome_plugins()})")

    print("== native resize: MOUSE-DRAG the sidebar|center border rightward ==")
    # At 140 cols the sidebar is 12% ≈ 17 cols, so its right border sits near
    # col 18; body rows start under the 1-row tabbar. Drag it out to ~col 34.
    sz_before = (surface_size("sidebar"), surface_size("panel"))
    mouse_drag(18, 20, 34)
    blk = focused_tab_block()
    print(f"   chrome after drag: {chrome_plugins(blk)}  "
          f"sizes {sz_before} -> ({surface_size('sidebar', blk)}, {surface_size('panel', blk)})")

    print("== B: geometry must be STABLE after resize (no tabbar oscillation) ==")
    g1 = focused_tab_block(); time.sleep(4); g2 = focused_tab_block()
    check(g1 == g2, "focused-tab layout identical across a 4s idle window (no oscillation)")

    print("== A: hide+show panel must restore it to its 27% slot ==")
    key(b"\x1b\x10")  # Ctrl+Alt+p hide
    cp = chrome_plugins()
    check("panel" not in cp, f"Ctrl+Alt+p hid the panel ({cp})")
    key(b"\x1b\x10")  # Ctrl+Alt+p show
    b = focused_tab_block()
    check("panel" in chrome_plugins(b), f"panel reappeared ({chrome_plugins(b)})")
    check(surface_size("panel", b) == "27%",
          f"panel restored to its 27% slot, not a 50% split (got {surface_size('panel', b)})")
    check('size="50%"' not in b, "no 50/50 split with the center after restore")

    print("== A: hide+show sidebar must restore it to its 12% slot ==")
    key(b"\x1b\x13")  # Ctrl+Alt+s hide
    check("sidebar" not in chrome_plugins(), "Ctrl+Alt+s hid the sidebar")
    key(b"\x1b\x13")  # Ctrl+Alt+s show
    b = focused_tab_block()
    check(surface_size("sidebar", b) == "12%",
          f"sidebar restored to its 12% slot (got {surface_size('sidebar', b)})")

    # ── Phase 2: TWO center terminals (swap_tiled_layout variants engage at
    # min_panes=6) — the case the memory flags as fragile. ─────────────────────
    print("== phase 2: open a 2nd center terminal, then mouse-drag resize ==")
    key(b"\x1bn")  # Alt+n: NewPane Down (a second center terminal)
    b = focused_tab_block()
    n_terms = b.count('pane ') - b.count('plugin ')  # rough: terminals are plugin-less panes
    print(f"   chrome now: {chrome_plugins(b)} (raw block has {b.count('superzej/')} plugins)")
    mouse_drag(18, 20, 36)
    time.sleep(1.0); drain()

    print("== B(2): geometry stable with 2 terminals after drag ==")
    g1 = focused_tab_block(); time.sleep(5); g2 = focused_tab_block()
    check(g1 == g2, "2-terminal layout identical across a 5s idle window (no oscillation)")

    print("== A(2): toggle panel restores with 2 center terminals ==")
    key(b"\x1b\x10")  # hide panel
    check("panel" not in chrome_plugins(), "panel hidden (2-term)")
    key(b"\x1b\x10")  # show panel
    b = focused_tab_block()
    check("panel" in chrome_plugins(b), f"panel restored (2-term) ({chrome_plugins(b)})")
    check('size="50%"' not in b or chrome_plugins(b) == ["sidebar", "tabbar", "panel", "statusbar"],
          "panel not jammed as a 50% split after restore (2-term)")
finally:
    cleanup()

print()
if FAILED:
    print(f"FAIL ({len(FAILED)}):")
    for f in FAILED: print(f"  - {f}")
    sys.exit(1)
print("PASS")
