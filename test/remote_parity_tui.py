#!/usr/bin/env python3
"""Measure interactive key echo latency through the real thegn TUI.

The probe never touches the owner's thegn: it gives the TUI a throwaway
HOME, `XDG_CONFIG_HOME` and `THEGN_CONFIG_DIR` (a caller's already-isolated
state dir stays put, so the TUI still finds that run's daemon) and skips the
first-run wizard. It refuses to start when the config path still resolves to
the real user config, and it types the ready marker only once the pane shows
a shell prompt, so keystrokes cannot land in a settings form that persists.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import json
import os
import pty
import re
import shutil
import signal
import struct
import sys
import tempfile
import termios
import threading
import time
from collections.abc import Mapping
from pathlib import Path
from typing import Any


READY = b"RPTUIREADY"
WORD = b"zqxjkv"
READY_COMMAND = b"printf '%s%s\\n' RP TUIREADY\r"
KEY_INTERVAL = 0.250
AFTER_WORD_PAUSE = 0.500
READ_WAIT = 0.050

# A repaint writes rows with cursor moves. Turning those into line breaks
# before the other escapes are dropped keeps the row a prompt ends on whole.
CURSOR_MOVE = re.compile(r"\x1b\[[0-9;]*[Hf]")
ANSI_ESCAPE = re.compile(
    r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[()][A-Za-z0-9]|[=>78c])"
)
PROMPT_TAILS = ("$", "#", "%", ">", "❯", "λ")
PROMPT_SCAN_ROWS = 200
PROMPT_SCAN_BYTES = 65536
# thegn's status bar is a row that ends in a percentage. A prompt that ends in
# `%` has a space, not a digit, in front of it.
PERCENT_TAIL = re.compile(r"\d%$")


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


def shell_prompt(output: bytes) -> str | None:
    """The most recent shell-prompt-shaped row in a terminal byte stream.

    Only a row that ends in a prompt character counts, so a frame that has not
    drawn a prompt yet (a splash, a wizard, a picker, the status bar) yields
    nothing and the caller types no keystroke at all.
    """
    text = output.decode("utf-8", "replace")
    rows = ANSI_ESCAPE.sub("", CURSOR_MOVE.sub("\n", text)).splitlines()
    for row in reversed(rows[-PROMPT_SCAN_ROWS:]):
        line = row.strip()
        if not line or len(line) > 160 or PERCENT_TAIL.search(line):
            continue
        if line.endswith(PROMPT_TAILS):
            return line
    return None


def config_path_for(env: Mapping[str, str]) -> Path:
    """thegn's config file as this environment resolves it.

    Mirrors `util::thegn_config_dir`: a non-empty absolute `THEGN_CONFIG_DIR`
    wins, else the XDG config home, else `~/.config`.
    """
    override = env.get("THEGN_CONFIG_DIR") or ""
    if override and os.path.isabs(override):
        return Path(override) / "config.toml"
    config_home = env.get("XDG_CONFIG_HOME") or ""
    if not config_home:
        config_home = os.path.join(env.get("HOME") or "", ".config")
    return Path(config_home) / "thegn" / "config.toml"


def keep_throwaway(value: str, home: Path, fallback: Path) -> str:
    """A caller's state dir when it is already throwaway, else one under `root`.

    A run finds its own daemon through its state dir, so state a caller already
    isolated stays put. State under the owner's home is the owner's, not a
    run's, and driving it would attach the probe to real sessions.
    """
    if value:
        try:
            resolved = Path(value).resolve()
        except (OSError, RuntimeError):
            return str(fallback)
        if not resolved.is_relative_to(home):
            return value
    return str(fallback)


def isolated_environment(base: Mapping[str, str], root: Path) -> dict[str, str]:
    """A child environment that cannot reach the owner's thegn.

    Home and config always move under `root`. State only moves when the caller
    left it inside the owner's home: the TUI finds a running daemon through the
    control socket its state dir keys, so replacing a run's own state would
    detach the probe from that run instead of isolating anything.
    """
    for name in ("home", "state", "thegn"):
        (root / name).mkdir(mode=0o700, parents=True, exist_ok=True)
    (root / "config" / "thegn").mkdir(mode=0o700, parents=True, exist_ok=True)
    env = dict(base)
    home = Path(env.get("HOME") or "").resolve()
    env["HOME"] = str(root / "home")
    env["XDG_CONFIG_HOME"] = str(root / "config")
    env["THEGN_CONFIG_DIR"] = str(root / "config" / "thegn")
    # The first-run wizard and the keymap picker are modal: they swallow driven
    # keystrokes and write the ones still in a field into config.toml.
    env["THEGN_SKIP_ONBOARDING"] = "1"
    env["XDG_STATE_HOME"] = keep_throwaway(
        env.get("XDG_STATE_HOME") or "", home, root / "state"
    )
    env["THEGN_DIR"] = keep_throwaway(
        env.get("THEGN_DIR") or "", home, root / "thegn"
    )
    if env["XDG_STATE_HOME"] == str(root / "state"):
        # No daemon lives in a state dir this run just made, and starting one
        # would leave it behind: run panes in-process for this throwaway run.
        env.setdefault("THEGN_NO_DAEMON", "1")
    return env


def isolation_refusal(
    child: Mapping[str, str], root: Path, owner: Mapping[str, str]
) -> str | None:
    """Why the child could still write the owner's config, or None when it cannot.

    Two independent claims: the resolved path is not the owner's, and it sits
    inside the throwaway root this run just made.
    """
    child_config = config_path_for(child).resolve()
    owner_config = config_path_for(owner).resolve()
    if child_config == owner_config:
        return f"{child_config} is the real user config"
    if not child_config.is_relative_to(root.resolve()):
        return f"{child_config} is outside the throwaway root {root}"
    return None


class PtyProbe:
    """Own the forked PTY and retain enough timing data to find byte arrivals."""

    def __init__(
        self, executable: str, repo: str, self_test: bool, env: Mapping[str, str]
    ) -> None:
        self.executable = executable
        self.repo = repo
        self.self_test = self_test
        self.env = dict(env)
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
                    os.execve("/bin/sh", ["/bin/sh", "-i"], self.env)
                os.execve(self.executable, [self.executable], self.env)
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

    def wait_for_prompt(self, deadline: float) -> str | None:
        """Wait until the pane draws a shell prompt, so nothing is typed blind.

        The tail is enough: a prompt that has scrolled out of it is not the
        surface a keystroke would land on.
        """
        while True:
            with self._condition:
                found = shell_prompt(bytes(self.output[-PROMPT_SCAN_BYTES:]))
                if found is not None:
                    return found
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return None
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
    first_key_ms: list[int] = []
    next_key_ms: list[int] = []
    root: Path | None = None
    probe: PtyProbe | None = None
    try:
        root = Path(tempfile.mkdtemp(prefix="thegn-rp-tui-"))
        child_env = isolated_environment(os.environ, root)
        refusal = isolation_refusal(child_env, root, os.environ)
        if refusal is not None:
            return result(
                "FAIL",
                f"refusing to drive the TUI: {refusal}",
                first_key_ms,
                next_key_ms,
            )
        probe = PtyProbe(executable, repo, self_test, child_env)
        probe.start()
        readiness_deadline = time.monotonic() + timeout
        # The self-test drives a known shell, so nothing modal can be in front of
        # it; a real TUI may be showing a wizard that must not get the marker.
        if not self_test and probe.wait_for_prompt(readiness_deadline) is None:
            return result(
                "SKIP",
                "no shell prompt appeared within "
                f"{seconds_text(timeout)}s, so the ready marker was not typed "
                "(first-run screen or focus elsewhere)",
            )
        probe.write(READY_COMMAND)
        while not probe.wait_for(READY, min(readiness_deadline, time.monotonic() + 5.0)):
            if time.monotonic() >= readiness_deadline:
                return result(
                    "SKIP",
                    "the pane showed a shell prompt but no shell ran the ready "
                    f"command within {seconds_text(timeout)}s",
                )
            probe.write(READY_COMMAND)

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
        if probe is not None:
            probe.close()
        if root is not None:
            shutil.rmtree(root, ignore_errors=True)
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
