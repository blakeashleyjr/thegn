#!/usr/bin/env python3
"""Hermetic consistency checks for the optional terminal demo driver."""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path


class DemoError(ValueError):
    pass


@dataclass(frozen=True)
class Step:
    kind: str
    arg: str
    note: str
    line: int


KINDS = {"wait", "key", "text", "pause", "snap", "detach", "reattach", "manual", "cleanup"}
CHORD = re.compile(
    r"^(?:(?:ctrl|alt|shift|super)\+)*(?:[a-z0-9]|enter|escape|space|tab|up|down|left|right|f[0-9]{1,2})$"
)
DEMO_CHORDS = {"alt+w", "ctrl+space", "alt+p", "alt+n", "alt+x", "alt+shift+x", "ctrl+q"}


def parse_steps(text: str) -> list[Step]:
    steps: list[Step] = []
    for number, raw in enumerate(text.splitlines(), 1):
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        fields = raw.split("\t")
        if len(fields) not in (2, 3):
            raise DemoError(f"steps.tsv:{number}: expected 2 or 3 tab-separated columns")
        kind, arg = fields[0].strip(), fields[1].strip()
        note = fields[2].strip() if len(fields) == 3 else ""
        if kind not in KINDS:
            raise DemoError(f"steps.tsv:{number}: unknown kind {kind!r}")
        if kind == "pause":
            try:
                int(arg)
            except ValueError as error:
                raise DemoError(f"steps.tsv:{number}: pause must be an integer") from error
        if kind == "wait":
            if len(arg) < 4:
                raise DemoError(f"steps.tsv:{number}: wait text must be at least 4 characters")
            if note:
                try:
                    int(note)
                except ValueError as error:
                    raise DemoError(f"steps.tsv:{number}: wait timeout must be an integer") from error
        if kind == "key" and (not arg or CHORD.fullmatch(arg) is None):
            raise DemoError(f"steps.tsv:{number}: invalid key chord {arg!r}")
        steps.append(Step(kind, arg, note, number))
    return steps


CHORD_TOKEN = re.compile(r"(?<![A-Za-z0-9])((?:(?:Alt|Ctrl|Shift|Super)-)+[A-Za-z0-9]+)", re.IGNORECASE)


def readme_chords(readme_text: str) -> set[str]:
    chords: set[str] = set()
    for match in CHORD_TOKEN.finditer(readme_text):
        pieces = match.group(1).split("-")
        key = pieces[-1]
        modifiers = [part.lower() for part in pieces[:-1]]
        if len(key) == 1 and key.isupper() and "alt" in modifiers:
            modifiers.append("shift")
        chords.add("+".join([*modifiers, key.lower()]))
    return chords


def check(steps: list[Step], readme_text: str) -> list[str]:
    problems: list[str] = []
    documented = readme_chords(readme_text)
    for step in steps:
        if step.kind == "key" and step.arg in DEMO_CHORDS and step.arg not in documented:
            problems.append(f"steps.tsv:{step.line}: {step.arg} is not documented in README.md")
        if step.kind == "manual" and not step.arg:
            problems.append(f"steps.tsv:{step.line}: manual instruction is empty")

    required = [
        ("key", "alt+w"),
        ("key", "ctrl+space"),
        ("key", "alt+p"),
        ("key", "alt+x"),
        ("key", "alt+shift+x"),
        ("detach", None),
        ("reattach", None),
    ]
    cursor = 0
    for kind, arg in required:
        try:
            cursor = next(
                index + 1
                for index in range(cursor, len(steps))
                if steps[index].kind == kind and (arg is None or steps[index].arg == arg)
            )
        except StopIteration:
            problems.append(f"missing ordered demo step: {kind} {arg or ''}".rstrip())
            break
    if not steps or steps[-1].kind != "cleanup":
        problems.append("last step must be cleanup")
    return problems


def repo_paths() -> tuple[Path, Path, Path]:
    root = Path(__file__).resolve().parents[2]
    return root, root / "test/demo/steps.tsv", root / "README.md"


