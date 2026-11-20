#!/usr/bin/env python3
"""Measure interactive key echo latency through the real thegn TUI."""

from __future__ import annotations

import argparse
import errno
import fcntl
import json
import os
import pty
import signal
import struct
import sys
import termios
import threading
import time
from typing import Any


READY = b"RPTUIREADY"
WORD = b"zqxjkv"
READY_COMMAND = b"printf '%s%s\\n' RP TUIREADY\r"
KEY_INTERVAL = 0.250
AFTER_WORD_PAUSE = 0.500
READ_WAIT = 0.050


def result(
    status: str,
    note: str,
    first_key_ms: list[int] | None = None,
    next_key_ms: list[int] | None = None,
) -> dict[str, Any]:
    first = first_key_ms or []
    following = next_key_ms or []
    return {
        "status": status,
        "note": note,
        "first": timing_summary(first),
        "next": timing_summary(following),
        "samples_file": os.environ.get("RP_TUI_SAMPLES_OUT") or None,
    }


def percentile(values: list[int], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    value = ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)
    return round(value, 3)


def timing_summary(values: list[int]) -> dict[str, int | float | None]:
    return {
        "n": len(values),
        "min": min(values) if values else None,
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "max": max(values) if values else None,
    }


def write_samples(first_key_ms: list[int], next_key_ms: list[int]) -> None:
    path = os.environ.get("RP_TUI_SAMPLES_OUT")
    if path:
        from pathlib import Path

        Path(path).write_text(
            json.dumps(
                {"first_key_ms": first_key_ms, "next_key_ms": next_key_ms},
                separators=(",", ":"),
            ),
            encoding="utf-8",
        )


def compact_shape(payload: dict[str, Any]) -> bool:
    return (
        list(payload) == ["status", "note", "first", "next", "samples_file"]
        and isinstance(payload.get("first"), dict)
        and isinstance(payload.get("next"), dict)
        and list(payload["first"]) == ["n", "min", "p50", "p95", "max"]
        and list(payload["next"]) == ["n", "min", "p50", "p95", "max"]
    )


def seconds_text(value: float) -> str:
    return str(int(value)) if value.is_integer() else str(value)


