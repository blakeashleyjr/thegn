#!/usr/bin/env python3
"""Keep Cargo.lock duplicate crate names from growing without review.

The committed baseline pins how many versions each duplicated package name
has. New names and additional versions fail; stale entries fail too, so the
list only shrinks. Version strings are sorted lexically and are informative:
a same-count version swap does not change the duplicate count.

Run directly with `python3 -B test/dup_crates_guard.py`, or regenerate the
baseline with `python3 -B test/dup_crates_guard.py --update`.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "test" / "dup-crates-ratchet.txt"
PACKAGE = re.compile(r'^\s*name\s*=\s*("(?:[^"\\]|\\.)*")\s*$')
VERSION = re.compile(r'^\s*version\s*=\s*("(?:[^"\\]|\\.)*")\s*$')


def parse_lock(text: str) -> dict[str, set[str]]:
    """Read package name/version pairs from Cargo.lock package blocks."""
    packages: dict[str, set[str]] = {}
    in_package = False
    name: str | None = None
    version: str | None = None

    def save_package() -> None:
        if name is not None and version is not None:
            packages.setdefault(name, set()).add(version)

    for line in text.splitlines():
        if line.strip() == "[[package]]":
            save_package()
            in_package = True
            name = None
            version = None
            continue
        if not in_package:
            continue
        if name is None and (match := PACKAGE.match(line)):
            name = json.loads(match.group(1))
        elif version is None and (match := VERSION.match(line)):
            version = json.loads(match.group(1))

    save_package()
    return packages


def duplicates(pkgs: dict[str, set[str]]) -> dict[str, list[str]]:
    """Return duplicate names with lexically sorted version strings."""
    return {
        name: sorted(versions)
        for name, versions in sorted(pkgs.items())
        if len(versions) >= 2
    }


def load_allowlist(text: str) -> tuple[list[str], dict[str, int]]:
    """Return the verbatim leading comment header and pinned duplicate counts."""
    header: list[str] = []
    entries: dict[str, int] = {}
    in_header = True
    for number, line in enumerate(text.splitlines(keepends=True), start=1):
        stripped = line.strip()
        if in_header and (not stripped or stripped.startswith("#")):
            header.append(line)
            continue
        in_header = False
        if not stripped:
            continue
        fields = stripped.split()
        if len(fields) < 3:
            raise ValueError(f"line {number}: expected name, count, and versions")
        name = fields[0]
        try:
            count = int(fields[1])
        except ValueError as exc:
            raise ValueError(f"line {number}: invalid version count {fields[1]!r}") from exc
        if count < 2 or len(fields[2:]) != count:
            raise ValueError(f"line {number}: count does not match pinned versions")
        if name in entries:
            raise ValueError(f"line {number}: duplicate baseline entry for {name}")
        entries[name] = count
    return header, entries


def check(dups: dict[str, list[str]], allow: dict[str, int]) -> list[str]:
    """Report new duplicate names, growth, or stale baseline entries."""
    problems: list[str] = []
    for name, versions in sorted(dups.items()):
        pinned = allow.get(name)
        if pinned is None:
            problems.append(f"new duplicate: {name} {' '.join(versions)}")
        elif len(versions) > pinned:
            problems.append(f"more versions of {name}: {len(versions)} > {pinned}")
        elif len(versions) < pinned:
            problems.append(
                f"stale entry: {name} (now {len(versions)}); "
                "shrink the list with just ratchet-update"
            )
    for name, pinned in sorted(allow.items()):
        if name not in dups:
            problems.append(
                f"stale entry: {name} (now 1); shrink the list with just ratchet-update"
            )
    return problems


def render(header: list[str], dups: dict[str, list[str]]) -> str:
    """Keep the header verbatim and write one sorted entry per duplicate name."""
    prefix = "".join(header)
    if prefix and not prefix.endswith(("\n", "\r")):
        prefix += "\n"
    body = "".join(
        f"{name} {len(versions)} {' '.join(versions)}\n"
        for name, versions in sorted(dups.items())
    )
    return prefix + body


def self_test() -> int:
    """Exercise the parser, baseline rules, and header-preserving renderer."""
    cases = [
        (
            "no duplicates pass",
            not check(duplicates({"one": {"1.0"}}), {}),
        ),
        (
            "an unpinned duplicate fails",
            check(duplicates({"crate": {"1.0", "2.0"}}), {})
            == ["new duplicate: crate 1.0 2.0"],
        ),
        (
            "a pinned duplicate passes",
            not check({"crate": ["1.0", "2.0"]}, {"crate": 2}),
        ),
        (
            "a third version fails",
            check(
                {"crate": ["1.0", "2.0", "3.0"]}, {"crate": 2}
            )
            == ["more versions of crate: 3 > 2"],
        ),
        (
            "a stale entry fails",
            check({}, {"crate": 2})
            == [
                "stale entry: crate (now 1); "
                "shrink the list with just ratchet-update"
            ],
        ),
        (
            "a same-count version swap passes",
            not check({"crate": ["1.1", "2.0"]}, {"crate": 2}),
        ),
    ]
    header = ["# duplicate-crate ratchet\n", "\n", "# keep this header\n"]
    rendered = render(header, {"crate": ["1.0", "2.0"]})
    cases.append(
        ("render keeps the header byte-for-byte", rendered.startswith("".join(header)))
    )

    lock_excerpt = '''[[package]]
name = "addr2line"
version = "0.25.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "1b5d307320b3181d6d7954e663bd7c774a838b8220fe0593c86d9fb09f498b4b"
dependencies = [
 "gimli",
]

[[package]]
name = "adler2"
version = "2.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "320119579fcad9c21884f5c4861d16174d0e06250625266f50fe6898340abefa"

[[package]]
name = "aead"
version = "0.5.2"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "d122413f284cf2d62fb1b7db97e02edb8cda96d769b16e443a4f6195e35662b0"
dependencies = [
 "crypto-common 0.1.7",
 "generic-array",
]
'''
    parsed = parse_lock(lock_excerpt)
    cases.append(
        (
            "package metadata and dependency arrays parse",
            parsed == {
                "addr2line": {"0.25.1"},
                "adler2": {"2.0.1"},
                "aead": {"0.5.2"},
            },
        )
    )

    failed = False
    for label, passed in cases:
        if not passed:
            print(f"FAIL: {label}", file=sys.stderr)
            failed = True
    if failed:
        return 1
    print(f"OK: {len(cases)} duplicate-crate guard self-tests")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--update", action="store_true", help="regenerate the baseline")
    mode.add_argument("--self-test", action="store_true", help="run in-memory cases")
    args = parser.parse_args()
    if args.self_test:
        return self_test()

    try:
        lock_path = ROOT / "Cargo.lock"
        lock_pkgs = parse_lock(lock_path.read_text())
        current = duplicates(lock_pkgs)
        if args.update:
            if BASELINE.exists():
                header, _ = load_allowlist(BASELINE.read_text())
            else:
                header = []
            BASELINE.write_text(render(header, current))
            print(f"Updated {BASELINE.relative_to(ROOT)} ({len(current)} duplicate names)")
            return 0

        header, allow = load_allowlist(BASELINE.read_text())
        problems = check(current, allow)
    except (OSError, ValueError) as exc:
        print(f"duplicate-crate ratchet: {exc}", file=sys.stderr)
        return 1

    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        return 1
    print(f"OK: {len(current)} duplicate crate names match {BASELINE.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
