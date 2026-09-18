#!/usr/bin/env python3
"""Report the exact legacy checkout hook without changing anything.

This file is executed from the immutable Nix store by the development shell.
It intentionally does not inspect a custom/global/shared hooksPath. The only
accepted target is the ordinary, repository-local ``.git/hooks`` directory of
the main checkout (including an explicit local setting resolving exactly there).
The candidate is opened with O_NOFOLLOW and bounded reads; all other path types
and ambiguous ownership are reported and left alone.
"""

from __future__ import annotations

import hashlib
import os
import stat
import subprocess
import sys
from pathlib import Path


LEGACY_SHA256 = "90b85945a3c30c3a9aa806f87fe9585e9ce660e450672e3e5e24b40dca140365"
MAX_BYTES = 64 * 1024


def say(message: str) -> None:
    print(f"thegn legacy checkout-hook detector: {message}", file=sys.stderr)


def git(root: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args],
        cwd=root,
        check=False,
        capture_output=True,
        text=True,
    )


def refuse(reason: str) -> int:
    say(f"refusing inspection ({reason}); no files were changed")
    return 0


def local_directory(path: Path) -> os.stat_result | None:
    try:
        entry = os.lstat(path)
    except OSError:
        return None
    if not stat.S_ISDIR(entry.st_mode) or stat.S_ISLNK(entry.st_mode):
        return None
    return entry