def selftest() -> list[str]:
    failures: list[str] = []

    def expect_error(label: str, source: str, fragment: str) -> None:
        try:
            parse_steps(source)
        except DemoError as error:
            if fragment not in str(error):
                failures.append(label)
        else:
            failures.append(label)

    expect_error("bad-column-count", "key\talt+w\textra\ttoo-many\n", "columns")
    expect_error("unknown-kind", "wat\tx\t-\n", "unknown kind")
    expect_error("non-integer-pause", "pause\tsoon\t-\n", "integer")
    expect_error("short-wait", "wait\t$\t-\n", "4 characters")
    expect_error("bad-chord", "key\talt+not-a-key\t-\n", "invalid key chord")

    valid = parse_steps(
        "key\talt+w\t-\nkey\tctrl+space\t-\nkey\talt+p\t-\n"
        "key\talt+x\t-\nkey\talt+shift+x\t-\ndetach\t-\t-\n"
        "reattach\t-\t-\ncleanup\t-\t-\n"
    )
    without_shift_remove = [step for step in valid if step.arg != "alt+shift+x"]
    if not any("alt+shift+x" in problem for problem in check(without_shift_remove, "")):
        failures.append("missing-alt-shift-x-step")
    without_reattach = [step for step in valid if step.kind != "reattach"]
    if not any("reattach" in problem for problem in check(without_reattach, "")):
        failures.append("detach-without-reattach")
    if not any("last step must be cleanup" in problem for problem in check(valid[:-1], "")):
        failures.append("last-step-not-cleanup")

    chords = readme_chords(
        "| Alt-w / Alt-W | new worktree |\n| Alt-x / Alt-X | close | Ctrl-Space opens palette"
    )
    if not {"alt+w", "alt+shift+w", "alt+x", "alt+shift+x", "ctrl+space"}.issubset(chords):
        failures.append("readme-chord-map")

    root, steps_path, readme_path = repo_paths()
    committed_steps = parse_steps(steps_path.read_text(encoding="utf-8"))
    if check(committed_steps, readme_path.read_text(encoding="utf-8")):
        failures.append("committed-steps-parse")
    bash = shutil.which("bash")
    if bash is None:
        failures.append("dry-run-needs-neither-muse-nor-thegn (bash unavailable)")
    else:
        with tempfile.TemporaryDirectory(prefix="thegn-demo-check-") as output:
            env = {"PATH": str(Path(bash).parent), "HOME": output}
            result = subprocess.run(
                [bash, str(root / "test/demo/demo.sh"), "--dry-run", "--out", output],
                cwd=root,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            if result.returncode != 0 or "step 1/" not in result.stdout:
                failures.append("dry-run-needs-neither-muse-nor-thegn")
    return failures


def main(argv: list[str]) -> int:
    root, default_steps, default_readme = repo_paths()
    args = list(argv)
    if "--selftest" in args:
        failures = selftest()
        if failures:
            for label in failures:
                print(f"FAIL {label}", file=sys.stderr)
            return 1
        print("selftest ok")
        return 0

    def option(name: str, default: Path) -> Path:
        if name not in args:
            return default
        index = args.index(name)
        if index + 1 >= len(args):
            raise DemoError(f"missing value for {name}")
        return Path(args[index + 1])

    try:
        steps_path = option("--steps", default_steps)
        readme_path = option("--readme", default_readme)
        steps = parse_steps(steps_path.read_text(encoding="utf-8"))
        problems = check(steps, readme_path.read_text(encoding="utf-8"))
    except (DemoError, OSError) as error:
        print(error, file=sys.stderr)
        return 1
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    bash = shutil.which("bash")
    if bash is not None:
        syntax = subprocess.run([bash, "-n", str(root / "test/demo/demo.sh")], check=False)
        if syntax.returncode != 0:
            return syntax.returncode
        with tempfile.TemporaryDirectory(prefix="thegn-demo-check-") as output:
            result = subprocess.run(
                [bash, str(root / "test/demo/demo.sh"), "--dry-run", "--out", output],
                cwd=root,
                text=True,
                capture_output=True,
                check=False,
            )
            if result.returncode != 0:
                print(result.stderr, file=sys.stderr)
                return result.returncode
            if "step 1/" not in result.stdout:
                print("demo dry-run did not print a step plan", file=sys.stderr)
                return 1
    print(f"demo script ok ({len(steps)} steps)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
