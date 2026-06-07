#!/usr/bin/env python3
"""End-to-end regression test for the top-bar resource-monitor selection
(2026-06-05). Driven via the same `superzej_select_topbar`/`_bottombar` pipes
the Super+Alt keybinds send (MessagePlugin), so it's independent of kitty key
delivery; cursor moves + Enter/Esc are real keystrokes to the focused pane.

  1. Super+Alt+Up focuses the top bar and renders the first stat as an
     accent-filled chip (visual); Esc cancels and refocuses the center.
  2. Enter opens the monitor for the selected stat as a FLOATING pane
     overlaying the center column (not tiled), focused, with no new tab.
  3. vim `l` -> MEM also opens the system monitor (cpu+mem share `[monitor].system`).
  4. `h` clamps at the first stat (left from cpu stays cpu).
  5. GPU segment -> `[monitor].gpu` (gated on a readable GPU counter); Right
     clamps at the last stat.
  6. Super+Alt+Down focuses the bottom statusbar; Enter is reserved (opens
     nothing); Esc leaves it.
  7. With >1 tab, selecting never teleports (the broadcast guard keeps a
     background instance from stealing focus via focus_plugin_pane).

Monitors are configured to distinctive `sleep <mark>` commands so the monitor
pane's dump-layout `args` unambiguously identify WHICH monitor opened — this
also proves `[monitor]` config is honored end-to-end.

Runs a real zellij client on a pty against THIS worktree's freshly built
binary + plugins, in a fully isolated sandbox HOME (its own socket, cache,
state, data + layout dirs) — it can never see or disturb a live session.
"""
import os
import pty
import struct
import subprocess
import sys
import tempfile
import termios
import fcntl
import time
import shutil
import signal

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SZ = os.path.join(ROOT, "target", "release", "superzej")
CONFIG = os.path.join(ROOT, "config", "zellij.kdl")
PLUGINS = {
    "tabbar": "plugin/tabbar/target/wasm32-wasip1/release/superzej-tabbar.wasm",
    "sidebar": "plugin/sidebar/target/wasm32-wasip1/release/superzej-sidebar.wasm",
    "panel": "plugin/panel/target/wasm32-wasip1/release/superzej-panel.wasm",
    "statusbar": "plugin/statusbar/target/wasm32-wasip1/release/superzej-statusbar.wasm",
}
SESSION = f"sz-monitor-{os.getpid()}"
FAILED = []


def ok(msg):
    print(f"  ✓ {msg}")


def bad(msg):
    print(f"  ✗ {msg}")
    FAILED.append(msg)


def check(cond, msg):
    ok(msg) if cond else bad(msg)


# ── setup ──────────────────────────────────────────────────────────────────
print("== setup ==")
missing = [n for n, p in PLUGINS.items() if not os.path.exists(os.path.join(ROOT, p))]
if not (os.path.exists(SZ) and shutil.which("zellij") and not missing):
    print(f"SKIP: need release superzej + plugins + zellij "
          f"(missing plugins: {missing or 'none'})")
    sys.exit(0)

SBX = tempfile.mkdtemp(prefix="sz-mon-")
HOME = SBX
DATA = os.path.join(SBX, ".local/share/superzej")
LAYOUTDIR = os.path.join(SBX, ".config/zellij/layouts")
CFGDIR = os.path.join(SBX, ".config/superzej")
CACHE = os.path.join(SBX, ".superzej/cache")
STATE = os.path.join(SBX, "state")
RUN = os.path.join(SBX, "run")
for d in (DATA, LAYOUTDIR, CFGDIR, CACHE, STATE, RUN):
    os.makedirs(d, exist_ok=True)
# Point the sandbox plugin dir + layout dir at THIS worktree's fresh artifacts.
for name, rel in PLUGINS.items():
    os.symlink(os.path.join(ROOT, rel), os.path.join(DATA, f"{name}.wasm"))
for lay in ("superzej", "home-tab", "worktree-tab", "worktree-tab-extra"):
    src = os.path.join(ROOT, "layouts", f"{lay}.kdl")
    if os.path.exists(src):
        os.symlink(src, os.path.join(LAYOUTDIR, f"{lay}.kdl"))

