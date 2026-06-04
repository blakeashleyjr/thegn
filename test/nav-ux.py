#!/usr/bin/env python3
"""End-to-end test for the navigation UX (2026-06-04 design):

  1. Alt+w opens a worktree tab with the CENTER terminal focused.
  2. Alt+h/j/k/l move pane focus (sidebar <-> terminal <-> panel) and the
     edges never spill into tab switching (MoveFocus, not MoveFocusOrTab).
  3. Alt+t opens a second full-chrome tab on the same worktree, named
     "{base} ·2" (then ·3); the center pane is a plain shell.
  4. zellij tab-mode `n` is repointed to the same flow — no bare tabs.
  5. resolve-worktree maps "{base} ·N" tabs to the base tab's worktree.
  6. With >=2 center terminals, Alt+] cycles swap layouts (chrome pinned).
  7. Toggle regression: Ctrl+Alt+s hide/show restores the sidebar, with one
     AND with two center terminals (next_swap_layout must not be hijacked).

Drives a real zellij client on a pty (layouts/plugins need a connected
client), asserts via `zellij action list-clients` / `dump-layout`.
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
SESSION = f"sz-navux-{os.getpid()}"

FAILED = []


def ok(msg):
    print(f"  ✓ {msg}")


def bad(msg):
    print(f"  ✗ {msg}")
    FAILED.append(msg)


def check(cond, msg):
    ok(msg) if cond else bad(msg)


def act(*args, timeout=10):
    env = dict(os.environ, ZELLIJ_SESSION_NAME=SESSION)
    r = subprocess.run(["zellij", "action", *args], env=env,
                       capture_output=True, text=True, timeout=timeout)
    return r.stdout


def focused_pane():
    """'terminal_3 <cmd>' / 'plugin_5 <url>' for client 1."""
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
    """The focused tab's block from dump-layout (tab nodes sit at indent 4;
    a sibling node at the same indent ends the block)."""
    d = d if d is not None else dump()
    blocks, cur = [], None
    for line in d.splitlines():
        stripped = line.lstrip()
        indent = len(line) - len(stripped)
        if indent == 4 and stripped.startswith("tab "):
            cur = [line]
            blocks.append(cur)
        elif indent == 4 and stripped and cur is not None:
            cur = None  # new_tab_template / swap_tiled_layout etc.
        elif cur is not None:
            cur.append(line)
    for b in blocks:
        if "focus=true" in b[0]:
            return "\n".join(b)
    return ""


def chrome_plugins(block):
    return [p for p in ("sidebar", "tabbar", "panel", "statusbar")
            if f"superzej/{p}.wasm" in block]


# ── setup ────────────────────────────────────────────────────────────────
print("== setup ==")
if not (os.path.exists(SZ) and shutil.which("zellij")):
    print("SKIP: need target/release/superzej and zellij")
    sys.exit(0)

tmphome = tempfile.mkdtemp()
state = os.path.join(tmphome, "state")
# Unique repo name per run: the worktree DEST dir (~/.superzej/worktrees/{slug})
# is keyed by repo basename, so a reused name collides with stale worktrees
# from previous runs (the random branch-name pool is small).
repo = os.path.join(tmphome, f"navux-{os.getpid()}")
os.makedirs(repo)
subprocess.run(["git", "-C", repo, "init", "-q"], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.email=t@e", "-c",
                "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"],
               check=True)

pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.environ["XDG_STATE_HOME"] = state
    os.execvp("zellij", ["zellij", "--config", CONFIG, "--session", SESSION])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 170, 0, 0))
os.set_blocking(fd, False)


def drain():
    try:
        while True:
            if not os.read(fd, 65536):
                break
    except (OSError, BlockingIOError):
        pass


def key(seq, wait=1.0):
    os.write(fd, seq)
    time.sleep(wait)
    drain()


def cleanup():
    subprocess.run(["zellij", "delete-session", SESSION, "--force"],
                   capture_output=True)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    shutil.rmtree(tmphome, ignore_errors=True)
    shutil.rmtree(
        os.path.expanduser(f"~/.superzej/worktrees/{os.path.basename(repo)}"),
        ignore_errors=True)


try:
    time.sleep(4)
    drain()
    check(len(tabs()) == 1, f"session up, home tab ({tabs()})")

    # ── 1. Alt+w: worktree tab, center terminal focused ─────────────────
    print("== Alt+w: new worktree ==")
    key(b"\x1bw", wait=6)
    t = tabs()
    check(len(t) == 2 and "/" in t[-1] and not t[-1].endswith("/home"),
          f"worktree tab created ({t})")
    wt_tab = t[-1]
    check("terminal" in focused_pane(),
          f"center terminal focused after create ({focused_pane()})")

    # ── 2. Alt+h/l focus nav, no tab spill ───────────────────────────────
    print("== Alt+h/j/k/l focus nav ==")
    key(b"\x1bh")
    check("sidebar.wasm" in focused_pane(), "Alt+h -> sidebar")
    before = tabs()
    key(b"\x1bh")
    check("sidebar.wasm" in focused_pane() and tabs() == before,
          "Alt+h at the left edge stays put (no tab switch)")
    key(b"\x1bl")
    check("terminal" in focused_pane(), "Alt+l -> center terminal")
    key(b"\x1bl")
    check("panel.wasm" in focused_pane(), "Alt+l -> diff/PR panel")
    key(b"\x1bl")
    check("panel.wasm" in focused_pane(),
          "Alt+l at the right edge stays put (no tab switch)")
    key(b"\x1bh")
    check("terminal" in focused_pane(), "Alt+h -> back to center terminal")

    # ── 3. Alt+t: second tab on the same worktree ────────────────────────
    print("== Alt+t: same-worktree tabs ==")
    key(b"\x1bt", wait=4)
    t = tabs()
    check(f"{wt_tab} ·2" in t, f"tab '{wt_tab} ·2' created ({t})")
    block = focused_tab_block()
    check(block.lstrip().startswith(f'tab name="{wt_tab} ·2"'),
          "the ·2 tab is focused")
    check(len(chrome_plugins(block)) == 4,
          f"full chrome present ({chrome_plugins(block)})")
    check("terminal" in focused_pane(),
          f"plain shell focused in center ({focused_pane()})")

    key(b"\x1bt", wait=4)
    check(f"{wt_tab} ·3" in tabs(), "second Alt+t -> ·3")

    # ── 4. tab-mode `n` repointed ────────────────────────────────────────
    print("== tab-mode n ==")
    key(b"\x14", wait=0.5)   # Ctrl+t -> tab mode
    key(b"n", wait=4)
    t = tabs()
    check(f"{wt_tab} ·4" in t and not any(x.startswith("Tab #") for x in t),
          f"tab-mode n -> ·4, no bare 'Tab #N' ({t})")

    # ── 5. sidebar tree: repo -> worktree -> tabs ────────────────────────
    print("== sidebar tree navigation ==")
    home_tab = tabs()[0]

    def focused_tab_name():
        b = focused_tab_block()
        first = b.splitlines()[0].lstrip() if b else ""
        parts = first.split('name="', 1)
        return parts[1].split('"', 1)[0] if len(parts) == 2 else ""

    # rows: 0 repo · 1 home · 2 worktree · 3-6 pages ·1-·4 · 7 +worktree
    key(b"\x1bh")
    check("sidebar.wasm" in focused_pane(), "sidebar focused for tree nav")
    key(b"j")
    key(b"\r", wait=1.5)  # row 1: the home worktree row
    check(focused_tab_name() == home_tab,
          f"home row -> home tab ({focused_tab_name()!r})")
    key(b"\x1bh")
    key(b"j")
    key(b"j")
    key(b"\r", wait=1.5)  # row 2: the worktree row -> its base tab
    check(focused_tab_name() == wt_tab,
          f"worktree row -> base tab ({focused_tab_name()!r})")
    key(b"\x1bh")
    for _ in range(4):
        key(b"j")
    key(b"\r", wait=1.5)  # row 4: page ·2
    check(focused_tab_name() == f"{wt_tab} ·2",
          f"page ·2 row -> its tab ({focused_tab_name()!r})")

    # ── 6. resolve-worktree strips the page suffix ───────────────────────
    print("== resolve-worktree ==")
    env = dict(os.environ, XDG_STATE_HOME=state)
    r = subprocess.run(
        [SZ, "resolve-worktree", "--session", SESSION, "--tab",
         f"{wt_tab} ·2"],
        env=env, capture_output=True, text=True)
    resolved = r.stdout.strip()
    check(resolved and os.path.isdir(resolved),
          f"'{wt_tab} ·2' resolves to the worktree ({resolved or 'NOTHING'})")

    # ── 6. swap layouts on the center column ─────────────────────────────
    print("== Alt+] swap layouts ==")
    act("go-to-tab-name", wt_tab)
    time.sleep(0.7)
    drain()
    key(b"\x1bn", wait=2)            # Alt+n: split a second center terminal
    base_block = focused_tab_block()
    check(len(chrome_plugins(base_block)) == 4,
          "chrome intact after Alt+n split")
    key(b"\x1b]", wait=1.5)          # next swap layout
    swapped = focused_tab_block()
    check(len(chrome_plugins(swapped)) == 4, "chrome intact after Alt+]")
    changed = swapped != base_block
    key(b"\x1b]", wait=1.5)
    swapped2 = focused_tab_block()
    check(changed or swapped2 != swapped,
          "Alt+] cycles center arrangements (layout changed)")
    check(len(chrome_plugins(swapped2)) == 4,
          "chrome intact after second Alt+]")

    # ── 7. sidebar toggle regression (next_swap_layout restore) ──────────
    print("== Ctrl+Alt+s toggle regression ==")
    key(b"\x1b\x13", wait=2)         # Ctrl+Alt+s: hide sidebar
    check("sidebar.wasm" not in focused_tab_block(),
          "sidebar hidden with 2 center terminals")
    key(b"\x1b\x13", wait=2)         # show again
    restored = focused_tab_block()
    check("sidebar.wasm" in restored,
          "sidebar restored with 2 center terminals")
    check(len(chrome_plugins(restored)) == 4,
          "full chrome after toggle restore")

    # close the split pane -> back to one center terminal, toggle again
    act("close-pane")
    time.sleep(1)
    key(b"\x1b\x13", wait=2)
    check("sidebar.wasm" not in focused_tab_block(),
          "sidebar hidden with 1 center terminal")
    key(b"\x1b\x13", wait=2)
    restored = focused_tab_block()
    check("sidebar.wasm" in restored and len(chrome_plugins(restored)) == 4,
          "sidebar restored to template with 1 center terminal")

    # ── 8. Alt+N: one in-place panel, no dead command pane ───────────────
    print("== Alt+N scoped panel ==")

    def pane_count(block):
        # leaf panes only — `split_direction` lines are layout containers,
        # and a split wraps siblings in a fresh container node
        return sum(1 for line in block.splitlines()
                   if line.lstrip().startswith("pane")
                   and "split_direction" not in line)

    before_panes = pane_count(focused_tab_block())
    key(b"\x1bN", wait=3)
    block = focused_tab_block()
    check('name="panel"' in block, "panel pane created (renamed in place)")
    check("new-panel" not in block,
          "no leftover exited 'superzej new-panel' pane")
    check(pane_count(block) == before_panes + 1,
          f"exactly one pane added ({before_panes} -> {pane_count(block)})")
finally:
    cleanup()

print()
if FAILED:
    print(f"FAIL ({len(FAILED)}):")
    for f in FAILED:
        print(f"  - {f}")
    sys.exit(1)
print("PASS")
