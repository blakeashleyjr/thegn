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

Drives a real zellij client on a pty (layouts/plugins need a connected client)
in a fully isolated sandbox HOME — its own socket/cache/state, this worktree's
freshly built binary + plugins symlinked in, and SUPERZEJ_LAYOUT_DIR pointed at
the source `layouts/` so `superzej new-worktree` tabs come back WITH chrome
(without it they resolve no layout and come up bare — the old harness's whole
failure mode). Asserts off `zellij action dump-layout` (focus is read from the
dump, not the laggy list-clients); every step polls the asserted end-state via
settle() and retries dropped keystrokes, since a headless pty under load drops
keys and returns transient/empty queries. See the superzej-navux-runaway memory
for why the session can still spiral under heavy external load.
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
PLUGINS = {
    "tabbar": "plugin/tabbar/target/wasm32-wasip1/release/superzej-tabbar.wasm",
    "sidebar": "plugin/sidebar/target/wasm32-wasip1/release/superzej-sidebar.wasm",
    "panel": "plugin/panel/target/wasm32-wasip1/release/superzej-panel.wasm",
    "statusbar": "plugin/statusbar/target/wasm32-wasip1/release/superzej-statusbar.wasm",
}
SESSION = f"sz-navux-{os.getpid()}"

FAILED = []


def ok(msg):
    print(f"  ✓ {msg}")


def bad(msg):
    print(f"  ✗ {msg}")
    FAILED.append(msg)


def check(cond, msg):
    ok(msg) if cond else bad(msg)


# Read-only queries that ALWAYS have output for a live session — an empty
# result means the action raced the daemon (it returns "" under load), so it's
# safe to retry rather than propagate a spurious empty.
_QUERIES = {"list-clients", "dump-layout", "query-tab-names"}


def act(*args, timeout=10):
    # ENV pins ZELLIJ_SOCKET_DIR to the sandbox session namespace — these
    # clients can never reach a real (system or live-superzej) session.
    for _ in range(6):
        r = subprocess.run(["zellij", "action", *args], env=ENV,
                           capture_output=True, text=True, timeout=timeout)
        if r.stdout.strip() or args[0] not in _QUERIES:
            return r.stdout
        time.sleep(0.25)
    return r.stdout


def focused_pane():
    """The focused LEAF pane, derived from dump-layout (NOT list-clients, which
    lags badly under load — it kept reporting a stale pane). Returns
    'sidebar.wasm' for a focused plugin pane, or 'terminal …' for a focused
    command/shell pane; '' if none is found."""
    block = focused_tab_block()
    lines = block.splitlines()
    for i, line in enumerate(lines):
        s = line.lstrip()
        if not s.startswith("pane") or "focus=true" not in s:
            continue
        # A plugin pane is `pane … focus=true {` whose VERY NEXT line is the
        # `plugin location=…` child. A terminal pane is self-closing (no `{`,
        # e.g. `pane cwd=… focus=true size=…`) or a command pane (`{` then
        # args). Only the immediate child line distinguishes them — looking
        # further ahead would grab the NEXT sibling pane's plugin line.
        if s.rstrip().endswith("{") and i + 1 < len(lines):
            nxt = lines[i + 1].strip()
            if nxt.startswith("plugin location="):
                return nxt
        return f"terminal {s}"
    return ""


def tabs():
    return [t for t in act("query-tab-names").strip().splitlines() if t]


def dump():
    # A healthy session ALWAYS has exactly one focus=true tab; under load
    # dump-layout intermittently returns a focus-less (or even wrong-command)
    # snapshot, which made the focused-tab/pane queries spuriously empty. Retry
    # until the snapshot is coherent.
    for _ in range(8):
        d = act("dump-layout")
        if "focus=true" in d:
            return d
        time.sleep(0.3)
    return d


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


def center_terminals(block):
    """Count center terminal panes in a focused-tab block — `pane` lines
    carrying a `cwd=`/`command=` (the center column's shells). The chrome bars
    are borderless plugin panes with no cwd, so they don't count."""
    return sum(1 for l in block.splitlines()
               if l.lstrip().startswith("pane")
               and ("cwd=" in l or "command=" in l))