def main() -> int:
    cwd = Path.cwd()
    top = git(cwd, "rev-parse", "--show-toplevel")
    if top.returncode != 0 or not top.stdout.strip():
        return refuse("the current directory is not a repository")
    root = Path(top.stdout.strip()).resolve()

    if git(root, "rev-parse", "--is-bare-repository").stdout.strip() != "false":
        return refuse("bare or ambiguous repository")

    dotgit = root / ".git"
    dotgit_stat = local_directory(dotgit)
    if dotgit_stat is None:
        return refuse("only a main checkout with a real .git directory is eligible")

    identity = git(root, "rev-parse", "--git-dir", "--git-common-dir")
    if identity.returncode != 0:
        return refuse("Git could not prove repository identity")
    identity_paths = []
    for line in identity.stdout.splitlines():
        path = Path(line)
        identity_paths.append((root / path if not path.is_absolute() else path).resolve())
    if len(identity_paths) != 2 or any(path != dotgit.resolve() for path in identity_paths):
        return refuse("the Git directory is linked, shared, or not this repository's .git")

    hooks = dotgit / "hooks"
    hooks_stat = local_directory(hooks)
    if hooks_stat is None:
        return refuse(".git/hooks is not a real local directory")
    if hooks_stat.st_uid != dotgit_stat.st_uid:
        return refuse(".git/hooks ownership is ambiguous")

    # git-hooks.nix may explicitly set the ordinary local path. That exact
    # value is eligible; every custom, global, system, worktree, or shared
    # setting is refused before its target can be inspected.
    local_path = git(root, "config", "--local", "--get-all", "core.hooksPath")
    if local_path.returncode not in (0, 1):
        return refuse("could not inspect the local core.hooksPath")
    local_values = local_path.stdout.splitlines()
    if len(local_values) > 1:
        return refuse("multiple local core.hooksPath values are ambiguous")
    if local_values:
        configured_path = Path(local_values[0])
        if not configured_path.is_absolute():
            configured_path = root / configured_path
        if configured_path.resolve() != hooks.resolve():
            return refuse("core.hooksPath is custom rather than this repository's .git/hooks")
    for scope in ("--global", "--system"):
        scoped = git(root, "config", scope, "--get-all", "core.hooksPath")
        if scoped.returncode not in (0, 1):
            return refuse(f"could not inspect {scope[2:]} core.hooksPath")
        if scoped.stdout.strip():
            return refuse(f"{scope[2:]} core.hooksPath is configured and out of scope")
    worktree_config = git(root, "config", "--local", "--get", "extensions.worktreeConfig")
    if worktree_config.returncode not in (0, 1):
        return refuse("could not inspect the worktree-config setting")
    if worktree_config.stdout.strip().lower() in {"true", "yes", "on", "1"}:
        scoped = git(root, "config", "--worktree", "--get-all", "core.hooksPath")
        if scoped.returncode not in (0, 1):
            return refuse("could not inspect worktree core.hooksPath")
        if scoped.stdout.strip():
            return refuse("worktree core.hooksPath is configured and out of scope")

    candidate = hooks / "post-checkout"
    try:
        candidate_stat = os.lstat(candidate)
    except FileNotFoundError:
        say("no legacy post-checkout hook found")
        return 0
    except OSError as error:
        return refuse(f"could not inspect post-checkout without following it: {error}")

    if not stat.S_ISREG(candidate_stat.st_mode) or stat.S_ISLNK(candidate_stat.st_mode):
        return refuse("post-checkout is not an ordinary regular file")
    if candidate_stat.st_uid != dotgit_stat.st_uid:
        return refuse("post-checkout ownership is ambiguous")
    if candidate_stat.st_size > MAX_BYTES:
        return refuse(f"post-checkout exceeds the {MAX_BYTES}-byte read bound")

    nofollow = getattr(os, "O_NOFOLLOW", 0)
    directory = getattr(os, "O_DIRECTORY", 0)
    if not nofollow or not directory:
        return refuse("this platform has no rooted no-follow open primitive")
    directory_flags = os.O_RDONLY | os.O_CLOEXEC | nofollow | directory
    try:
        # Open every parent by descriptor so a replacement of .git or hooks
        # after lstat cannot redirect the candidate read through a symlink.
        root_fd = os.open(root, directory_flags)
        try:
            dotgit_fd = os.open(".git", directory_flags, dir_fd=root_fd)
            try:
                opened_dotgit = os.fstat(dotgit_fd)
                if (opened_dotgit.st_dev, opened_dotgit.st_ino) != (
                    dotgit_stat.st_dev,
                    dotgit_stat.st_ino,
                ):
                    return refuse(".git changed during no-follow inspection")
                hooks_fd = os.open("hooks", directory_flags, dir_fd=dotgit_fd)
                try:
                    opened_hooks = os.fstat(hooks_fd)
                    if (opened_hooks.st_dev, opened_hooks.st_ino) != (
                        hooks_stat.st_dev,
                        hooks_stat.st_ino,
                    ):
                        return refuse(".git/hooks changed during no-follow inspection")
                    fd = os.open(
                        "post-checkout",
                        os.O_RDONLY | os.O_CLOEXEC | nofollow,
                        dir_fd=hooks_fd,
                    )
                finally:
                    os.close(hooks_fd)
            finally:
                os.close(dotgit_fd)
        finally:
            os.close(root_fd)
    except OSError as error:
        return refuse(f"post-checkout changed or could not be opened without following it: {error}")
    try:
        opened = os.fstat(fd)
        if not stat.S_ISREG(opened.st_mode) or opened.st_uid != dotgit_stat.st_uid:
            return refuse("post-checkout changed to an ambiguous path while being read")
        if opened.st_size > MAX_BYTES:
            return refuse(f"post-checkout exceeds the {MAX_BYTES}-byte read bound")
        data = bytearray()
        while len(data) <= MAX_BYTES:
            chunk = os.read(fd, min(8192, MAX_BYTES + 1 - len(data)))
            if not chunk:
                break
            data.extend(chunk)
        if len(data) > MAX_BYTES:
            return refuse(f"post-checkout exceeded the {MAX_BYTES}-byte read bound")
        after = os.fstat(fd)
        if (opened.st_dev, opened.st_ino, opened.st_size, opened.st_mtime_ns) != (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
        ):
            return refuse("post-checkout changed during the bounded read")
    finally:
        os.close(fd)

    digest = hashlib.sha256(data).hexdigest()
    if digest == LEGACY_SHA256:
        say(
            "found the exact legacy checkout hook; it remains active until explicitly removed. "
            f"After reviewing {candidate}, remove only that repository-local regular file "
            "manually (never use this diagnostic as an uninstall step)."
        )
    else:
        say("found a foreign post-checkout file; it was preserved and requires manual review")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
