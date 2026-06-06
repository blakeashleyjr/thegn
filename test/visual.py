#!/usr/bin/env python3
"""Visual-regression harness for superzej's TUI (cell-grid snapshots).

For each flow in test/visual/manifest.toml we boot superzej inside a FULLY
sandboxed zellij (its own SUPERZEJ_DIR / XDG_STATE_HOME / ZELLIJ_SOCKET_DIR — so
a run never touches your daily session or DB, per the project's test-isolation
rule), drive a scripted key sequence, capture the terminal via
`zellij action dump-screen`, normalize it, and diff it against a committed golden
in test/visual/<name>.txt.

A flow passes at >= SIMILARITY cell agreement (the "~95%"). Determinism comes
from freezing the volatile widgets: SZ_FAKE_STATS / SZ_FAKE_TIME pin the tabbar,
a fixed terminal size, and the default theme.

    python3 test/visual.py            # check all flows against goldens
    python3 test/visual.py --update   # (re)capture goldens — review the diff!

This is the harness scaffold: the launch/key-drive steps are intentionally
small and explicit so they can be tuned on the target machine. When no goldens
exist yet it prints guidance and exits 0 (nothing to regress against), so the
pre-push hook doesn't block before you've captured a baseline.
"""

from __future__ import annotations

import os
import pty
import shutil
import subprocess
import sys
import time
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GOLDEN_DIR = ROOT / "test" / "visual"
MANIFEST = GOLDEN_DIR / "manifest.toml"
SIMILARITY = 0.95  # minimum cell agreement to pass
COLS, ROWS = 120, 40

# A deterministic terminal/session environment, isolated from the daily driver.
SESSION = "sz-visual"


def sandbox_env(tmp: Path) -> dict[str, str]:
    env = dict(os.environ)
    env.update(
        {
            "SUPERZEJ_DIR": str(tmp),
            "XDG_STATE_HOME": str(tmp / "state"),
            "XDG_CONFIG_HOME": str(tmp / "config"),
            "ZELLIJ_SOCKET_DIR": str(tmp / "run"),
            "SUPERZEJ_SESSION_NAME": SESSION,
            # Freeze the volatile tabbar widgets for stable snapshots.
            "SZ_FAKE_STATS": "cpu=12 mem=34 gpu=0 time=09:41",
            "SZ_FAKE_TIME": "09:41",
            "COLUMNS": str(COLS),
            "LINES": str(ROWS),
            "TERM": "xterm-256color",
            "NO_COLOR": "0",
        }
    )
    # Drop any inherited zellij context so we never attach to a live session.
    for k in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID"):
        env.pop(k, None)
    return env


def superzej_bin() -> Path:
    for c in (ROOT / "target/release/superzej", ROOT / "target/debug/superzej"):
        if c.exists():
            return c
    sys.exit("visual: build superzej first (cargo build [--release])")


def capture(env: dict[str, str], keys: list[str], tmp: Path) -> str:
    """Boot superzej, send `keys`, return the dumped screen as text.

    Keys are zellij chord strings (e.g. "Super k", "Ctrl Alt s"); they're sent
    with `zellij action write-chars` / key actions against the sandboxed socket,
    which is more robust than encoding raw bytes onto the pty.
    """
    sj = superzej_bin()
    # Launch the session detached on a pty so plugins (which need a client) load.
    master, slave = pty.openpty()
    os.set_blocking(master, False)
    proc = subprocess.Popen(
        [str(sj), "attach", SESSION],
        env=env,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        close_fds=True,
    )
    try:
        time.sleep(2.0)  # let the session + plugins come up
        for chord in keys:
            subprocess.run(
                ["zellij", "-s", SESSION, "action", "new-tab"] if False else
                ["zellij", "-s", SESSION, "action", "write", *_chord_bytes(chord)],
                env=env,
                check=False,
                capture_output=True,
            )
            time.sleep(0.4)
        dump = subprocess.run(
            ["zellij", "-s", SESSION, "action", "dump-screen", "/dev/stdout"],
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )
        return normalize(dump.stdout)
    finally:
        subprocess.run(
            ["zellij", "-s", SESSION, "kill-session", SESSION],
            env=env,
            check=False,
            capture_output=True,
        )
        proc.kill()
        os.close(master)
        os.close(slave)


def _chord_bytes(chord: str) -> list[str]:
    """Translate a zellij chord into `zellij action write` byte args. Only the
    handful used by the manifest are mapped; extend as flows grow."""
    # zellij `action write` takes decimal byte codes. This is deliberately tiny;
    # most flows reach their state via a single superzej keybind.
    raise NotImplementedError(
        "map manifest chords to `zellij action` calls on the target machine"
    )


def normalize(screen: str) -> str:
    # Strip trailing whitespace per line + trailing blank lines; the grid content
    # is what matters, not padding.
    lines = [ln.rstrip() for ln in screen.splitlines()]
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines) + "\n"


def similarity(a: str, b: str) -> float:
    al, bl = a.splitlines(), b.splitlines()
    n = max(len(al), len(bl)) or 1
    same = 0
    total = 0
    for i in range(n):
        ra = al[i] if i < len(al) else ""
        rb = bl[i] if i < len(bl) else ""
        w = max(len(ra), len(rb)) or 1
        total += w
        same += sum(1 for j in range(w) if (ra[j:j+1] == rb[j:j+1]))
    return same / total if total else 1.0


def load_flows() -> list[dict]:
    if not MANIFEST.exists():
        sys.exit(f"visual: no manifest at {MANIFEST}")
    return tomllib.loads(MANIFEST.read_text()).get("flow", [])


def main() -> int:
    update = "--update" in sys.argv[1:]
    flows = load_flows()
    GOLDEN_DIR.mkdir(parents=True, exist_ok=True)

    if not shutil.which("zellij"):
        print("visual: zellij not found; skipping (CI/dev-shell provides it)")
        return 0

    have_goldens = any((GOLDEN_DIR / f"{f['name']}.txt").exists() for f in flows)
    if not have_goldens and not update:
        print("visual: no goldens yet — run `just visual-update` to capture a")
        print("        baseline on this machine, review the diff, and commit.")
        return 0

    import tempfile

    failures = []
    for flow in flows:
        name = flow["name"]
        keys = flow.get("keys", [])
        golden = GOLDEN_DIR / f"{name}.txt"
        with tempfile.TemporaryDirectory(prefix="sz-visual-") as td:
            env = sandbox_env(Path(td))
            shot = capture(env, keys, Path(td))
        if update:
            golden.write_text(shot)
            print(f"  updated {name}.txt")
            continue
        if not golden.exists():
            failures.append(f"{name}: no golden (run --update)")
            continue
        sim = similarity(golden.read_text(), shot)
        mark = "ok" if sim >= SIMILARITY else "FAIL"
        print(f"  [{mark}] {name}  ({sim:.1%})")
        if sim < SIMILARITY:
            failures.append(f"{name}: {sim:.1%} < {SIMILARITY:.0%}")

    if failures:
        print("\nvisual regressions:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("visual: all flows within tolerance")
    return 0


if __name__ == "__main__":
    sys.exit(main())