def focused_tab_name():
    """The name of the focused tab (`""` if no tab is currently focused)."""
    b = focused_tab_block()
    first = b.splitlines()[0].lstrip() if b else ""
    parts = first.split('name="', 1)
    return parts[1].split('"', 1)[0] if len(parts) == 2 else ""


# ── setup ────────────────────────────────────────────────────────────────
print("== setup ==")
missing = [n for n, p in PLUGINS.items()
           if not os.path.exists(os.path.join(ROOT, p))]
if not (os.path.exists(SZ) and shutil.which("zellij") and not missing):
    print(f"SKIP: need release superzej + plugins + zellij "
          f"(missing plugins: {missing or 'none'})")
    sys.exit(0)

# Fully isolated sandbox HOME — its own socket, cache, state, data + layout dirs,
# plus this worktree's freshly built binary on PATH. This harness can NEVER see
# or disturb a real (system or live-superzej) session. (Mirrors the sandbox in
# resource-monitor.py — the proven pattern for driving a real chrome session.)
SBX = tempfile.mkdtemp(prefix="sz-navux-")
HOME = SBX
DATA = os.path.join(SBX, ".local/share/superzej")
LAYOUTDIR = os.path.join(SBX, ".config/zellij/layouts")
CFGDIR = os.path.join(SBX, ".config/superzej")
CACHE = os.path.join(SBX, ".superzej/cache")
STATE = os.path.join(SBX, "state")
RUN = os.path.join(SBX, "run")
for d in (DATA, LAYOUTDIR, CFGDIR, CACHE, STATE, RUN):
    os.makedirs(d, exist_ok=True)
# Point the sandbox plugin dir + config layout dir at THIS worktree's fresh
# artifacts. Without the layouts, the initial `default_layout "superzej"` and
# the `worktree-tab` layout that `superzej new-worktree` spawns can't resolve —
# zellij falls back to a bare, chrome-less tab (the old harness's failure mode).
for name, rel in PLUGINS.items():
    os.symlink(os.path.join(ROOT, rel), os.path.join(DATA, f"{name}.wasm"))
for lay in ("superzej", "home-tab", "worktree-tab", "worktree-tab-extra",
            "worktree-tab-restore"):
    src = os.path.join(ROOT, "layouts", f"{lay}.kdl")
    if os.path.exists(src):
        os.symlink(src, os.path.join(LAYOUTDIR, f"{lay}.kdl"))

# Worktrees land under the sandbox (never the live ~/.superzej/worktrees).
# picker = "select": the worktree center pane runs `pick-agent`, which without a
# preset shows the agent picker. The interactive gum/fzf TUIs repaint on every
# layout reflow — and the swap/toggle tests trigger a storm of reflows, which
# pegged the headless zellij server at >1000% CPU and corrupted the run. The
# "select" backend is a plain numbered prompt that just blocks on stdin (still a
# live terminal pane, which is all the test asserts) — no repaint loop.
with open(os.path.join(CFGDIR, "config.toml"), "w") as f:
    f.write(f'worktrees_dir = "{SBX}/wt"\n'
            'picker = "select"\n')
# Append pins configuration for E2E tests
with open(os.path.join(CFGDIR, "config.toml"), "a") as f:
    f.write('\n[[pins]]\n'
            'name = "tab-pin"\n'
            'command = "echo tab-pin-ready; exec sh"\n'
            'location = "tab"\n\n'
            '[[pins]]\n'
            'name = "layout-pin"\n'
            'command = "echo layout-pin-ready; exec sh"\n'
            'location = "layout"\n')

