#!/usr/bin/env python3
"""Visual + structural regression for the bottom file-manager drawer (yazi).

Drives a FULLY SANDBOXED, SELF-CONTAINED superzej session on a pty and exercises
the drawer across many situations:

  • open via Ctrl+Alt+f → a bottom-anchored floating `superzej-files` pane
    appears in the focused tab, and yazi actually renders in the bottom rows
    (pyte screen capture);
  • toggle closed (Ctrl+Alt+f again) and `q`-close both remove it and record
    the dismiss in the per-worktree state file;
  • re-open records open again, and is idempotent (no duplicate panes);
  • a narrow terminal still opens the drawer (percentage geometry);
  • per-tab independence: exactly one drawer pane exists in the session;
  • restore: `superzej files --restore` re-opens a worktree left OPEN and
    no-ops one left CLOSED (the statusbar's poke target).

Self-contained per the plugin-e2e pattern: a sandbox HOME holds the
WORKTREE-built plugins (so the NEW statusbar — close pipe + restore — is what
loads), with permissions seeded for those paths and a private socket/cache/state
+ SUPERZEJ_DIR. Never touches a real session or your ~/.superzej.

Needs the release binary, the built plugins, zellij, and a yazi
(SUPERZEJ_YAZI_BIN or `yazi` on PATH); SKIPs otherwise.
"""
import os, pty, signal, struct, subprocess, sys, tempfile, termios, fcntl, time, shutil

try:
    import pyte
except ImportError:
    pyte = None

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SZ = os.path.join(ROOT, "target", "release", "superzej")
CONFIG = os.path.join(ROOT, "config", "zellij.kdl")
SESSION = f"sz-files-{os.getpid()}"
CAF = b"\x1b\x06"  # Ctrl+Alt+f (Alt=ESC prefix, Ctrl+f=0x06)
ROWS, COLS = 45, 160
WIDTH = os.environ.get("SZ_TEST_DRAWER_WIDTH", "full")

FAILED = []
def ok(m): print(f"  \033[32m✓\033[0m {m}")
def bad(m): print(f"  \033[31m✗\033[0m {m}"); FAILED.append(m)
def check(cond, m): ok(m) if cond else bad(m)

PLUGINS = ("sidebar", "panel", "tabbar", "statusbar")


def plugin_wasm(name):
    # The crate builds `superzej-<name>.wasm`; install.sh symlinks it as <name>.wasm.
    return os.path.join(ROOT, "plugin", name, "target", "wasm32-wasip1", "release",
                        f"superzej-{name}.wasm")


def have_prereqs():
    if not (os.path.exists(SZ) and shutil.which("zellij")):
        return False
    if not all(os.path.exists(plugin_wasm(p)) for p in PLUGINS):
        return False
    return bool(os.environ.get("SUPERZEJ_YAZI_BIN")) or shutil.which("yazi")


if not have_prereqs():
    print("SKIP: need `just release build-plugins`, zellij, and yazi (SUPERZEJ_YAZI_BIN or PATH)")
    sys.exit(0)

# ── sandbox (self-contained: HOME holds the worktree-built plugins) ──────────
HOME = tempfile.mkdtemp()
SZ_DIR = os.path.join(HOME, "sz")                 # SUPERZEJ_DIR (state + yazi cfg)
CACHE = os.path.join(SZ_DIR, "cache")             # XDG_CACHE_HOME == superzej_dir/cache
RUN = os.path.join(SZ_DIR, "run")                 # private socket dir
STATE = os.path.join(HOME, "state")
XDG_CONFIG = os.path.join(HOME, ".config")
PLUGDIR = os.path.join(HOME, ".local", "share", "superzej")
LAYOUTDIR = os.path.join(XDG_CONFIG, "zellij", "layouts")
for d in (os.path.join(CACHE, "zellij"), RUN, STATE,
          os.path.join(XDG_CONFIG, "superzej"), PLUGDIR, LAYOUTDIR):
    os.makedirs(d)
for p in PLUGINS:
    shutil.copy(plugin_wasm(p), os.path.join(PLUGDIR, f"{p}.wasm"))
