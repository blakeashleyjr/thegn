#!/usr/bin/env python3
"""Quiet-session keystroke echo driver for test/perf/echo.sh.

Runs one child under a PTY, optionally sends a prelude (e.g. dismiss a
first-launch wizard, start `cat`), then types KEYS printable characters at a
fixed SPACING. For each keystroke it records the first PTY output chunk that
arrives at or after the write.

That is SUBMISSION latency: the multiplexer wrote output for the key to its
PTY. It is not paint latency, and it does not prove the application responded.
Chunks are coalesced by the kernel and by the child, so one chunk can answer
more than one key under load; at 20 ms spacing on a quiet session this is
rare, and the sample count is reported so a reader can judge.

Run with `python3 -I` (isolated mode: no user site, no PYTHON* env, no cwd on
sys.path). Writes one JSON object (or a text table with no --json) to stdout.
"""

import argparse
import bisect
import codecs
import fcntl
import json
import math
import os
import pty
import select
import signal
import string
import struct
import subprocess
import sys
import termios
import time

WINDOW_ROWS = 50
WINDOW_COLS = 200
MAX_LATENCY_S = 1.0  # a key with no output within this window is unanswered
TAIL_S = 1.0  # time to wait for trailing output after the last key


def percentile(sorted_vals, p):
    """Nearest-rank percentile over an already sorted list."""
    if not sorted_vals:
        return None
    rank = max(1, math.ceil(p / 100.0 * len(sorted_vals)))
    return sorted_vals[rank - 1]


def decode(s):
    # Prelude steps are written with shell-style escapes (\x1b, \r).
    return codecs.decode(s, "unicode_escape").encode("latin-1")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--child", required=True, help="sh command line run under the PTY")
    ap.add_argument("--keys", type=int, default=200)
    ap.add_argument("--spacing-ms", type=float, default=20.0)
    ap.add_argument("--settle-ms", type=float, default=3000.0)
    ap.add_argument(
        "--prelude",
        action="append",
        default=[],
        help="input written after settle, each followed by a 500 ms wait (escapes allowed)",
    )
    ap.add_argument("--tool", default="thegn")
    ap.add_argument("--build", default="unknown")
    ap.add_argument("--sha", default="unknown")
    ap.add_argument("--host", default="unknown")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    if args.keys < 1:
        sys.exit("echo_pty: --keys must be >= 1")

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", WINDOW_ROWS, WINDOW_COLS, 0, 0))
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    proc = subprocess.Popen(
        ["/bin/sh", "-c", args.child],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        start_new_session=True,
        close_fds=True,
    )
    os.close(slave)

    chunks = []  # monotonic arrival time of each output read

    def pump(until):
        """Read PTY output until `until`. Returns False if the child closed the PTY."""
        while True:
            remaining = until - time.monotonic()
            if remaining <= 0:
                return True
            ready, _, _ = select.select([master], [], [], remaining)
            if not ready:
                continue
            try:
                data = os.read(master, 65536)
            except OSError:  # EIO once the child side is gone
                return False
            if not data:
                return False
            chunks.append(time.monotonic())

    sent = []
    try:
        if not pump(time.monotonic() + args.settle_ms / 1000.0):
            sys.exit("echo_pty: child exited during settle")
        for step in args.prelude:
            os.write(master, decode(step))
            if not pump(time.monotonic() + 0.5):
                sys.exit("echo_pty: child exited during prelude")

        spacing = args.spacing_ms / 1000.0
        letters = string.ascii_lowercase
        for i in range(args.keys):
            t = time.monotonic()
            os.write(master, letters[i % len(letters)].encode())
            sent.append(t)
            if not pump(t + spacing):
                break
        pump(time.monotonic() + TAIL_S)
    finally:
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()
        os.close(master)

    latencies_ms = []
    unanswered = 0
    for t in sent:
        j = bisect.bisect_left(chunks, t)  # chunks are appended in time order
        if j < len(chunks) and chunks[j] - t <= MAX_LATENCY_S:
            latencies_ms.append((chunks[j] - t) * 1000.0)
        else:
            unanswered += 1
    latencies_ms.sort()

    def r(v):
        return None if v is None else round(v, 3)

    result = {
        "scenario": "echo",
        "tool": args.tool,
        "build": args.build,
        "git_sha": args.sha,
        "host_tag": args.host,
        "method": "first PTY output chunk after each keystroke; submission, not paint",
        "keys": args.keys,
        "spacing_ms": args.spacing_ms,
        "samples": len(latencies_ms),
        "unanswered": unanswered,
        "sufficient": len(latencies_ms) >= 200,
        "p50_ms": r(percentile(latencies_ms, 50)),
        "p95_ms": r(percentile(latencies_ms, 95)),
        "p99_ms": r(percentile(latencies_ms, 99)),
        "max_ms": r(latencies_ms[-1] if latencies_ms else None),
        "mean_ms": r(sum(latencies_ms) / len(latencies_ms) if latencies_ms else None),
    }
    if args.json:
        print(json.dumps(result, separators=(",", ":")))
    else:
        print(
            f"scenario=echo tool={args.tool} build={args.build} keys={args.keys} "
            f"spacing={args.spacing_ms}ms (sha={args.sha} host={args.host})"
        )
        print(f"  key→first output  p50={result['p50_ms']}ms  p95={result['p95_ms']}ms  "
              f"p99={result['p99_ms']}ms  max={result['max_ms']}ms")
        print(f"  samples={result['samples']} unanswered={result['unanswered']} "
              f"sufficient(>=200)={result['sufficient']}")
        print("  submission, not paint: the PTY got output, the screen was not measured")


if __name__ == "__main__":
    main()
