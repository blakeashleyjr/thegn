#!/usr/bin/env python3
"""Report duplicate Cargo.lock dependency roots.

Run: python3 -B test/dup_crates_report.py [--json] [--lock PATH]

Structural only: no size/security/disposition.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import NamedTuple

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dup_crates_guard  # noqa: E402


Package = tuple[str, str]
DependencyRef = tuple[str, str | None]
DEPENDENCY = re.compile(r'^\s*("(?:[^"\\]|\\.)*")\s*,?\s*$')
PACKAGE_NAME = re.compile(r'^\s*name\s*=\s*("(?:[^"\\]|\\.)*")\s*$')
PACKAGE_VERSION = re.compile(r'^\s*version\s*=\s*("(?:[^"\\]|\\.)*")\s*$')


class Graph(NamedTuple):
    edges: dict[Package, list[DependencyRef]]
    members: set[Package]


def parse_graph(text: str) -> Graph:
    """Parse package edges and workspace members from Cargo.lock text."""
    edges: dict[Package, list[DependencyRef]] = {}
    members: set[Package] = set()
    blocks = re.split(r"(?m)^\[\[package\]\]\s*$", text)

    for block in blocks[1:]:
        name: str | None = None
        version: str | None = None
        has_source = False
        dependencies: list[DependencyRef] = []
        in_dependencies = False

        for line in block.splitlines():
            stripped = line.strip()
            if in_dependencies:
                if stripped == "]":
                    in_dependencies = False
                    continue
                match = DEPENDENCY.match(line)
                if match is not None:
                    tokens = json.loads(match.group(1)).split()
                    if tokens:
                        version_ref = tokens[1] if len(tokens) > 1 else None
                        dependencies.append((tokens[0], version_ref))
                continue

            if stripped.startswith("dependencies = ["):
                in_dependencies = True
            elif stripped.startswith("source ="):
                has_source = True
            elif name is None and (match := PACKAGE_NAME.match(line)):
                name = json.loads(match.group(1))
            elif version is None and (match := PACKAGE_VERSION.match(line)):
                version = json.loads(match.group(1))

        if name is None or version is None:
            continue
        package = (name, version)
        edges.setdefault(package, []).extend(dependencies)
        if not has_source:
            members.add(package)

    for package in edges:
        edges[package] = sorted(set(edges[package]), key=lambda ref: (ref[0], ref[1] or ""))
    return Graph(edges=edges, members=members)


def resolve(graph: Graph, ref: DependencyRef) -> Package | None:
    """Resolve a dependency reference, refusing absent or ambiguous bare names."""
    name, version = ref
    if version is not None:
        package = (name, version)
        return package if package in graph.edges else None
    candidates = [package for package in graph.edges if package[0] == name]
    return candidates[0] if len(candidates) == 1 else None


def dependents(graph: Graph) -> dict[Package, set[Package]]:
    """Build reverse edges for dependencies which resolve unambiguously."""
    reverse: dict[Package, set[Package]] = {package: set() for package in graph.edges}
    for package, refs in graph.edges.items():
        for ref in refs:
            dependency = resolve(graph, ref)
            if dependency is not None:
                reverse[dependency].add(package)
    return reverse


def reaching_members(graph: Graph, target: Package) -> list[str]:
    """Return sorted workspace-member names that can reach target."""
    reverse = dependents(graph)
    return _reaching_members(graph, target, reverse)


def _reaching_members(
    graph: Graph, target: Package, reverse: dict[Package, set[Package]]
) -> list[str]:
    visited = {target}
    pending = [target]
    roots: set[str] = set()
    while pending:
        package = pending.pop()
        if package in graph.members:
            roots.add(package[0])
        for parent in reverse.get(package, set()):
            if parent not in visited:
                visited.add(parent)
                pending.append(parent)
    return sorted(roots)


def report(text: str) -> dict:
    """Return deterministic structural details for each duplicate version."""
    graph = parse_graph(text)
    reverse = dependents(graph)
    duplicate_versions = dup_crates_guard.duplicates(dup_crates_guard.parse_lock(text))
    duplicate_packages = {
        (name, version)
        for name, versions in duplicate_versions.items()
        for version in versions
    }

    duplicate_rows: dict[str, list[dict[str, object]]] = {}
    for name, versions in sorted(duplicate_versions.items()):
        rows: list[dict[str, object]] = []
        for version in versions:
            package = (name, version)
            rows.append(
                {
                    "version": version,
                    "dependents": sorted(
                        f"{dep_name} {dep_version}"
                        for dep_name, dep_version in reverse.get(package, set())
                    ),
                    "workspace_roots": _reaching_members(graph, package, reverse),
                }
            )
        duplicate_rows[name] = rows

    introducers: list[dict[str, object]] = []
    for package, refs in graph.edges.items():
        introduced = sorted(
            f"{dependency[0]} {dependency[1]}"
            for ref in refs
            if (dependency := resolve(graph, ref)) in duplicate_packages
        )
        if introduced:
            introducers.append(
                {"package": f"{package[0]} {package[1]}", "introduces": introduced}
            )
    introducers.sort(
        key=lambda row: (-len(row["introduces"]), row["package"])
    )
    return {"duplicates": duplicate_rows, "introducers": introducers}


def render_text(rep: dict) -> str:
    """Render a stable human-readable report."""
    lines: list[str] = []
    for name, entries in sorted(rep["duplicates"].items()):
        lines.append(f"{name} ({len(entries)} versions)")
        for entry in entries:
            dependents_text = ", ".join(entry["dependents"]) or "-"
            roots_text = ", ".join(entry["workspace_roots"]) or "-"
            lines.append(
                f"  {entry['version']}  <- dependents: {dependents_text}  "
                f"[roots: {roots_text}]"
            )
    lines.extend(["", "Packages introducing the most duplicates:"])
    for row in rep["introducers"][:25]:
        introduced = ", ".join(row["introduces"])
        lines.append(f"  {len(row['introduces'])}  {row['package']}: {introduced}")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="print JSON")
    parser.add_argument(
        "--lock",
        type=Path,
        default=dup_crates_guard.ROOT / "Cargo.lock",
        metavar="PATH",
        help="Cargo.lock path (default: repository Cargo.lock)",
    )
    args = parser.parse_args(argv)

    try:
        lock_text = args.lock.read_text()
    except (OSError, UnicodeError) as error:
        print(f"dup-crates report: {error}", file=sys.stderr)
        return 1

    result = report(lock_text)
    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
    else:
        print(render_text(result), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
