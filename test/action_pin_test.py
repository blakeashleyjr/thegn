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
test-action-pins`; `just test` and `just lint` include it.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent

SHA = re.compile(r"^[0-9a-f]{40}$")
# `uses:` values we accept without a SHA: a local action in this repository
# (recursed into) and a fully-qualified docker digest.
LOCAL = re.compile(r"^\./")
DOCKER_DIGEST = re.compile(r"^docker://[^@]+@sha256:[0-9a-f]{64}$")


def load_allowlist(root: Path = ROOT) -> set[str]:
    """Reviewed exceptions, one `owner/repo@ref` per line; `#` comments.

    Seeded EMPTY: every reference in the tree is pinned. An addition here is a
    deliberate, reviewed decision to run a mutable reference in a privileged
    workflow, and needs a comment saying why.
    """
    allowlist = root / "test" / "action-pin-allowlist.txt"
    if not allowlist.exists():
        return set()
    entries = set()
    for line in allowlist.read_text().splitlines():
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


def local_action_file(ref: str, root: Path = ROOT) -> Path | None:
    """Resolve `./.github/actions/x` to its action.yml, or None if missing."""
    # removeprefix, not lstrip: lstrip strips CHARACTERS, so "./.github/..."
    # would lose the leading dot of ".github" too.
    base = root / ref.removeprefix("./")
    for name in ("action.yml", "action.yaml"):
        candidate = base / name
        if candidate.is_file():
            return candidate
    return base if base.is_file() else None


def scan(
    path: Path, allowlist: set[str], seen: set[Path], root: Path = ROOT
) -> list[str]:
    """Report every mutable reference reachable from `path`, recursively."""
    if path in seen:
        return []
    seen.add(path)
    try:
        doc = yaml.safe_load(path.read_text())
    except yaml.YAMLError as exc:
        return [f"{path.relative_to(root)}: unparsable YAML: {exc}"]

    problems: list[str] = []
    for ref in uses_refs(doc):
        if LOCAL.match(ref):
            target = local_action_file(ref, root)
            if target is None:
                problems.append(
                    f"{path.relative_to(root)}: local action {ref!r} does not exist"
                )
                continue
            # Recurse: a clean caller must not hide a mutable callee.
            problems.extend(scan(target, allowlist, seen, root))
            continue
        if DOCKER_DIGEST.match(ref):
            continue
        if ref in allowlist:
            continue
        if "@" not in ref:
            problems.append(
                f"{path.relative_to(root)}: {ref!r} has no ref; pin a commit SHA"
            )
            continue
        _, _, rev = ref.rpartition("@")
        if not SHA.match(rev):
            problems.append(
                f"{path.relative_to(root)}: {ref!r} is a mutable ref "
                f"({rev!r}); pin the full commit SHA"
            )
    return problems


def comment_problems(root: Path = ROOT) -> list[str]:
    """A bare SHA is unreadable; each pin carries its version in a comment."""
    problems: list[str] = []
    pattern = re.compile(r"uses:\s*(?!\./)(\S+@[0-9a-f]{40})(.*)$")
    github = root / ".github"
    for path in sorted(github.rglob("*.yml")) + sorted(github.rglob("*.yaml")):
        for number, line in enumerate(path.read_text().splitlines(), start=1):
            match = pattern.search(line)
            if match and "#" not in match.group(2):
                problems.append(
                    f"{path.relative_to(root)}:{number}: {match.group(1)} "
                    "is pinned but has no version comment"
                )
    return problems


def validate(root: Path = ROOT) -> tuple[set[Path], list[str]]:
    """Return scanned files and policy violations for a repository root."""
    github = root / ".github"
    if not github.is_dir():
        return set(), []
    allowlist = load_allowlist(root)
    workflows = sorted((github / "workflows").glob("*.yml")) + sorted(
        (github / "workflows").glob("*.yaml")
    )
    if not workflows:
        return set(), ["no workflows found; the scan would vacuously pass"]

    seen: set[Path] = set()
    problems: list[str] = []
    for workflow in workflows:
        problems.extend(scan(workflow, allowlist, seen, root))
    # Composite actions no workflow happens to call are still checked, so an
    # unreferenced action cannot rot into a mutable reference unnoticed.
    for action in sorted(github.rglob("action.yml")) + sorted(
        github.rglob("action.yaml")
    ):
        problems.extend(scan(action, allowlist, seen, root))
    problems.extend(comment_problems(root))
    return seen, problems


def main(root: Path = ROOT) -> int:
    github = root / ".github"
    if not github.is_dir():
        print("no .github directory; nothing to check")
        return 0
    seen, problems = validate(root)

    if problems:
        print("FAIL: mutable or unreadable GitHub Action references:\n")
        for problem in problems:
            print(f"  {problem}")
        print(
            "\nPin each to a full 40-character commit SHA with a trailing "
            "version comment, e.g.\n"
            "  uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0\n"
            f"See docs/ci-action-pinning.md. Reviewed exceptions go in "
            f"{(root / 'test' / 'action-pin-allowlist.txt').relative_to(root)}."
        )
        return 1

    print(
        f"OK: {len(seen)} workflow/action files; every external reference is "
        "SHA-pinned with a version comment"
    )
    return 0


def self_test() -> int:
    """Exercise rejection, valid pins, and recursive local-action scanning."""
    sha = "0123456789abcdef0123456789abcdef01234567"
    script = Path(__file__).resolve()

    def fixture(
        base: Path,
        workflow_uses: str,
        action_uses: str | None = None,
        action_dir: str = ".github/actions/fixture",
    ) -> Path:
        (base / ".github" / "workflows").mkdir(parents=True)
        (base / "test").mkdir()
        (base / "test" / "action-pin-allowlist.txt").write_text("")
        workflow = (
            "name: fixture\njobs:\n  check:\n    steps:\n"
            f"      - uses: {workflow_uses}\n"
        )
        (base / ".github" / "workflows" / "ci.yml").write_text(workflow)
        if action_uses is not None:
            action = base / action_dir / "action.yml"
            action.parent.mkdir(parents=True)
            action.write_text(
                "name: fixture\nruns:\n  using: composite\n  steps:\n"
                f"    - uses: {action_uses}\n"
            )
        return base

    with tempfile.TemporaryDirectory(prefix="action-pin-self-test-") as temp:
        root = Path(temp)
        mutable = fixture(root / "mutable", "actions/checkout@v4")
        result = subprocess.run(
            [sys.executable, "-B", str(script), "--root", str(mutable)],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0 or "mutable ref" not in result.stdout:
            print("SELF-TEST FAIL: mutable actions/checkout@v4 was not rejected")
            print(result.stdout, end="")
            print(result.stderr, end="", file=sys.stderr)
            return 1
        print(f"SELF-TEST OK: mutable actions/checkout@v4 exited {result.returncode}")

        pinned = fixture(
            root / "pinned",
            f"actions/checkout@{sha} # v4.2.2",
        )
        _, problems = validate(pinned)
        if problems:
            print(f"SELF-TEST FAIL: valid pinned ref rejected: {problems}")
            return 1

        local = fixture(
            root / "local",
            "./.github/actions/fixture",
            f"actions/checkout@{sha} # v4.2.2",
        )
        _, problems = validate(local)
        if problems:
            print(
                "SELF-TEST FAIL: local action with pinned callee rejected: "
                f"{problems}"
            )
            return 1

        # The callee lives OUTSIDE `.github/`, so `validate`'s direct
        # `rglob("action.yml")` pass cannot reach it: the only way the mutable
        # ref is found is by recursing through the workflow's `./` reference.
        # (Inside `.github/` the direct pass would catch it even with broken
        # recursion, making this case vacuous.)
        transitive = fixture(
            root / "transitive",
            "./callee",
            "actions/checkout@v4",
            action_dir="callee",
        )
        _, problems = validate(transitive)
        if not any("actions/checkout@v4" in problem for problem in problems):
            print("SELF-TEST FAIL: mutable ref in local callee was not rejected")
            return 1

        no_comment = fixture(root / "no-comment", f"actions/checkout@{sha}")
        _, problems = validate(no_comment)
        if not any("no version comment" in problem for problem in problems):
            print("SELF-TEST FAIL: pinned ref without version comment was accepted")
            return 1

    print(
        "SELF-TEST OK: SHA comments, local actions, recursive callees, "
        "and missing comments"
    )
    return 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    sys.exit(self_test() if args.self_test else main(args.root.resolve()))