class PtyProbe:
    """Own the forked PTY and retain enough timing data to find byte arrivals."""

    def __init__(self, executable: str, repo: str, self_test: bool) -> None:
        self.executable = executable
        self.repo = repo
        self.self_test = self_test
        self.pid: int | None = None
        self.fd: int | None = None
        self.saved_attrs: list[Any] | None = None
        self.output = bytearray()
        self.read_events: list[tuple[float, int]] = []
        self._chunks: list[tuple[float, int, int]] = []
        self._condition = threading.Condition()
        self._reader: threading.Thread | None = None

    def start(self) -> None:
        pid, fd = pty.fork()
        if pid == pty.CHILD:
            try:
                os.chdir(self.repo)
                if self.self_test:
                    os.execv("/bin/sh", ["/bin/sh", "-i"])
                os.execv(self.executable, [self.executable])
            except OSError:
                os._exit(127)
            os._exit(127)

        self.pid = pid
        self.fd = fd
        try:
            self.saved_attrs = termios.tcgetattr(fd)
        except termios.error:
            self.saved_attrs = None
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self._reader = threading.Thread(target=self._read, name="remote-parity-tui-reader", daemon=True)
        self._reader.start()

    def _read(self) -> None:
        assert self.fd is not None
        while True:
            try:
                chunk = os.read(self.fd, 65536)
            except OSError as error:
                if error.errno in (errno.EIO, errno.EBADF):
                    return
                return
            if not chunk:
                return
            now = time.monotonic()
            with self._condition:
                start = len(self.output)
                self.output.extend(chunk)
                self.read_events.append((now, len(chunk)))
                self._chunks.append((now, start, start + len(chunk)))
                self._condition.notify_all()

    def write(self, data: bytes) -> tuple[int, float]:
        assert self.fd is not None
        started = time.monotonic()
        offset = self.output_length()
        view = memoryview(data)
        while view:
            written = os.write(self.fd, view)
            view = view[written:]
        return offset, started

    def output_length(self) -> int:
        with self._condition:
            return len(self.output)

    def wait_for_byte(
        self, value: int, offset: int, started: float, deadline: float
    ) -> float | None:
        while True:
            with self._condition:
                found = self.output.find(bytes((value,)), offset)
                while found >= 0:
                    for timestamp, start, end in self._chunks:
                        if start <= found < end:
                            if timestamp >= started:
                                return timestamp
                            break
                    found = self.output.find(bytes((value,)), found + 1)
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return None
                self._condition.wait(min(READ_WAIT, remaining))

    def wait_for(self, needle: bytes, deadline: float) -> bool:
        while True:
            with self._condition:
                if needle in self.output:
                    return True
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                self._condition.wait(min(READ_WAIT, remaining))

    def _wait_for_child(self, deadline: float) -> bool:
        assert self.pid is not None
        while time.monotonic() < deadline:
            waited, _ = os.waitpid(self.pid, os.WNOHANG)
            if waited == self.pid:
                self.pid = None
                return True
            time.sleep(0.050)
        return False

    def close(self) -> None:
        if self.pid is not None:
            try:
                os.kill(self.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            if not self._wait_for_child(time.monotonic() + 5.0):
                try:
                    os.kill(self.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    os.waitpid(self.pid, 0)
                except ChildProcessError:
                    pass
                self.pid = None

        if self.fd is not None:
            if self.saved_attrs is not None:
                try:
                    termios.tcsetattr(self.fd, termios.TCSANOW, self.saved_attrs)
                except termios.error:
                    pass
            try:
                os.close(self.fd)
            except OSError:
                pass
            self.fd = None
        if self._reader is not None:
            self._reader.join(1.0)


def run_probe(
    executable: str,
    repo: str,
    samples: int,
    timeout: float,
    self_test: bool,
) -> dict[str, Any]:
    probe = PtyProbe(executable, repo, self_test)
    first_key_ms: list[int] = []
    next_key_ms: list[int] = []
    try:
        probe.start()
        readiness_deadline = time.monotonic() + timeout
        while True:
            if probe.wait_for(READY, time.monotonic()):
                break
            now = time.monotonic()
            if now >= readiness_deadline:
                return result(
                    "SKIP",
                    "the TUI did not run a command in a shell pane within "
                    f"{seconds_text(timeout)}s (first-run screen or focus elsewhere)",
                )
            probe.write(READY_COMMAND)
            wait_until = min(readiness_deadline, time.monotonic() + 5.0)
            if not probe.wait_for(READY, wait_until):
                continue

        for _ in range(samples):
            for index, value in enumerate(WORD):
                offset, started = probe.write(bytes((value,)))
                found = probe.wait_for_byte(
                    value, offset, started, time.monotonic() + min(timeout, 10.0)
                )
                if found is None:
                    return result(
                        "FAIL",
                        "the TUI did not echo probe input within the configured timeout",
                        first_key_ms,
                        next_key_ms,
                    )
                elapsed = max(0, round((found - started) * 1000))
                if index == 0:
                    first_key_ms.append(elapsed)
                else:
                    next_key_ms.append(elapsed)
                remaining = KEY_INTERVAL - (time.monotonic() - started)
                if remaining > 0:
                    time.sleep(remaining)
            probe.write(b"\x15")
            time.sleep(AFTER_WORD_PAUSE)

        output = result("PASS", "interactive TUI key latency measured", first_key_ms, next_key_ms)
        if self_test and max(first_key_ms + next_key_ms, default=51) >= 50:
            return result(
                "FAIL",
                "the self-test did not produce sub-50ms key timings",
                first_key_ms,
                next_key_ms,
            )
        return output
    except (OSError, ValueError, termios.error):
        return result("FAIL", "the TUI parity probe failed", first_key_ms, next_key_ms)
    finally:
        probe.close()
        try:
            write_samples(first_key_ms, next_key_ms)
        except OSError as error:
            print(f"remote_parity_tui: samples file not written: {error}", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", default=None, help="thegn executable")
    parser.add_argument("--repo", default=None, help="repository used as the child cwd")
    parser.add_argument("--samples", type=int, default=12)
    parser.add_argument("--timeout", type=float, default=90.0)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)

    if not args.self_test and (args.bin is None or args.repo is None):
        print(
            "remote_parity_tui: plan only; pass --bin BIN --repo REPO to drive the TUI, "
            "or --self-test",
            flush=True,
        )
        return 0
    if args.samples <= 0 or args.timeout <= 0:
        payload = result("FAIL", "samples and timeout must be positive")
    else:
        payload = run_probe(
            args.bin or "/bin/sh",
            args.repo or ".",
            args.samples,
            args.timeout,
            args.self_test,
        )
    if args.self_test and not compact_shape(payload):
        print("remote_parity_tui: self-test produced a non-compact result", file=sys.stderr)
        return 1
    print(json.dumps(payload, separators=(",", ":")), flush=True)
    return 0 if payload["status"] in ("PASS", "SKIP") else 1


if __name__ == "__main__":
    raise SystemExit(main())
