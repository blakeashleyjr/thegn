#!/usr/bin/env python3
"""Every external GitHub Action reference must be an immutable commit SHA.

A privileged workflow is only as trustworthy as the code it invokes. The
release jobs here hold `contents: write`, `id-token: write`,
`attestations: write` and publishing secrets, so a third-party action resolved
through a MUTABLE ref (`@main`, `@v4`, `@stable`) hands whoever can retarget
that ref arbitrary execution inside those jobs. Tags are not immutable on
GitHub: a tag can be force-moved to any commit at any time.

The scan is recursive on purpose. A SHA-pinned workflow step that calls a
LOCAL composite action inherits everything that action invokes, so pinning
only the workflow layer can hide a mutable transitive reference one level
down -- which is exactly the shape this repository had: every caller of
`./.github/actions/ci-setup` looked clean while the composite itself ran
`DeterminateSystems/nix-installer-action@main`.

Run directly (`python3 -B test/action_pin_test.py`) or via `just
test-action-pins`; `just test` includes it.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
GITHUB = ROOT / ".github"
ALLOWLIST = Path(__file__).resolve().parent / "action-pin-allowlist.txt"

SHA = re.compile(r"^[0-9a-f]{40}$")
# `uses:` values we accept without a SHA: a local action in this repository
# (recursed into) and a fully-qualified docker digest.
LOCAL = re.compile(r"^\./")
DOCKER_DIGEST = re.compile(r"^docker://[^@]+@sha256:[0-9a-f]{64}$")


def load_allowlist() -> set[str]:
    """Reviewed exceptions, one `owner/repo@ref` per line; `#` comments.

    Seeded EMPTY: every reference in the tree is pinned. An addition here is a
    deliberate, reviewed decision to run a mutable reference in a privileged
    workflow, and needs a comment saying why.
    """
    if not ALLOWLIST.exists():
        return set()
    entries = set()
    for line in ALLOWLIST.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            entries.add(line)
    return entries


def uses_refs(doc: object) -> list[str]:
    """Every `uses:` value anywhere in a parsed workflow/action document."""
    found: list[str] = []
    if isinstance(doc, dict):
        for key, value in doc.items():
            if key == "uses" and isinstance(value, str):
                found.append(value)
            else:
                found.extend(uses_refs(value))
    elif isinstance(doc, list):
        for item in doc:
            found.extend(uses_refs(item))
    return found


def local_action_file(ref: str) -> Path | None:
    """Resolve `./.github/actions/x` to its action.yml, or None if missing."""
    # removeprefix, not lstrip: lstrip strips CHARACTERS, so "./.github/..."
    # would lose the leading dot of ".github" too.
    base = ROOT / ref.removeprefix("./")
    for name in ("action.yml", "action.yaml"):
        candidate = base / name
        if candidate.is_file():
            return candidate
    return base if base.is_file() else None


def scan(path: Path, allowlist: set[str], seen: set[Path]) -> list[str]:
    """Report every mutable reference reachable from `path`, recursively."""
    if path in seen:
        return []
    seen.add(path)
    try:
        doc = yaml.safe_load(path.read_text())
    except yaml.YAMLError as exc:
        return [f"{path.relative_to(ROOT)}: unparsable YAML: {exc}"]

    problems: list[str] = []
    for ref in uses_refs(doc):
        if LOCAL.match(ref):
            target = local_action_file(ref)
            if target is None:
                problems.append(
                    f"{path.relative_to(ROOT)}: local action {ref!r} does not exist"
                )
                continue
            # Recurse: a clean caller must not hide a mutable callee.
            problems.extend(scan(target, allowlist, seen))
            continue
        if DOCKER_DIGEST.match(ref):
            continue
        if ref in allowlist:
            continue
        if "@" not in ref:
            problems.append(
                f"{path.relative_to(ROOT)}: {ref!r} has no ref; pin a commit SHA"
            )
            continue
        _, _, rev = ref.rpartition("@")
        if not SHA.match(rev):
            problems.append(
                f"{path.relative_to(ROOT)}: {ref!r} is a mutable ref "
                f"({rev!r}); pin the full commit SHA"
            )
    return problems


def comment_problems() -> list[str]:
    """A bare SHA is unreadable; each pin carries its version in a comment."""
    problems: list[str] = []
    pattern = re.compile(r"uses:\s*(?!\./)(\S+@[0-9a-f]{40})(.*)$")
    for path in sorted(GITHUB.rglob("*.yml")) + sorted(GITHUB.rglob("*.yaml")):
        for number, line in enumerate(path.read_text().splitlines(), start=1):
            match = pattern.search(line)
            if match and "#" not in match.group(2):
                problems.append(
                    f"{path.relative_to(ROOT)}:{number}: {match.group(1)} "
                    "is pinned but has no version comment"
                )
    return problems


def main() -> int:
    if not GITHUB.is_dir():
        print("no .github directory; nothing to check")
        return 0
    allowlist = load_allowlist()
    workflows = sorted((GITHUB / "workflows").glob("*.yml")) + sorted(
        (GITHUB / "workflows").glob("*.yaml")
    )
    if not workflows:
        print("FAIL: no workflows found; the scan would vacuously pass")
        return 1

    seen: set[Path] = set()
    problems: list[str] = []
    for workflow in workflows:
        problems.extend(scan(workflow, allowlist, seen))
    # Composite actions no workflow happens to call are still checked, so an
    # unreferenced action cannot rot into a mutable reference unnoticed.
    for action in sorted(GITHUB.rglob("action.yml")) + sorted(
        GITHUB.rglob("action.yaml")
    ):
        problems.extend(scan(action, allowlist, seen))
    problems.extend(comment_problems())

    if problems:
        print("FAIL: mutable or unreadable GitHub Action references:\n")
        for problem in problems:
            print(f"  {problem}")
        print(
            "\nPin each to a full 40-character commit SHA with a trailing "
            "version comment, e.g.\n"
            "  uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0\n"
            f"See docs/ci-action-pinning.md. Reviewed exceptions go in "
            f"{ALLOWLIST.relative_to(ROOT)}."
        )
        return 1

    print(
        f"OK: {len(seen)} workflow/action files; every external reference is "
        "SHA-pinned with a version comment"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
