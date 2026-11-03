#!/usr/bin/env python3
"""Screen release metadata as inert text before a publisher uses it."""

from __future__ import annotations

import hashlib
import os
import stat
import sys
import unicodedata
from pathlib import Path


# Match the existing .SRCINFO limit in release.yml.
MAX_METADATA_BYTES = 64 * 1024


def screen(data: bytes) -> str | None:
    """Return a refusal reason for unsafe metadata, without interpreting it."""
    if not data:
        return "metadata is empty"
    if len(data) > MAX_METADATA_BYTES:
        return f"metadata is at least {len(data)} bytes; maximum is {MAX_METADATA_BYTES} bytes"
    try:
        text = data.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        return f"metadata is not strict UTF-8 at byte offset {error.start}"

    byte_offset = 0
    for character in text:
        codepoint = ord(character)
        category = unicodedata.category(character)
        is_disallowed_c0 = codepoint < 0x20 and character not in "\t\n"
        if (
            character == "\r"
            or is_disallowed_c0
            or codepoint == 0x7F
            or 0x80 <= codepoint <= 0x9F
            or category == "Cf"
        ):
            return f"disallowed U+{codepoint:04X} at byte offset {byte_offset}"
        byte_offset += len(character.encode("utf-8"))
    return None


def _read_regular_file(path: Path) -> bytes:
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode):
        raise ValueError("input is not a regular file")

    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags)
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise ValueError("input is not a regular file")
        with os.fdopen(descriptor, "rb", closefd=False) as handle:
            return handle.read(MAX_METADATA_BYTES + 1)
    finally:
        os.close(descriptor)


def main(argv: list[str]) -> int:
    if not argv:
        print("usage: screen_metadata.py FILE...", file=sys.stderr)
        return 2

    for raw_path in argv:
        path = Path(raw_path)
        try:
            data = _read_regular_file(path)
        except (OSError, ValueError) as error:
            print(f"refused: {path}: {error}", file=sys.stderr)
            return 1

        reason = screen(data)
        if reason is not None:
            print(f"refused: {path}: {reason}", file=sys.stderr)
            return 1
        digest = hashlib.sha256(data).hexdigest()
        print(f"ok: {path} {len(data)} bytes sha256={digest}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
