#!/usr/bin/env python3
"""Repro + regression for the multi-tab toggle corruption.

Reported sequence: on a tab with several others open, drag the sidebar border
wider, then the Ctrl+Alt+s/p toggles stop restoring — switch to another tab and
back and the chrome is jammed at a 50/50 split with a surface gone.

Root cause: the toggle keybind broadcasts to EVERY tab's statusbar instance, and
reconcile() calls next_swap_layout() (which acts on the FOCUSED tab). With N
tabs that fires N times on the visible tab. The fix gates reconcile() to the
active tab only. This needs ≥2 tabs to trigger — single-tab harnesses miss it.

Boots the isolated `~/.superzej-repro` instance (run test/gen-fixture.sh repro
first) with its multi-tab stress layout. Fully sandboxed.
"""
import os, pty, signal, struct, subprocess, sys, termios, fcntl, time

NAME = "repro"
INST = os.path.expanduser(f"~/.superzej-{NAME}")
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SZ = os.path.join(ROOT, "target", "release", "superzej")
ZJ = subprocess.run(["nix", "build", "--no-link", "--print-out-paths", ".#zellij"],
                    cwd=ROOT, capture_output=True, text=True).stdout.strip() + "/bin/zellij"
REPO = f"{INST}/fixtures/repos/east/washu"
SESSION = "repro"
FAILED = []


def ok(m): print(f"  ✓ {m}")
def bad(m): print(f"  ✗ {m}"); FAILED.append(m)
def check(c, m): ok(m) if c else bad(m)

base = dict(os.environ, SUPERZEJ_DIR=INST, XDG_STATE_HOME=f"{INST}/state",
            XDG_CONFIG_HOME=f"{INST}/config", ZELLIJ_SOCKET_DIR=f"{INST}/run",
            XDG_CACHE_HOME=f"{INST}/cache", SUPERZEJ_ZELLIJ_BIN=ZJ,
            SUPERZEJ_LAYOUT=f"{INST}/layout-stress.kdl",
            SUPERZEJ_CONFIG=os.path.join(ROOT, "config", "zellij.kdl"), SUPERZEJ_FRESH="1")
for k in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID", "SUPERZEJ_NO_EXEC"):
    base.pop(k, None)


def act(*a, timeout=15):
    e = dict(base, ZELLIJ_SESSION_NAME=SESSION)
    return subprocess.run([ZJ, "action", *a], env=e, capture_output=True, text=True, timeout=timeout).stdout


def focused_tab_block(d=None):
    d = d if d is not None else act("dump-layout")
    blocks, cur = [], None
    for line in d.splitlines():
        s = line.lstrip(); indent = len(line) - len(s)
        if indent == 4 and s.startswith("tab "):
            cur = [line]; blocks.append(cur)
        elif indent == 4 and s and cur is not None:
            cur = None
        elif cur is not None:
            cur.append(line)
    for b in blocks:
        if "focus=true" in b[0]:
            return "\n".join(b)
    return ""


def chrome_plugins(b=None):
    b = b if b is not None else focused_tab_block()
    return [p for p in ("sidebar", "tabbar", "panel", "statusbar") if f"superzej/{p}.wasm" in b]


def surface_size(name, b=None):
    b = b if b is not None else focused_tab_block()
    prev = None
    for line in b.splitlines():
        if f"superzej/{name}.wasm" in line:
            return prev.split('size="', 1)[1].split('"', 1)[0] if (prev and 'size="' in prev) else None
        prev = line.strip()
    return None


if not (os.path.exists(SZ) and os.path.isdir(f"{INST}/state")):
    print(f"SKIP: need {SZ} and {INST} (run: just stress-gen {NAME} 3 6)"); sys.exit(0)

pid, fd = pty.fork()
if pid == 0:
    os.chdir(INST)
    os.execvpe(SZ, [SZ, "new-workspace", REPO], base)
    os._exit(127)
os.set_blocking(fd, False)


def drain():
    try:
        while True:
            if not os.read(fd, 65536): break
    except (OSError, BlockingIOError): pass


def key(seq, wait=2.0):
    os.write(fd, seq); time.sleep(wait); drain()


def mouse_drag(c1, r, c2, wait=2.5):
    os.write(fd, f"\x1b[<0;{c1};{r}M".encode()); time.sleep(0.2)
    step = 1 if c2 >= c1 else -1
    for c in range(c1, c2 + step, step):
        os.write(fd, f"\x1b[<32;{c};{r}M".encode()); time.sleep(0.05)
    os.write(fd, f"\x1b[<0;{c2};{r}m".encode()); time.sleep(wait); drain()


fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 150, 0, 0))
try:
    print("== boot multi-tab session ==")
    names = []
    for _ in range(25):
        time.sleep(2); drain()
        r = act("query-tab-names")
        names = r.strip().splitlines() if r.strip() else []
        if len(names) >= 3 and len(chrome_plugins()) == 4:
            break  # wait for the active tab's chrome to finish loading
    check(len(names) >= 3, f"session up with {len(names)} tabs")
    b0 = focused_tab_block()
    check(len(chrome_plugins(b0)) == 4, f"full chrome on the active (home) tab ({chrome_plugins(b0)})")
    # Stability: with many tabs, every statusbar firing next_swap_layout() on the
    # active tab makes the chrome thrash. The active tab's layout must be steady.
    g1 = focused_tab_block(); time.sleep(3); g2 = focused_tab_block()
    check(g1 == g2, "active-tab chrome stable across a 3s window (no multi-statusbar thrash)")

    print("== drag the sidebar border wider (native resize) ==")
    mouse_drag(20, 22, 40)
    print(f"   sidebar now {surface_size('sidebar')} (dragged)")

    print("== toggle sidebar off+on — must restore to 12%, not break ==")
    key(b"\x1b\x13"); check("sidebar" not in chrome_plugins(), "Ctrl+Alt+s hid the sidebar")
    key(b"\x1b\x13")
    check(surface_size("sidebar") == "12%", f"sidebar restored to 12% (got {surface_size('sidebar')})")

    print("== switch to another tab, then back to home ==")
    act("go-to-next-tab"); time.sleep(2); drain()
    act("go-to-previous-tab"); time.sleep(2); drain()

    print("== back on home: chrome must be intact (THE reported bug) ==")
    b = focused_tab_block()
    cp = chrome_plugins(b)
    check(len(cp) == 4, f"all four chrome surfaces present (got {cp})")
    check('size="50%"' not in b, "center NOT jammed at a 50/50 split")
    check(surface_size("sidebar", b) == "12%", f"sidebar at 12% (got {surface_size('sidebar', b)})")
    check(surface_size("panel", b) == "27%", f"panel at 27% (got {surface_size('panel', b)})")

    print("== sidebar toggle still works after the round trip ==")
    key(b"\x1b\x13"); check("sidebar" not in chrome_plugins(), "Ctrl+Alt+s still hides")
    key(b"\x1b\x13"); check(surface_size("sidebar") == "12%", "Ctrl+Alt+s still restores to 12%")
finally:
    act("delete-session", SESSION, "--force")
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass

print()
if FAILED:
    print(f"FAIL ({len(FAILED)}):")
    for f in FAILED: print(f"  - {f}")
    sys.exit(1)
print("PASS")