# Deterministic, dependency-free monitors: a long sleep keeps the pane alive so
# we can observe it; the pane NAME (system/gpu) is what we assert.
# Distinctive sleep durations so the monitor pane's `command`/`args` in
# dump-layout unambiguously identify WHICH monitor (system vs gpu) was opened.
SYS_MARK = "4242"
GPU_MARK = "5353"
with open(os.path.join(CFGDIR, "config.toml"), "w") as f:
    f.write(
        f'worktrees_dir = "{SBX}/wt"\n'
        'name_scheme = "numbered"\n'
        "[monitor]\n"
        f'system = "sleep {SYS_MARK}"\n'
        f'gpu = "sleep {GPU_MARK}"\n'
    )

# This harness may itself run inside a live zellij/superzej — strip the inherited
# ZELLIJ_* vars so the sandbox session never nests into or leaks to it.
_base = {k: v for k, v in os.environ.items()
         if k not in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID")}
# Env for the forked zellij that hosts the session (it picks the name via
# --session, so it must NOT carry ZELLIJ_SESSION_NAME).
CHILD_ENV = dict(
    _base,
    HOME=HOME,
    XDG_CACHE_HOME=CACHE,
    XDG_STATE_HOME=STATE,
    XDG_CONFIG_HOME=os.path.join(SBX, ".config"),
    ZELLIJ_SOCKET_DIR=RUN,
    # superzej-spawned tabs resolve named layouts via --layout-dir = this dir
    # (no longer ~/.config/zellij/layouts). Point it at this worktree's source so
    # `Alt+w` worktree tabs come back with the real worktree-tab layout.
    SUPERZEJ_LAYOUT_DIR=os.path.join(ROOT, "layouts"),
    PATH=os.path.join(ROOT, "target", "release") + os.pathsep + os.environ["PATH"],
)
# Env for the `zellij action`/`pipe` clients — they target the session by name.
ENV = dict(CHILD_ENV, ZELLIJ_SESSION_NAME=SESSION)

# Pre-grant plugin permissions (a prompt is un-approvable in a fixed pane).
subprocess.run([SZ, "grant-plugins"], env=ENV, capture_output=True)

# A throwaway git repo to root the session (the home tab resolves its cwd).
repo = os.path.join(SBX, f"mon-{os.getpid()}")
os.makedirs(repo)
subprocess.run(["git", "-C", repo, "init", "-q"], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.email=t@e", "-c", "user.name=t",
                "commit", "-q", "--allow-empty", "-m", "init"], check=True)

PLUGIN_URL = f"file:{DATA}/tabbar.wasm"
STATUSBAR_URL = f"file:{DATA}/statusbar.wasm"


def act(*args, timeout=10):
    r = subprocess.run(["zellij", "action", *args], env=ENV,
                       capture_output=True, text=True, timeout=timeout)
    return r.stdout


def pipe_plugin(url, name):
    # Mirror the keybind's MessagePlugin via the CLI pipe (payload-bearing so the
    # plugin acts on it, not the trailing EOF message).
    subprocess.run(["zellij", "pipe", "--plugin", url, "--name", name, "--", "x"],
                   env=ENV, capture_output=True, timeout=10)


def focused_pane():
    lines = act("list-clients").strip().splitlines()
    if len(lines) < 2:
        return ""
    parts = lines[1].split(None, 2)
    return " ".join(parts[1:]) if len(parts) >= 2 else ""


def tabs():
    return [t for t in act("query-tab-names").strip().splitlines() if t]


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


pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.environ.clear()
    os.environ.update(CHILD_ENV)
    # No --layout: zellij resolves `default_layout "superzej"` from the config
    # against $XDG_CONFIG_HOME/zellij/layouts (symlinked above). Passing an
    # absolute --layout path makes zellij attach-not-create and the session dies.
    os.execvp("zellij", ["zellij", "--config", CONFIG, "--session", SESSION])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 170, 0, 0))
os.set_blocking(fd, False)


CAP = bytearray()


def drain():
    try:
        while True:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            CAP.extend(chunk)
    except (OSError, BlockingIOError):
        pass


def key(seq, wait=1.0):
    os.write(fd, seq)
    time.sleep(wait)
    drain()


