#!/usr/bin/env python3
"""Check the parts of a release build that can be rehearsed on this host."""

from __future__ import annotations

import argparse
import importlib.util
import struct
import sys
from pathlib import Path
from typing import Any, Mapping


_RELEASE_PATH = Path(__file__).resolve().with_name("release.py")
_RELEASE_SPEC = importlib.util.spec_from_file_location(
    "thegn_release_for_rehearsal_check", _RELEASE_PATH
)
if _RELEASE_SPEC is None or _RELEASE_SPEC.loader is None:
    raise RuntimeError(f"cannot load release verifier library at {_RELEASE_PATH}")
release = importlib.util.module_from_spec(_RELEASE_SPEC)
sys.modules[_RELEASE_SPEC.name] = release
_RELEASE_SPEC.loader.exec_module(release)


MAX_ELF_READ = 64 * 1024
MAX_INTERPRETER_SIZE = 4096


class RehearsalError(ValueError):
    """Invalid ELF data or an ambiguous host-target mapping."""


def _malformed(path: Path, detail: str) -> RehearsalError:
    return RehearsalError(f"malformed ELF {path}: {detail}")


def elf_interpreter(path: Path) -> str | None:
    """Read the ELF64 little-endian PT_INTERP segment, if there is one."""
    try:
        with path.open("rb") as binary:
            data = binary.read(MAX_ELF_READ)
    except OSError as error:
        raise RehearsalError(f"cannot read binary {path}: {error}") from error

    if not data.startswith(b"\x7fELF"):
        return None
    if len(data) < 6:
        raise _malformed(path, "truncated ELF identification")
    if data[4] != 2 or data[5] != 1:
        raise RehearsalError(f"unsupported ELF class/byte order in {path}")
    if len(data) < 64:
        raise _malformed(path, "truncated ELF header")

    phoff = struct.unpack_from("<Q", data, 32)[0]
    phentsize = struct.unpack_from("<H", data, 54)[0]
    phnum = struct.unpack_from("<H", data, 56)[0]
    if phnum and phentsize < 40:
        raise _malformed(path, "program header is too small")
    if phnum and (phoff > len(data) or phnum * phentsize > len(data) - phoff):
        raise _malformed(path, "program header table falls outside the bytes read")

    for index in range(phnum):
        header = phoff + index * phentsize
        p_type = struct.unpack_from("<I", data, header)[0]
        if p_type != 3:
            continue
        p_offset = struct.unpack_from("<Q", data, header + 8)[0]
        p_filesz = struct.unpack_from("<Q", data, header + 32)[0]
        if p_filesz > MAX_INTERPRETER_SIZE:
            raise _malformed(path, "PT_INTERP segment exceeds 4096 bytes")
        if p_offset > len(data) or p_filesz > len(data) - p_offset:
            raise _malformed(path, "PT_INTERP segment falls outside the bytes read")
        raw_interpreter = data[p_offset : p_offset + p_filesz]
        if not raw_interpreter.endswith(b"\0"):
            raise _malformed(path, "PT_INTERP is not NUL-terminated")
        try:
            return raw_interpreter[:-1].decode("utf-8")
        except UnicodeDecodeError as error:
            raise _malformed(path, "PT_INTERP is not valid UTF-8") from error
    return None


def host_only_findings(binary: Path) -> list[str]:
    interpreter = elf_interpreter(binary)
    if interpreter is not None and interpreter.startswith("/nix/store/"):
        return [
            f"{binary}: ELF interpreter {interpreter} is a Nix store path: this build runs "
            "only where that store path exists; the shipped linux-gnu archive is built on "
            "ubuntu-latest by .github/workflows/release.yml"
        ]
    return []


def manager_for_target(manifest: Mapping[str, Any], target: str) -> str:
    managers = manifest.get("managers")
    if not isinstance(managers, Mapping):
        raise RehearsalError(f"no enabled package manager for target {target!r}")
    matches = [
        name
        for name, config in managers.items()
        if isinstance(name, str)
        and isinstance(config, Mapping)
        and config.get("enabled") is True
        and config.get("target") == target
    ]
    if len(matches) != 1:
        reason = "no enabled manager" if not matches else "more than one enabled manager"
        raise RehearsalError(f"{reason} for target {target!r}")
    return matches[0]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    manager_parser = commands.add_parser("manager", help="find the enabled manager for a target")
    manager_parser.add_argument("--target", required=True)
    binary_parser = commands.add_parser("check-binary", help="check for a Nix-only ELF interpreter")
    binary_parser.add_argument("--binary", required=True, type=Path)
    binary_parser.add_argument("--host-only", action="store_true")
    args = parser.parse_args(argv)

    try:
        if args.command == "manager":
            print(manager_for_target(release.load_manifest(), args.target))
            return 0
        findings = host_only_findings(args.binary)
        if args.host_only:
            for finding in findings:
                print(f"release-rehearsal: WARNING (host-only rehearsal): {finding}", file=sys.stderr)
            return 0
        for finding in findings:
            print(f"release-rehearsal: {finding}", file=sys.stderr)
        return 1 if findings else 0
    except RehearsalError as error:
        print(f"release-rehearsal: {error}", file=sys.stderr)
        return 2
    except release.ReleaseError as error:
        print(f"release-rehearsal: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