# The config's `default_layout "superzej"` resolves from here (HOME is sandboxed,
# so the real ~/.config layouts aren't visible).
for lay in ("superzej", "home-tab", "worktree-tab", "worktree-tab-extra"):
    shutil.copy(os.path.join(ROOT, "layouts", f"{lay}.kdl"), os.path.join(LAYOUTDIR, f"{lay}.kdl"))

with open(os.path.join(XDG_CONFIG, "superzej", "config.toml"), "w") as f:
    f.write(f'[drawer]\nwidth = "{WIDTH}"\nheight = "35%"\n')

CHILD_ENV = dict(
    HOME=HOME, XDG_STATE_HOME=STATE, XDG_CACHE_HOME=CACHE, XDG_CONFIG_HOME=XDG_CONFIG,
    ZELLIJ_SOCKET_DIR=RUN, SUPERZEJ_DIR=SZ_DIR,
    PATH=os.path.join(ROOT, "target", "release") + os.pathsep + os.environ["PATH"],
)
if os.environ.get("SUPERZEJ_YAZI_BIN"):
    CHILD_ENV["SUPERZEJ_YAZI_BIN"] = os.environ["SUPERZEJ_YAZI_BIN"]

# Seed plugin permissions for THESE plugin paths (zellij keys by abs path).
subprocess.run([SZ, "grant-plugins"], env=dict(os.environ, **CHILD_ENV),
               capture_output=True, timeout=20)

# A repo with deterministic files so yazi has stable content.
repo = os.path.join(HOME, f"repo-{os.getpid()}")
os.makedirs(repo)
for name in ("ALPHA_FILE.txt", "BETA_FILE.md"):
    open(os.path.join(repo, name), "w").write("x\n")
subprocess.run(["git", "-C", repo, "init", "-q"], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.email=t@e", "-c", "user.name=t", "add", "-A"], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.email=t@e", "-c", "user.name=t",
                "commit", "-q", "-m", "init"], check=True)

# Safety: never operate on anything but our throwaway sandbox socket.
assert RUN.startswith(HOME) and "tmp" in HOME, f"refusing non-sandbox socket dir {RUN}"

pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.environ.update(CHILD_ENV)
    os.execvp("zellij", ["zellij", "--config", CONFIG, "--session", SESSION])
os.set_blocking(fd, False)

cap = bytearray()
def drain():
    try:
        while True:
            b = os.read(fd, 65536)
            if not b: break
            cap.extend(b)
    except (OSError, BlockingIOError): pass

def resize(rows, cols, settle=2.5):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    time.sleep(settle); drain()

def key(seq, wait=2.5):
    os.write(fd, seq); time.sleep(wait); drain()

def act(*args, timeout=10):
    env = dict(os.environ, **CHILD_ENV, ZELLIJ_SESSION_NAME=SESSION)
    return subprocess.run(["zellij", "action", *args], env=env,
                          capture_output=True, text=True, timeout=timeout).stdout

def sz(*args, timeout=20):
    env = dict(os.environ, **CHILD_ENV, ZELLIJ_SESSION_NAME=SESSION)
    return subprocess.run([SZ, *args], env=env, capture_output=True, text=True,
                          cwd=repo, timeout=timeout)

def focused_block(d=None):
    d = d if d is not None else act("dump-layout")
    blocks, cur = [], None
    for line in d.splitlines():
        stripped = line.lstrip(); indent = len(line) - len(stripped)
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

def focused_tab_name():
    line = (focused_block() or "\n").splitlines()[0]
    return line.split('name="', 1)[1].split('"', 1)[0] if 'name="' in line else ""

def drawer_present(block=None):
    block = block if block is not None else focused_block()
    return 'name="superzej-files"' in block

def drawer_count():
    return act("dump-layout").count('name="superzej-files"')

def state_value(slug):
    try:
        return open(os.path.join(SZ_DIR, "drawer", slug)).read().strip()
    except FileNotFoundError:
        return None

def only_slug():
    ddir = os.path.join(SZ_DIR, "drawer")
    return (os.listdir(ddir) or [None])[0] if os.path.isdir(ddir) else None

def screen_rows():
    if pyte is None:
        return None
    s = pyte.Screen(COLS, ROWS)
    s.report_device_status = lambda *a, **k: None
    pyte.ByteStream(s).feed(bytes(cap))
    return list(s.display)

