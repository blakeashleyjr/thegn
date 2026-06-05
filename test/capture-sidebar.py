#!/usr/bin/env python3
"""Render the live layout on a pty, reconstruct the screen with pyte, and print
the left (sidebar) columns so we can see exactly where the title/box land."""
import os, pty, signal, struct, subprocess, tempfile, termios, fcntl, time, shutil
import pyte

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CONFIG = os.path.join(ROOT, "config", "zellij.kdl")
SESSION = f"sz-cap-{os.getpid()}"
ROWS, COLS = 45, 170

tmphome = tempfile.mkdtemp()
repo = os.path.join(tmphome, f"cap-{os.getpid()}")
os.makedirs(repo)
subprocess.run(["git", "-C", repo, "init", "-q"], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.email=t@e", "-c", "user.name=t",
                "commit", "-q", "--allow-empty", "-m", "init"], check=True)

cache = os.path.join(tmphome, "cache", "zellij")
os.makedirs(cache)
shutil.copy(os.path.expanduser("~/.cache/zellij/permissions.kdl"),
            os.path.join(cache, "permissions.kdl"))

buf = bytearray()
pid, fd = pty.fork()
if pid == 0:
    os.chdir(repo)
    os.environ["XDG_STATE_HOME"] = os.path.join(tmphome, "state")
    os.environ["XDG_CACHE_HOME"] = os.path.join(tmphome, "cache")
    os.execvp("zellij", ["zellij", "--config", CONFIG, "--session", SESSION])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
os.set_blocking(fd, False)

def drain():
    try:
        while True:
            b = os.read(fd, 65536)
            if not b: break
            buf.extend(b)
    except (OSError, BlockingIOError):
        pass

try:
    time.sleep(6); drain()
    os.write(fd, b"\x1bh"); time.sleep(1.5); drain()   # focus sidebar (repaint)
    screen = pyte.Screen(COLS, ROWS)
    screen.report_device_status = lambda *a, **k: None  # pyte chokes on private DSR
    stream = pyte.ByteStream(screen)
    stream.feed(bytes(buf))
    raw = bytes(buf).decode("utf-8", errors="replace")
    print("RAW stream has SENTINELXYZ:", "SENTINELXYZ" in raw)
    print("RAW stream has '%' bottom-border marker count:", raw.count("%"))
    print("=== left 22 columns, ALL rows ===")
    for r in range(0, ROWS):
        line = screen.display[r][:22]
        if line.strip():
            print(f"{r:2} |{line}|")
finally:
    subprocess.run(["zellij", "delete-session", SESSION, "--force"], capture_output=True)
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass
    shutil.rmtree(tmphome, ignore_errors=True)
