#!/usr/bin/env python3
"""Summarise the network-class syscall lines emitted by strace."""

from __future__ import annotations

import argparse
import ast
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
import re
import sys
from typing import Iterable


NETWORK_SYSCALLS = frozenset(
    {
        "socket",
        "socketpair",
        "bind",
        "listen",
        "accept",
        "accept4",
        "connect",
        "sendto",
        "sendmsg",
        "recvfrom",
        "recvmsg",
        "getsockname",
        "getpeername",
        "setsockopt",
        "getsockopt",
        "shutdown",
    }
)


@dataclass(frozen=True)
class Report:
    syscalls_seen: int
    connect_total: int
    connect_unix: int
    connect_other: int
    targets: tuple[tuple[str, str, int], ...]
    unix_targets: tuple[tuple[str, int], ...]
    verdict: str


def _strip_prefix(line: str) -> str:
    text = line.strip()
    text = re.sub(r"^\[pid\s+\d+\]\s+", "", text, count=1)
    text = re.sub(r"^\d+\s+", "", text, count=1)
    text = re.sub(r"^\d+\.\d+\s+", "", text, count=1)
    return text


def _sockaddr(text: str) -> tuple[str, str | None] | None:
    family_match = re.search(r"sa_family=([A-Z0-9_]+)", text)
    if family_match is None:
        return None

    family = family_match.group(1)
    opening = text.rfind("{", 0, family_match.start())
    if opening < 0:
        return family, None
    depth = 0
    closing = None
    for index in range(opening, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                closing = index
                break
    address_text = text[opening : closing + 1] if closing is not None else text[opening:]

    if family == "AF_UNIX":
        path_match = re.search(r"sun_path=(\@\"(?:[^\"\\]|\\.)*\"|\"(?:[^\"\\]|\\.)*\")", address_text)
        if path_match is None:
            return family, "<unnamed>"
        raw_path = path_match.group(1)
        if raw_path.startswith('@"'):
            return family, "@" + ast.literal_eval(raw_path[1:])
        return family, ast.literal_eval(raw_path)

    if family == "AF_INET":
        ip_match = re.search(r'sin_addr=inet_addr\("([^"]+)"\)', address_text)
        port_match = re.search(r"sin_port=htons\((\d+)\)", address_text)
        if ip_match is None:
            return family, "unknown"
        address = ip_match.group(1)
        if port_match is not None:
            address = f"{address}:{port_match.group(1)}"
        return family, address

    if family == "AF_INET6":
        ip_match = re.search(r'inet_pton\(AF_INET6,\s*"([^"]+)"', address_text)
        port_match = re.search(r"sin6_port=htons\((\d+)\)", address_text)
        if ip_match is None:
            return family, "unknown"
        address = f"[{ip_match.group(1)}]"
        if port_match is not None:
            address = f"{address}:{port_match.group(1)}"
        return family, address

    fields = []
    for field in ("nl_pid", "nl_groups", "sll_protocol", "sll_ifindex", "sll_addr"):
        field_match = re.search(rf"{field}=([^,}}]+)", address_text)
        if field_match is not None:
            fields.append(f"{field.removeprefix('sll_')}={field_match.group(1).strip()}")
    return family, ",".join(fields) if fields else "unknown"


def parse_line(line: str) -> tuple[str, str, str | None, str | None] | None:
    """Return syscall, socket family, target address and Unix path when present."""
    try:
        text = _strip_prefix(line)
        match = re.match(r"([a-z][a-z0-9_]*)\(", text)
        if match is None:
            return None
        syscall = match.group(1)
        if syscall not in NETWORK_SYSCALLS:
            return None
        address_text = text[match.end() :]
        if syscall == "sendmsg":
            msg_name = re.search(r"msg_name=(\{.*)", address_text)
            address_text = msg_name.group(1) if msg_name is not None else ""
        sockaddr = _sockaddr(address_text)
        if sockaddr is None:
            if ")" not in text and "<unfinished ...>" not in text:
                return None
            return syscall, "", None, None
        family, address = sockaddr
        if family == "AF_UNIX":
            return syscall, family, None, address
        return syscall, family, address, None
    except (ValueError, SyntaxError, IndexError):
        return None


def _is_loopback(family: str, address: str) -> bool:
    if family == "AF_INET":
        host = address.rsplit(":", 1)[0]
        return host.startswith("127.")
    if family == "AF_INET6":
        return address.split("]", 1)[0].lower() in ("[::1", "[0:0:0:0:0:0:0:1")
    return False


def _single_line(value: str) -> str:
    escaped = []
    for char in value:
        codepoint = ord(char)
        if char == "\n":
            escaped.append(r"\n")
        elif char == "\r":
            escaped.append(r"\r")
        elif char == "\t":
            escaped.append(r"\t")
        elif codepoint < 32 or codepoint == 127:
            escaped.append(f"\\x{codepoint:02x}")
        else:
            escaped.append(char)
    return "".join(escaped)


def summarize(lines: Iterable[str]) -> Report:
    syscall_count = 0
    connect_total = 0
    connect_unix = 0
    connect_other = 0
    targets: Counter[tuple[str, str]] = Counter()
    unix_targets: Counter[str] = Counter()
    saw_content = False

    for line in lines:
        if line.strip():
            saw_content = True
        parsed = parse_line(line)
        if parsed is None:
            continue
        syscall, family, address, unix_path = parsed
        syscall_count += 1
        if syscall == "connect" and (address is not None or unix_path is not None):
            connect_total += 1
            if family == "AF_UNIX":
                connect_unix += 1
                unix_targets[unix_path or "<unknown>"] += 1
            else:
                connect_other += 1
                targets[(family, address or "unknown")] += 1
        elif syscall in ("sendto", "sendmsg") and address is not None and family in (
            "AF_INET",
            "AF_INET6",
        ):
            connect_other += 1
            targets[(family, address)] += 1

    if not saw_content:
        verdict = "no-trace"
    elif connect_other:
        verdict = "non-unix-connect"
    else:
        verdict = "clean"
    return Report(
        syscall_count,
        connect_total,
        connect_unix,
        connect_other,
        tuple((family, address, count) for (family, address), count in sorted(targets.items())),
        tuple(sorted(unix_targets.items())),
        verdict,
    )


def render(report: Report, binary: str, window: str) -> str:
    lines = [
        "net-trace.version: 1",
        f"net-trace.binary: {_single_line(binary)}",
        f"net-trace.window: {_single_line(window)}",
        f"net-trace.syscalls_seen: {report.syscalls_seen}",
        f"net-trace.connect_total: {report.connect_total}",
        f"net-trace.connect_unix: {report.connect_unix}",
        f"net-trace.connect_other: {report.connect_other}",
    ]
    for index, (family, address, count) in enumerate(report.targets, start=1):
        suffix = " (loopback)" if _is_loopback(family, address) else ""
        lines.append(f"net-trace.target.{index}: {family} {_single_line(address)}{suffix} x{count}")
    for index, (path, count) in enumerate(report.unix_targets, start=1):
        lines.append(f"net-trace.unix.{index}: {_single_line(path)} x{count}")
    lines.append(f"net-trace.verdict: {report.verdict}")
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    summarize_parser = subparsers.add_parser("summarize")
    summarize_parser.add_argument("trace")
    summarize_parser.add_argument("--binary", default="unknown")
    summarize_parser.add_argument("--window", default="first-frame")
    summarize_parser.add_argument("--out")
    args = parser.parse_args(argv)

    trace_path = Path(args.trace)
    try:
        with trace_path.open(encoding="utf-8", errors="replace") as trace_file:
            report = summarize(trace_file)
    except OSError:
        report = summarize(())
    rendered = render(report, args.binary, args.window)
    if args.out:
        try:
            Path(args.out).write_text(rendered, encoding="utf-8")
        except OSError as error:
            print(f"net-trace: cannot write report: {error}", file=sys.stderr)
            return 2
    print(rendered, end="")
    if report.verdict == "no-trace":
        return 2
    return 1 if report.connect_other else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
