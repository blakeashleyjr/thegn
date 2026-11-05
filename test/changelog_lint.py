#!/usr/bin/env python3
"""Reject internal process labels in newly added changelog headings."""

import argparse
import re
import sys
from pathlib import Path

BASELINE = "0.1.0-alpha.55"
HEADING = re.compile(r"^## \[(?P<version>[^\]]+)\]")
VERSION = re.compile(
    r"^(?P<major>0|[1-9]\d*)\.(?P<minor>0|[1-9]\d*)\.(?P<patch>0|[1-9]\d*)"
    r"(?:-(?P<prerelease>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
INTERNAL_LABEL = re.compile(r"\b(?:batch|lane)\b|\bTHE-\d+\b", re.IGNORECASE)


def version_key(v: str) -> tuple:
    """Return a SemVer ordering key, ignoring build metadata."""
    match = VERSION.fullmatch(v)
    if match is None:
        raise ValueError(f"invalid semantic version: {v}")
    prerelease = match.group("prerelease")
    identifiers = ()
    if prerelease is not None:
        identifiers = tuple(
            (0, int(part)) if part.isdigit() else (1, part)
            for part in prerelease.split(".")
        )
    return (
        int(match.group("major")),
        int(match.group("minor")),
        int(match.group("patch")),
        0 if prerelease is not None else 1,
        identifiers,
    )


def lint(text: str, baseline: str) -> list[tuple[int, str]]:
    baseline_key = version_key(baseline)
    violations = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        heading = HEADING.match(line)
        if heading is None:
            continue
        version = heading.group("version")
        if version == "Unreleased" or version_key(version) <= baseline_key:
            continue
        label = INTERNAL_LABEL.search(line)
        if label is not None:
            violations.append((line_number, label.group(0)))
    return violations


def selftest() -> None:
    assert lint("## [0.1.0-alpha.56] — 2026-11-06\n", BASELINE) == []
    assert lint(
        "## [0.1.0-alpha.56] — 2026-11-06 (maintenance batch 94)\n", BASELINE
    ) == [(1, "batch")]
    assert lint(
        "## [0.1.0-alpha.55] — 2026-11-05 (maintenance batch 93)\n"
        "## [0.1.0-alpha.9] — 2026-10-18 (batch 46)\n",
        BASELINE,
    ) == []
    assert version_key("0.1.0-alpha.10") > version_key("0.1.0-alpha.9")
    assert lint("## [Unreleased]\n", BASELINE) == []
    assert lint("## [0.1.0-alpha.57] — THE-123\n", BASELINE) == [(1, "THE-123")]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", default="CHANGELOG.md", help="changelog file to check")
    parser.add_argument("--baseline", default=BASELINE, help="first version excluded from checks")
    parser.add_argument("--selftest", action="store_true", help="run hermetic embedded cases")
    args = parser.parse_args(argv)
    if args.selftest:
        selftest()
        print("selftest ok")
        return 0
    try:
        text = Path(args.file).read_text(encoding="utf-8")
        violations = lint(text, args.baseline)
    except (OSError, ValueError) as error:
        print(f"changelog lint: {error}", file=sys.stderr)
        return 1
    for line_number, label in violations:
        print(
            f"{args.file}:{line_number}: heading names an internal process label "
            f"({label}); rewrite it as a plain release heading (docs/changelog-format.md)",
            file=sys.stderr,
        )
    return 1 if violations else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
