#!/usr/bin/env python3
"""Strictly compare rustc dep-info prerequisites with the current core sources."""

import os
import re
import stat
import sys


SOURCE_SUFFIX = re.compile(r"(?:^|/)crates/thegn-core/src/(.+\.rs)\Z")
MAX_DEPINFO_BYTES = 64 * 1024 * 1024
MAX_SOURCE_FILES = 100_000


class UnsupportedDepInfo(Exception):
    pass


def validated_source_identity(path: str) -> str:
    """Validate the original path before resolving aliases to a physical file."""
    if not path or any(char.isspace() for char in path) or "\\" in path:
        raise UnsupportedDepInfo("unsupported source path")
    # Check the exact spelling first. realpath can simplify an existing but
    # unsearchable (or non-directory) component before `..` without asking the
    # kernel to traverse it, while stat must resolve the original pathname.
    info = os.stat(path)
    if not stat.S_ISREG(info.st_mode):
        raise UnsupportedDepInfo("source path is not a regular file")
    return os.path.realpath(path, strict=True)


def source_paths(root: str) -> set[str]:
    src = os.path.join(root, "crates", "thegn-core", "src")
    if not os.path.isdir(src):
        raise UnsupportedDepInfo("missing core source directory")
    required: set[str] = set()
    def walk_error(error: OSError) -> None:
        raise error

    for directory, dirs, files in os.walk(src, followlinks=False, onerror=walk_error):
        dirs[:] = [name for name in dirs if not os.path.islink(os.path.join(directory, name))]
        for name in files:
            path = os.path.join(directory, name)
            if name.endswith(".rs"):
                info = os.lstat(path)
                if stat.S_ISREG(info.st_mode):
                    required.add(validated_source_identity(path))
                    if len(required) > MAX_SOURCE_FILES:
                        raise UnsupportedDepInfo("source tree exceeds parser bound")
    if not required:
        raise UnsupportedDepInfo("no Rust source files")
    return required


def parse_prerequisites(depinfo: bytes) -> set[str]:
    if b"\0" in depinfo:
        raise UnsupportedDepInfo("NUL in dep-info")
    try:
        text = depinfo.decode("utf-8", "strict")
    except UnicodeDecodeError as error:
        raise UnsupportedDepInfo("dep-info is not UTF-8") from error
    rule_sets: list[set[str]] = []
    for line in text.split("\n"):
        if not line:
            continue
        if line.startswith("#"):
            if not line.startswith(("# env-dep:", "# checksum:")):
                raise UnsupportedDepInfo("unknown dep-info comment")
            continue
        # rustc escapes spaces in Make paths with backslash-space. This parser
        # admits plain UTF-8 tokens with ASCII spaces only as separators.
        if "\\" in line or any(char.isspace() and char != " " for char in line):
            raise UnsupportedDepInfo("escaped or control path syntax")
        if line.count(":") != 1:
            raise UnsupportedDepInfo("unsupported Make rule")
        target, rhs = line.split(":", 1)
        if not target or any(char.isspace() for char in target):
            raise UnsupportedDepInfo("unsupported Make target")
        # rustc emits zero-prerequisite dummy targets after its real rules.
        # Only nonempty right-hand sides can contribute source coverage.
        prerequisites = [token for token in rhs.split(" ") if token]
        if not prerequisites:
            continue
        if any(":" in token or "#" in token for token in prerequisites):
            raise UnsupportedDepInfo("unsupported prerequisite token")
        rule_sets.append(set(prerequisites))

    if not rule_sets:
        raise UnsupportedDepInfo("no prerequisite-bearing rustc rule")
    if any(rule != rule_sets[0] for rule in rule_sets[1:]):
        raise UnsupportedDepInfo("inconsistent rustc prerequisite rules")
    return rule_sets[0]


def source_dependencies(root: str, prerequisites: set[str]) -> tuple[set[str], bool, str]:
    checkout = os.path.realpath(root)
    if any(char.isspace() for char in checkout):
        raise UnsupportedDepInfo("checkout path contains whitespace")
    checkout_sources = os.path.join(checkout, "crates", "thegn-core", "src")
    dependencies: set[str] = set()
    foreign = False
    lib_path = ""
    for token in prerequisites:
        match = SOURCE_SUFFIX.search(token)
        if not match:
            continue
        path = token if os.path.isabs(token) else os.path.join(checkout, token)
        identity = validated_source_identity(path)
        dependencies.add(identity)
        if not (identity.startswith(checkout_sources + os.sep) and os.path.isfile(identity)):
            foreign = True
        if match.group(1) == "lib.rs":
            lib_path = token
    if not dependencies or not lib_path:
        raise UnsupportedDepInfo("core sources absent from rustc prerequisites")
    return dependencies, foreign, lib_path


def main() -> int:
    if len(sys.argv) != 3:
        return 2
    try:
        root = os.path.realpath(sys.argv[1])
        if not os.path.isdir(root):
            raise UnsupportedDepInfo("checkout is not a directory")
        with open(sys.argv[2], "rb") as depinfo_file:
            raw = depinfo_file.read(MAX_DEPINFO_BYTES + 1)
        if len(raw) > MAX_DEPINFO_BYTES:
            raise UnsupportedDepInfo("dep-info exceeds parser bound")
        prerequisites = parse_prerequisites(raw)
        listed, foreign, lib_path = source_dependencies(root, prerequisites)
        required = source_paths(root)
        missing = len(required - listed)
        print(f"{missing}\t{int(foreign)}\t{lib_path}")
        return 0
    except (OSError, UnsupportedDepInfo, ValueError):
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