def wait_for_screen(needles, timeout=12.0):
    """Poll the live stream, rebuilding the screen until any needle shows (or
    timeout). yazi paints asynchronously after its pane appears, so a single
    snapshot races it; polling makes the visual assertion deterministic."""
    if pyte is None:
        return None
    deadline = time.time() + timeout
    rows = screen_rows()
    while time.time() < deadline:
        os.write(fd, b"\x0c")  # Ctrl+L: nudge a redraw
        time.sleep(0.6)
        drain()
        rows = screen_rows()
        if any(n in "\n".join(rows) for n in needles):
            return rows
    return rows


def cleanup():
    subprocess.run(["zellij", "delete-session", SESSION, "--force"], capture_output=True,
                   env=dict(os.environ, ZELLIJ_SOCKET_DIR=RUN))
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass
    shutil.rmtree(HOME, ignore_errors=True)


resize(ROWS, COLS, settle=0.0)
try:
    time.sleep(6); drain()
    print(f"== drawer regression (width={WIDTH}) ==")
    check(len(focused_block()) > 0, "session up with a focused tab")
    check(not drawer_present(), "drawer hidden by default")

    print("== open (Ctrl+Alt+f) ==")
    key(CAF, wait=4.0)
    blk = focused_block()
    check(drawer_present(blk), "drawer pane appears in the focused tab")
    check("floating" in blk, "drawer is a floating pane")
    slug = only_slug()
    check(slug is not None and state_value(slug) == "true",
          f"open recorded per-worktree (slug={slug})")
    yconf = os.path.join(SZ_DIR, "yazi")
    check(all(os.path.exists(os.path.join(yconf, f))
              for f in ("yazi.toml", "keymap.toml", "theme.toml")),
          "bundled yazi config seeded into the private YAZI_CONFIG_HOME")
    check(drawer_count() == 1, "exactly one drawer pane in the session")

    print("== visual capture (pyte) ==")
    rows = wait_for_screen(["ALPHA_FILE", "BETA_FILE"])
    if os.environ.get("SZ_DEBUG_SCREEN") and rows:
        for _i, _r in enumerate(rows):
            if _r.strip():
                print(f"R{_i:02d}|{_r.rstrip()}")
    if rows is None:
        ok("pyte not installed — skipping screen capture (structural checks stand)")
    else:
        split = int(ROWS * 0.62)
        bottom = "\n".join(rows[split:])
        top = "\n".join(rows[:split])
        check(("ALPHA_FILE" in bottom) or ("BETA_FILE" in bottom),
              "yazi renders the repo file listing in the bottom drawer region")
        check("ALPHA_FILE" not in top,
              "the file listing is confined to the drawer (not the upper area)")

    print("== q closes + records dismiss ==")
    key(b"q", wait=3.5)
    check(not drawer_present(), "drawer removed after q")
    check(state_value(slug) == "false", "q dismiss recorded (state=false)")

    print("== restore honors per-worktree state ==")
    # Use --tab here (the statusbar's real poke form) so the DB tab->worktree
    # resolution path is exercised; it falls back to the cwd for a home tab.
    sz("files", "--restore", "--tab", focused_tab_name(), "--session", SESSION)
    time.sleep(3.0); drain()
    check(not drawer_present(), "restore does NOT reopen a worktree left closed")

    key(CAF, wait=4.0)
    check(drawer_present() and state_value(slug) == "true", "reopened + recorded open")
    sz("files", "--restore", "--worktree", repo, "--session", SESSION)
    time.sleep(2.5); drain()
    check(drawer_count() == 1, "restore is idempotent when already open")

    print("== toggle close (Ctrl+Alt+f) ==")
    key(CAF, wait=3.5)
    check(not drawer_present(), "drawer toggled closed")
    check(state_value(slug) == "false", "toggle-close recorded")

    print("== narrow terminal ==")
    resize(40, 80, settle=2.5)
    key(CAF, wait=4.0)
    check(drawer_present(), "drawer opens on a narrow (80-col) terminal")
    key(CAF, wait=2.5)
finally:
    cleanup()

print()
if FAILED:
    print(f"FAIL ({len(FAILED)}):")
    for f in FAILED:
        print(f"  - {f}")
    sys.exit(1)
print("PASS")