# This harness may itself run inside a live zellij/superzej — strip the inherited
# This harness may itself run inside a live zellij/superzej — strip the inherited
# ZELLIJ_* vars so the sandbox session never nests into or leaks to it.
_base = {k: v for k, v in os.environ.items()
         if k not in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID")}
# Env for the forked zellij that hosts the session (picks its name via
# --session, so it must NOT carry ZELLIJ_SESSION_NAME).
CHILD_ENV = dict(
    _base,
    HOME=HOME,
    XDG_CACHE_HOME=CACHE,
    XDG_STATE_HOME=STATE,
    XDG_CONFIG_HOME=os.path.join(SBX, ".config"),
    ZELLIJ_SOCKET_DIR=RUN,
    # superzej-spawned tabs resolve named layouts via --layout-dir = this dir;
    # point it at this worktree's source so Alt+w / Alt+t tabs come back with
    # the real (chrome-bearing) worktree-tab layout.
    SUPERZEJ_LAYOUT_DIR=os.path.join(ROOT, "layouts"),
    PATH=os.path.join(ROOT, "target", "release") + os.pathsep + os.environ["PATH"],
)
# Env for the `zellij action`/`pipe` clients + the `superzej` calls — these
# target the session by name.
ENV = dict(CHILD_ENV, ZELLIJ_SESSION_NAME=SESSION)

# Pre-grant plugin permissions (a prompt is un-approvable in a fixed pane).
subprocess.run([SZ, "grant-plugins"], env=ENV, capture_output=True)

# A throwaway git repo to root the session (the home tab resolves its cwd).
# Unique repo name per run: the worktree DEST dir ({worktrees_dir}/{slug}) is
# keyed by repo basename, so a reused name could collide with a stale worktree.
repo = os.path.join(SBX, f"navux-{os.getpid()}")
os.makedirs(repo)
subprocess.run(["git", "-C", repo, "init", "-q"], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.email=t@e", "-c",
                "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"],
               check=True)

pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.environ.clear()
    os.environ.update(CHILD_ENV)
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


def settle(cond, timeout=10.0, interval=0.3):
    """Poll until cond() is truthy (draining the pty each tick), then return
    its final value. Zellij applies focus moves, tab spawns and swap-layout
    reflows asynchronously, so a fixed sleep races them under load — polling
    the asserted end-state is what makes this harness deterministic (the same
    pattern resource-monitor.py uses)."""
    deadline = time.time() + timeout
    val = cond()
    while not val and time.time() < deadline:
        time.sleep(interval)
        drain()
        val = cond()
    return val


def chrome_settled(want=4):
    """The focused tab's block, re-read until all `want` chrome plugins are
    present (or timeout). A swap-layout or NewPane reflow can momentarily drop
    a plugin from dump-layout, so poll before asserting on it."""
    box = {"b": ""}

    def ready():
        box["b"] = focused_tab_block()
        return len(chrome_plugins(box["b"])) == want
    settle(ready)
    return box["b"]


def cleanup():
    subprocess.run(["zellij", "delete-session", SESSION, "--force"],
                   capture_output=True, env=ENV)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    # delete-session does NOT reliably kill the zellij SERVER daemon — and with
    # 5+ tabs of plugins it can spin at >1000% CPU, so a survivor poisons every
    # later run. SIGKILL anything still referencing this sandbox's socket dir
    # (server + any client); RUN is unique to this run, so this can't touch a
    # real session.
    subprocess.run(["pkill", "-9", "-f", RUN], capture_output=True)
    # Everything (worktrees included) lives under SBX — one rmtree clears it all.
    shutil.rmtree(SBX, ignore_errors=True)


# Turn SIGTERM (what a `timeout` wrapper sends) into a clean exit so the finally
# below still tears the sandbox session down — otherwise a killed run orphans a
# zellij server that keeps polling and piles load onto the next run.
signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))