def cleanup():
    subprocess.run(["zellij", "delete-session", SESSION, "--force"],
                   capture_output=True, env=ENV)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    shutil.rmtree(SBX, ignore_errors=True)


try:
    # Poll for the session rather than a fixed sleep — startup time varies with
    # machine load (e.g. a concurrent build), and a connected client takes a
    # moment to lay out the plugins.
    deadline = time.time() + 30
    while time.time() < deadline:
        drain()
        if len(tabs()) == 1:
            break
        time.sleep(0.5)
    up = len(tabs()) == 1
    check(up, f"session up, home tab ({tabs()})")
    if not up:
        sess = subprocess.run(["zellij", "list-sessions", "--no-formatting"],
                              env=ENV, capture_output=True, text=True)
        print("  -- list-sessions --")
        print("    " + (sess.stdout or sess.stderr).replace("\n", "\n    "))
        print("  -- last pty output --")
        print("    " + CAP[-1500:].decode("utf-8", "replace").replace("\n", "\n    "))
    # Let the tabbar poll `superzej stats` so CPU/MEM segments populate.
    time.sleep(3)
    drain()

    before_tabs = tabs()

    # The default accent (#76eede) as the truecolor bg the selected stat fills.
    ACCENT_BG = "48;2;118;238;222m"

    def clear_cap():
        drain()
        CAP.clear()

    def select_top():
        pipe_plugin(PLUGIN_URL, "superzej_select_topbar")
        time.sleep(1.2)
        drain()

    def cap_has(substr, timeout=5.0):
        """Poll the pty until `substr` shows up in the captured output (the
        highlight persists until Esc, so repaints keep re-emitting it)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            drain()
            if substr in CAP.decode("utf-8", "replace"):
                return True
            time.sleep(0.3)
        return False

    def open_pane_count():
        """Leaf command/plugin panes anywhere in the dump (monitor adds one)."""
        return sum(1 for l in dump().splitlines()
                   if l.lstrip().startswith("pane") and
                   ("command=" in l or "plugin location=" in l))

    def floating_monitor(block, mark):
        """The monitor pane (`args "<mark>"`) is a FLOATING pane overlaying the
        center — inside a `floating_panes {…}` block, not tiled between the
        sidebar and panel plugins."""
        depth = 0
        in_float = None  # brace depth at which floating_panes opened
        for l in block.splitlines():
            if in_float is not None and f'"{mark}"' in l:
                return True
            if in_float is None and l.lstrip().startswith("floating_panes"):
                in_float = depth
            depth += l.count("{") - l.count("}")
            if in_float is not None and depth <= in_float:
                in_float = None
        return False

    # ── 1. select highlights the first stat (visual) + Esc cancels ──────────
    print("== select highlights a stat (visual) + Esc cancels ==")
    clear_cap()
    select_top()
    check("tabbar.wasm" in focused_pane(),
          f"Super+Alt+Up focuses the top bar ({focused_pane()})")
    # When selected, the composited output renders CPU as a bold, accent-bg
    # chip (`…[48;2;…m[1mCPU`); unselected it's a faint, non-bold fg. A bold CPU
    # label is a reliable selected-state signal (poll, since the repaint flush
    # is async); then confirm the accent bg sits just before a CPU label.
    bold_cpu = cap_has("\x1b[1mCPU")
    cap = CAP.decode("utf-8", "replace")
    accent_cpu = any(ACCENT_BG in cap[max(0, m - 48):m]
                     for m in (i for i in range(len(cap)) if cap.startswith("CPU", i)))
    check(bold_cpu and accent_cpu,
          "the selected CPU stat renders as a bold accent-filled chip (visual)")
    key(b"\x1b", wait=1.5)  # Esc
    check("tabbar.wasm" not in focused_pane(),
          f"Esc leaves the top bar, back to the center ({focused_pane()})")
    check("command=\"sleep\"" not in dump(), "Esc opened no monitor pane")

    # ── 2. Enter floats the system monitor over the center column, focused ───
    print("== Enter opens the floating system monitor (cpu) ==")
    select_top()
    check(tabs() == before_tabs, "selecting the top bar did not switch tabs")
    key(b"\r", wait=3)  # Enter -> cpu -> system monitor
    block = focused_tab_block()
    check(f'"{SYS_MARK}"' in block, "cpu -> [monitor].system opened")
    check(floating_monitor(block, SYS_MARK),
          "system monitor floats over the center column (not tiled)")
    check(SYS_MARK in focused_pane(),
          f"the floating monitor is focused ({focused_pane()})")
    check(len(tabs()) == len(before_tabs), "no new tab was created")
    act("close-pane")
    time.sleep(1)

    # ── 3. vim `l` -> MEM also opens the system monitor (cpu+mem share it) ───
    print("== vim l -> MEM -> system monitor ==")
    select_top()
    key(b"l", wait=0.6)  # Right -> mem
    key(b"\r", wait=3)
    block = focused_tab_block()
    check(f'"{SYS_MARK}"' in block, "mem -> [monitor].system (shared with cpu)")
    act("close-pane")
    time.sleep(1)

    # ── 4. left-clamp: `h` from the first stat stays on cpu ─────────────────
    print("== vim h clamps at the first stat ==")
    select_top()
    key(b"h", wait=0.6)  # Left from cpu -> stays cpu
    key(b"h", wait=0.6)
    key(b"\r", wait=3)
    block = focused_tab_block()
    check(f'"{SYS_MARK}"' in block and f'"{GPU_MARK}"' not in block,
          "left from the first stat clamps on cpu -> system")
    act("close-pane")
    time.sleep(1)

    # ── 5. GPU segment -> the gpu monitor (only if this box reports a GPU) ───
    print("== GPU stat -> gpu monitor ==")
    stats = subprocess.run([SZ, "stats"], env=ENV, capture_output=True, text=True).stdout
    if "gpu=" in stats:
        select_top()
        key(b"\x1b[C", wait=0.6)  # Right -> mem
        key(b"\x1b[C", wait=0.6)  # Right -> gpu
        key(b"\x1b[C", wait=0.6)  # Right again -> clamps on gpu
        key(b"\r", wait=3)
        block = focused_tab_block()
        check(f'"{GPU_MARK}"' in block,
              "GPU + Enter embeds the gpu monitor ([monitor].gpu)")
        check(f'"{SYS_MARK}"' not in block,
              "GPU opened the gpu monitor, not the system one")
        act("close-pane")
        time.sleep(1)
    else:
        ok("SKIP gpu path (no GPU counter on this box)")

    # ── 6. bottom bar: focus-only, Enter reserved (no-op) ───────────────────
    print("== Super+Alt+Down selects the bottom bar (reserved) ==")
    act("focus-next-pane")
    time.sleep(0.5)
    panes_before = open_pane_count()
    pipe_plugin(STATUSBAR_URL, "superzej_select_bottombar")
    time.sleep(1.2)
    drain()
    check("statusbar.wasm" in focused_pane(),
          f"Super+Alt+Down focuses the bottom bar ({focused_pane()})")
    key(b"\r", wait=1.5)  # Enter is reserved -> nothing should open
    check(open_pane_count() == panes_before,
          "Enter on the bottom bar opens nothing (reserved)")
    key(b"\x1b", wait=1.5)  # Esc leaves it
    check("statusbar.wasm" not in focused_pane(),
          f"Esc leaves the bottom bar ({focused_pane()})")

    # ── 7. broadcast guard: select with >1 tab never teleports ──────────────
    print("== multi-tab: selecting acts only on the active tab ==")
    key(b"\x1bw", wait=7)  # Alt+w -> a worktree tab (center picker, left as-is)
    multi = len(tabs()) >= 2
    if multi:
        active_before = focused_tab_block().splitlines()[0]
        select_top()  # broadcast hits every tab's tabbar; only the active acts
        check(focused_tab_block().splitlines()[0] == active_before,
              "selecting the top bar did not teleport to another tab")
        check("tabbar.wasm" in focused_pane(),
              "the active tab's top bar is the one focused")
        key(b"\x1b", wait=1.0)  # Esc
    else:
        ok(f"SKIP multi-tab guard (worktree tab not created: {tabs()})")
finally:
    cleanup()

print()
if FAILED:
    print(f"FAIL ({len(FAILED)}):")
    for f in FAILED:
        print(f"  - {f}")
    sys.exit(1)
print("PASS")
