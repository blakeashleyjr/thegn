#!/usr/bin/env python3
"""Parse timestamped strace execve events into a bounded idle-window report."""

import json
import re
import sys
from collections import Counter
from pathlib import Path


EVENT = re.compile(r"^(?:\s*\d+\s+)?(?P<time>\d+\.\d+)\s+execve\(")


class TraceError(ValueError):
    pass


def c_string(source: str, start: int) -> tuple[str, int]:
    if start >= len(source) or source[start] != '"':
        raise TraceError("expected quoted string")
    result = []
    i = start + 1
    while i < len(source):
        char = source[i]
        i += 1
        if char == '"':
            return "".join(result), i
        if char != "\\":
            result.append(char)
            continue
        if i >= len(source):
            break
        escaped = source[i]
        i += 1
        simple = {"n": "\n", "r": "\r", "t": "\t", "b": "\b", "f": "\f", "v": "\v", "\\": "\\", '"': '"'}
        if escaped in simple:
            result.append(simple[escaped])
        elif escaped == "x":
            digits = source[i : i + 2]
            if len(digits) != 2 or not re.fullmatch(r"[0-9a-fA-F]{2}", digits):
                raise TraceError("malformed hex escape")
            result.append(chr(int(digits, 16)))
            i += 2
        elif escaped in "01234567":
            digits = escaped
            while i < len(source) and len(digits) < 3 and source[i] in "01234567":
                digits += source[i]
                i += 1
            result.append(chr(int(digits, 8)))
        else:
            # strace quotes unrecognized bytes as backslash plus byte.
            result.append(escaped)
    raise TraceError("unterminated quoted string")


def argv_from_line(line: str) -> list[str]:
    event = EVENT.match(line)
    if not event:
        raise TraceError("line is not a timestamped execve event")
    call = line[event.end() :]
    _, after_path = c_string(call, 0)
    args_start = call.find("[", after_path)
    if args_start < 0:
        raise TraceError("execve argv array is missing")
    values = []
    i = args_start + 1
    while i < len(call):
        while i < len(call) and (call[i].isspace() or call[i] == ","):
            i += 1
        if i < len(call) and call[i] == "]":
            break
        if call[i : i + 3] == "...":
            raise TraceError("truncated execve argv array")
        value, i = c_string(call, i)
        values.append(value)
    else:
        raise TraceError("unterminated execve argv array")
    if not values:
        raise TraceError("execve argv array is empty")
    if ") =" not in call[i + 1 :]:
        raise TraceError("execve result is missing")
    return values


def parse_traces(paths: list[Path], start: float, end: float) -> dict:
    counts = Counter()
    total_events = 0
    for path in paths:
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except OSError as error:
            raise TraceError(f"cannot read trace {path}: {error}") from error
        if not lines or not any(line.strip() for line in lines):
            raise TraceError(f"trace is empty: {path}")
        events = 0
        for line_number, line in enumerate(lines, 1):
            if not line.strip():
                continue
            match = EVENT.match(line)
            if not match:
                raise TraceError(f"{path} line {line_number} has no timestamped execve event")
            try:
                timestamp = float(match.group("time"))
                argv = argv_from_line(line)
            except TraceError as error:
                raise TraceError(f"{path} line {line_number}: {error}") from error
            events += 1
            if start <= timestamp <= end:
                command = Path(argv[0]).name or argv[0]
                label = " ".join([command, *argv[1:4]])
                counts[label] += 1
        if events == 0:
            raise TraceError(f"trace contains no execve events: {path}")
        total_events += events
    if total_events == 0:
        raise TraceError("trace contains no execve events")
    offenders = [
        {"command": command, "count": count}
        for command, count in sorted(counts.items(), key=lambda item: (-item[1], item[0]))
    ]
    return {"status": "measured", "method": "strace", "count": sum(counts.values()), "ceiling": 0, "offenders": offenders}


def self_test() -> None:
    import tempfile

    fixture = """100 10.000000 execve("/usr/bin/true", ["true"], 0x1 /* 1 vars */) = 0
100 20.000000 execve("/usr/bin/podman", ["podman", "ps", "--format", "json"], 0x1 /* 1 vars */) = 0
101 20.500000 execve("/bin/podman", ["podman", "ps", "--format", "json"], 0x1 /* 1 vars */) = 0
102 20.750000 execve("/bin/docker", ["docker", "ps"], 0x1 /* 1 vars */) = 0
103 31.000000 execve("/usr/bin/late", ["late"], 0x1 /* 1 vars */) = 0
"""
    with tempfile.TemporaryDirectory() as directory:
        trace = Path(directory) / "trace"
        daemon_trace = Path(directory) / "daemon-trace"
        trace.write_text(fixture, encoding="utf-8")
        daemon_trace.write_text(
            '201 20.600000 execve("/usr/bin/podman", ["podman", "ps", "--format", "json"], 0x1) = 0\n',
            encoding="utf-8",
        )
        result = parse_traces([trace, daemon_trace], 20.0, 30.0)
        assert result["count"] == 4
        assert result["offenders"] == [
            {"command": "podman ps --format json", "count": 3},
            {"command": "docker ps", "count": 1},
        ]
        trace.write_text("90 19.999999 execve(\"/bin/true\", [\"true\"], 0x1) = 0\n", encoding="utf-8")
        assert parse_traces([trace], 20.0, 30.0)["count"] == 0
        for malformed in ("", "not a strace event\n"):
            trace.write_text(malformed, encoding="utf-8")
            try:
                parse_traces([trace], 20.0, 30.0)
            except TraceError:
                pass
            else:
                raise AssertionError("malformed/empty trace was accepted")
    print("spawn-trace self-test: 6 parser cases passed")


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return 0
    if len(sys.argv) < 4:
        print("usage: spawn-trace.py START_EPOCH END_EPOCH TRACE [TRACE ...]", file=sys.stderr)
        return 1
    try:
        result = parse_traces([Path(path) for path in sys.argv[3:]], float(sys.argv[1]), float(sys.argv[2]))
    except (TraceError, ValueError) as error:
        print(f"spawn-rate trace invalid: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