try:
    time.sleep(4)
    drain()
    check(len(tabs()) == 1, f"session up, home tab ({tabs()})")

    # ── 1. Alt+w: worktree tab, center terminal focused ─────────────────
    print("== Alt+w: new worktree ==")
    key(b"\x1bw", wait=6)
    # new-worktree does git work + the agent picker, which can run well past a
    # fixed wait under load — poll for the second tab rather than reading a
    # stale single-tab list (a premature read cascaded into wrong-tab spawns).
    check(settle(lambda: len(tabs()) == 2, timeout=25.0),
          f"worktree tab created ({tabs()})")
    t = tabs()
    wt_tab = next((x for x in t if not x.endswith("/home")), t[-1])
    check("/" in wt_tab and not wt_tab.endswith("/home"),
          f"worktree tab is a real worktree ({wt_tab})")
    # And it must be the FOCUSED tab — everything downstream drives it.
    check(settle(lambda: focused_tab_name() == wt_tab),
          f"worktree tab focused after create ({focused_tab_name()!r})")
    check(settle(lambda: "terminal" in focused_pane()),
          f"center terminal focused after create ({focused_pane()})")

    # ── 2. Alt+h/l focus nav, no tab spill ───────────────────────────────
    print("== Alt+h/j/k/l focus nav ==")
    key(b"\x1bh")
    check(settle(lambda: "sidebar.wasm" in focused_pane()), "Alt+h -> sidebar")
    before = tabs()
    key(b"\x1bh")
    check("sidebar.wasm" in focused_pane() and tabs() == before,
          "Alt+h at the left edge stays put (no tab switch)")
    key(b"\x1bl")
    check(settle(lambda: "terminal" in focused_pane()), "Alt+l -> center terminal")
    key(b"\x1bl")
    check(settle(lambda: "panel.wasm" in focused_pane()), "Alt+l -> diff/PR panel")
    key(b"\x1bl")
    check("panel.wasm" in focused_pane(),
          "Alt+l at the right edge stays put (no tab switch)")
    key(b"\x1bh")
    check(settle(lambda: "terminal" in focused_pane()),
          "Alt+h -> back to center terminal")

    # ── 3. Alt+t: second tab on the same worktree ────────────────────────
    print("== Alt+t: same-worktree tabs ==")
    key(b"\x1bt", wait=4)
    check(settle(lambda: f"{wt_tab} ·2" in tabs()),
          f"tab '{wt_tab} ·2' created ({tabs()})")
    block = chrome_settled()
    check(block.lstrip().startswith(f'tab name="{wt_tab} ·2"'),
          "the ·2 tab is focused")
    check(len(chrome_plugins(block)) == 4,
          f"full chrome present ({chrome_plugins(block)})")
    check(settle(lambda: "terminal" in focused_pane()),
          f"plain shell focused in center ({focused_pane()})")

    key(b"\x1bt", wait=4)
    check(settle(lambda: f"{wt_tab} ·3" in tabs()), "second Alt+t -> ·3")

    # ── 4. tab-mode `n` repointed ────────────────────────────────────────
    print("== tab-mode n ==")
    key(b"\x14", wait=0.5)   # Ctrl+t -> tab mode
    key(b"n", wait=4)
    settle(lambda: f"{wt_tab} ·4" in tabs())
    t = tabs()
    check(f"{wt_tab} ·4" in t and not any(x.startswith("Tab #") for x in t),
          f"tab-mode n -> ·4, no bare 'Tab #N' ({t})")

    # ── 5. sidebar tree: repo -> worktree -> tabs ────────────────────────
    print("== sidebar tree navigation ==")
    # The tab-mode-n spawn is still settling; let the tab list + the sidebar's
    # TabUpdate-driven tree quiesce before driving it (queries transiently
    # return empty mid-spawn).
    settle(lambda: len(tabs()) == 5)
    home_tab = tabs()[0]

    def focus_sidebar():
        """Land focus on the sidebar plugin. Alt+h is MoveFocus Left; from any
        center/panel pane a few presses walk to the leftmost (sidebar) column.
        Poll between presses because a freshly spawned tab's plugins load async
        and a move can race the load."""
        for _ in range(4):
            key(b"\x1bh")
            if settle(lambda: "sidebar.wasm" in focused_pane(), timeout=3.0):
                return True
        return False

    def select_row(n, want, label):
        """Focus the sidebar, drive its tree cursor to row `n`, Enter, and
        assert the resulting focused tab is `want`.

        Each Enter is a TAB SWITCH, so the next visit lands on a different
        tab's sidebar instance — and the cursor is PERSISTENT per-instance
        plugin state. So every attempt first clamps the cursor to row 0 with a
        burst of `k` (move_cursor saturates at 0), then descends exactly `n`
        rows with `j`. Under load a stray keystroke or query can be dropped, so
        the whole idempotent sequence is retried until the asserted tab is the
        one focused (or attempts run out)."""
        for _ in range(6):
            if not focus_sidebar():
                continue
            # A freshly focused sidebar instance drops the first keystroke or
            # two while it finishes loading, which made `j`-counts systematically
            # undershoot. Send an ignored primer first (the plugin's on_key
            # discards unmatched bare keys), then drive the cursor at an unhurried
            # cadence so every move actually lands.
            key(b"x", wait=0.25)
            for _ in range(16):   # clamp the cursor to the top row
                key(b"k", wait=0.18)
            for _ in range(n):    # descend to the target row
                key(b"j", wait=0.25)
            key(b"\r", wait=1.0)  # Enter → tab switch
            if settle(lambda: focused_tab_name() == want, timeout=4.0):
                ok(label)
                return
        bad(f"{label} ({focused_tab_name()!r})")

    # rows: 0 repo · 1 home · 2 worktree · 3-6 pages ·1-·4 · 7 +worktree.
    # The first call doubles as the "sidebar focusable" assertion.
    check(focus_sidebar(), "sidebar focused for tree nav")
    select_row(1, home_tab, "home row -> home tab")
    select_row(2, wt_tab, "worktree row -> base tab")
    select_row(4, f"{wt_tab} ·2", "page ·2 row -> its tab")

    # ── 6. resolve-worktree strips the page suffix ───────────────────────
    print("== resolve-worktree ==")
    # ENV carries the sandbox socket dir + state, so the binary's own zellij
    # query hits the sandbox session (never a live one).
    r = subprocess.run(
        [SZ, "resolve-worktree", "--session", SESSION, "--tab",
         f"{wt_tab} ·2"],
        env=ENV, capture_output=True, text=True)
    resolved = r.stdout.strip()
    check(resolved and os.path.isdir(resolved),
          f"'{wt_tab} ·2' resolves to the worktree ({resolved or 'NOTHING'})")

    def goto_tab(name):
        """Focus the tab named `name` BY INDEX — go-to-tab-name would prefix-
        match, so "{base}" could land on "{base} ·2". Index is exact."""
        t = tabs()
        if name in t:
            act("go-to-tab", str(t.index(name) + 1))
        return settle(lambda: focused_tab_name() == name, timeout=6.0)

    def focus_center_terminal():
        """Land focus on a center terminal (between the sidebar and panel
        columns): walk left to the sidebar, then one MoveFocus Right into the
        center. Used before close-pane / Alt+N so they act on a TERMINAL —
        closing or splitting relative to a chrome plugin mangles the layout.
        Retried as a unit: the final Right can be dropped under load, leaving
        focus on the sidebar, so keep re-stepping until a terminal is focused."""
        for _ in range(6):
            if settle(lambda: "terminal" in focused_pane(), timeout=1.0):
                return True
            key(b"\x1bh")  # toward the sidebar (leftmost column)
            settle(lambda: "sidebar.wasm" in focused_pane(), timeout=2.0)
            key(b"\x1bl", wait=0.6)  # step right into the center
        return settle(lambda: "terminal" in focused_pane(), timeout=2.0)

    # Tear down the extra pages now that the tree + resolve checks are done.
    # Each open tab keeps four plugins re-rendering, and the session server's
    # CPU climbs with every open tab and layout op (it doesn't drain back). The
    # swap/toggle/panel tests below only need the base worktree tab, so dropping
    # to home + base keeps that growth well under the threshold where it spirals
    # into a runaway that corrupts the run under load.
    for extra in (f"{wt_tab} ·4", f"{wt_tab} ·3", f"{wt_tab} ·2"):
        if extra in tabs():
            goto_tab(extra)
            act("close-tab")
            settle(lambda e=extra: e not in tabs(), timeout=5.0)

    # ── 6. swap layouts on the center column ─────────────────────────────
    print("== Alt+] swap layouts ==")
    goto_tab(wt_tab)
    focus_center_terminal()
    # Split a SECOND center terminal (the swap variants need ≥2 to match) and
    # verify it landed — Alt+n is a single chord that can drop under load, and a
    # missing split cascades (close-pane later closes the lone terminal). Re-fire
    # until two center terminals exist.
    for _ in range(4):
        key(b"\x1bn", wait=2)        # Alt+n: NewPane Down
        if settle(lambda: center_terminals(chrome_settled()) >= 2, timeout=5.0):
            break
    base_block = chrome_settled()
    check(len(chrome_plugins(base_block)) == 4,
          "chrome intact after Alt+n split")

    # Decouple the two facts under test: first let the center RE-ARRANGE (the
    # block differs from before), then let chrome RE-SETTLE to all four plugins
    # — folding both into one poll let a slow reflow masquerade as missing
    # chrome. chrome is pinned, so chrome_settled always reaches 4 once quiesced.
    key(b"\x1b]", wait=1.5)          # next swap layout
    settle(lambda: focused_tab_block() != base_block, timeout=6.0)
    swapped = chrome_settled()
    check(len(chrome_plugins(swapped)) == 4, "chrome intact after Alt+]")
    changed = swapped != base_block
    key(b"\x1b]", wait=1.5)
    settle(lambda: focused_tab_block() != swapped, timeout=6.0)
    swapped2 = chrome_settled()
    check(changed or swapped2 != swapped,
          "Alt+] cycles center arrangements (layout changed)")
    check(len(chrome_plugins(swapped2)) == 4,
          "chrome intact after second Alt+]")

    # ── 7. sidebar toggle regression (next_swap_layout restore) ──────────
    print("== Ctrl+Alt+s toggle regression ==")

    def sidebar_shown():
        return "sidebar.wasm" in focused_tab_block()

    def toggle_sidebar(want_shown):
        """Ctrl+Alt+s pipes `superzej_toggle_sidebar` to the statusbar (one
        MessagePlugin, NOT a CLI pipe — a CLI pipe to the toggle would block on
        every per-tab instance and can double-fire). The chord itself can be
        dropped under load, so poll for the target visibility and re-fire only
        if the whole window elapses without the flip — a delivered chord
        reflects well within it, so a successful toggle is never undone."""
        for _ in range(2):
            key(b"\x1b\x13", wait=1.0)   # Ctrl+Alt+s
            # A long window so a delivered-but-slow toggle is observed rather
            # than mistaken for a drop — re-firing a successful toggle would
            # undo it (oscillation).
            if settle(lambda: sidebar_shown() == want_shown, timeout=8.0):
                return True
        return False

    check(toggle_sidebar(False), "sidebar hidden with 2 center terminals")
    check(toggle_sidebar(True), "sidebar restored with 2 center terminals")
    check(len(chrome_plugins(chrome_settled())) == 4,
          "full chrome after toggle restore")

    # close one center terminal -> back to one, toggle again. Focus a center
    # terminal first so close-pane removes a TERMINAL, not a chrome plugin
    # (closing a plugin collapses the tab into a bogus no-terminal layout); and
    # only close while two remain, then confirm exactly one is left — so a
    # dropped/raced close can't leave the tab terminal-less.
    for _ in range(4):
        if center_terminals(chrome_settled()) <= 1:
            break
        focus_center_terminal()
        act("close-pane")
        settle(lambda: center_terminals(chrome_settled()) == 1, timeout=5.0)
    settle(lambda: sidebar_shown() and "terminal" in focused_pane())
    check(toggle_sidebar(False), "sidebar hidden with 1 center terminal")
    restored = toggle_sidebar(True)
    check(restored and len(chrome_plugins(chrome_settled())) == 4,
          "sidebar restored to template with 1 center terminal")

    # ── 8. Alt+N: one in-place panel, no dead command pane ───────────────
    print("== Alt+N scoped panel ==")

    def pane_count(block):
        # leaf panes only — `split_direction` lines are layout containers,
        # and a split wraps siblings in a fresh container node
        return sum(1 for line in block.splitlines()
                   if line.lstrip().startswith("pane")
                   and "split_direction" not in line)

    # Anchor on the clean base worktree tab with focus on its center terminal:
    # Alt+N opens its panel relative to the focused pane, and firing it from a
    # chrome plugin (or a layout left mid-toggle) makes the Run pane open
    # floating and drops the sidebar. A known tab + terminal focus is the state
    # the real keybind is used from.
    goto_tab(wt_tab)
    focus_center_terminal()
    before_panes = pane_count(chrome_settled())
    # Alt+N opens a Run pane that becomes the panel (`new-panel --in-place`
    # renames itself to "panel" then execs a shell). Re-fire ONLY when the chord
    # was dropped (pane count unchanged) — never when a pane was added, so we
    # can't stack two panels. If a pane was added but not yet renamed (a rename
    # race under load), wait it out rather than re-firing.
    for _ in range(3):
        key(b"\x1bN", wait=3)
        if settle(lambda: 'name="panel"' in chrome_settled(), timeout=12.0):
            break
        if pane_count(chrome_settled()) > before_panes:
            settle(lambda: 'name="panel"' in chrome_settled(), timeout=8.0)
            break
    block = chrome_settled()
    check('name="panel"' in block, "panel pane created (renamed in place)")
    check("new-panel" not in block,
          "no leftover exited 'superzej new-panel' pane")
    check(pane_count(block) == before_panes + 1,
          f"exactly one pane added ({before_panes} -> {pane_count(block)})")

    # ── 9. Layout pins vs Tab pins ───────────────────────────────────────
    print("== 9. Layout pins vs Tab pins ==")
    
    # A. Tab-Pin
    # Tab pins are bound to Alt-1 for the first pin (tab-pin)
    goto_tab(wt_tab)
    key(b"\x1b1", wait=4)
    
    check(settle(lambda: "pin:tab-pin" in tabs(), timeout=10.0),
          f"tab-pin opens as a new tab named pin:tab-pin ({tabs()})")
    check(settle(lambda: focused_tab_name() == "pin:tab-pin", timeout=5.0),
          "tab-pin tab becomes active")
    
    # B. Layout-Pin
    # Second pin is bound to Alt-2 (layout-pin)
    goto_tab(wt_tab)
    focus_center_terminal()
    before_tabs = tabs()
    
    # We use CLI instead of Alt-2 because Alt-keybinds might be hijacked or dropped under load
    r = subprocess.run([SZ, "pin", "open", "2", "--session", SESSION], env=ENV, capture_output=True)
    
    check(settle(lambda: 'name="📌 layout-pin"' in chrome_settled(), timeout=10.0),
          "layout-pin pane is injected into active tab")
    check(focused_tab_name() == wt_tab,
          "active tab does not change after opening layout-pin")
    check(tabs() == before_tabs,
          "no pin:layout-pin tab was created")
          
    # Check that layout pane does not float
    block = chrome_settled()
    pin_pane_line = next((l for l in block.splitlines() if 'name="📌 layout-pin"' in l), "")
    check('floating=true' not in pin_pane_line,
          "layout pin is a standard tiled pane (not floating)")
          
    # C. Duplicate Panes
    r = subprocess.run([SZ, "pin", "open", "2", "--session", SESSION], env=ENV, capture_output=True)
    settle(lambda: sum(1 for l in chrome_settled().splitlines() if 'name="📌 layout-pin"' in l) == 2, timeout=10.0)
    check(sum(1 for l in chrome_settled().splitlines() if 'name="📌 layout-pin"' in l) == 2,
          "opening same layout-pin again spawns a duplicate pane")

finally:
    cleanup()

print()
if FAILED:
    print(f"FAIL ({len(FAILED)}):")
    for f in FAILED:
        print(f"  - {f}")
    sys.exit(1)
print("PASS")
