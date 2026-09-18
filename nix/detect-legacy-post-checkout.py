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
import selectors
import signal
import stat
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path


LEGACY_SHA256 = "90b85945a3c30c3a9aa806f87fe9585e9ce660e450672e3e5e24b40dca140365"
MAX_BYTES = 64 * 1024
GIT_TIMEOUT_SECONDS = 2.0
MAX_GIT_OUTPUT = 64 * 1024
DIRECT_REAP_TIMEOUT_SECONDS = 0.5

# Keep normal user configuration discovery intact, including HOME/XDG and the
# configured global file. These variables can instead retarget the repository,
# object database, discovery, or command-local config and must not cross the
# detector boundary.
GIT_REDIRECT_ENV = frozenset(
    {
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    }
)


class GitQueryRefusal(Exception):
    """A bounded Git query could not produce a safe, complete answer."""


@dataclass(frozen=True)
class GitResult:
    returncode: int
    stdout: bytes
    stderr: bytes


def say(message: str) -> None:
    print(f"thegn legacy checkout-hook detector: {message}", file=sys.stderr)


def unsupported_platform_reason() -> str | None:
    """Return a refusal reason before any subprocess is created."""

    if os.name != "posix":
        return "this detector supports only POSIX process groups and no-follow primitives"
    required = ("O_NOFOLLOW", "O_DIRECTORY", "O_NONBLOCK", "O_CLOEXEC", "killpg")
    if any(not getattr(os, name, 0) for name in required[:-1]) or not hasattr(os, "killpg"):
        return "this platform lacks the required POSIX process-group/no-follow primitives"
    return None


def terminate_process_group(process: subprocess.Popen[bytes]) -> None:
    """Kill only the isolated Git process group, never an ambient group."""

    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, OSError):
        # The group may have exited between poll and cleanup. Direct reaping
        # below is still required to avoid leaving the Git leader a zombie.
        pass


def close_pipes(process: subprocess.Popen[bytes]) -> None:
    for stream in (process.stdout, process.stderr):
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass


def reap_direct(process: subprocess.Popen[bytes]) -> None:
    """Reap the direct leader with a finite bound after group termination."""

    try:
        process.wait(timeout=DIRECT_REAP_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        # A process that ignored SIGKILL is not expected, but retain a bounded
        # direct-child fallback. This does not broaden the process-group scope.
        try:
            process.kill()
        except OSError:
            pass
        try:
            process.wait(timeout=DIRECT_REAP_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            pass


def git(root: Path, *args: str) -> GitResult:
    """Run one non-interactive Git query with bounded time and output."""

    if (reason := unsupported_platform_reason()) is not None:
        raise GitQueryRefusal(reason)
    query_env = os.environ.copy()
    for variable in list(query_env):
        if variable in GIT_REDIRECT_ENV or variable.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_")):
            query_env.pop(variable, None)
    query_env.update(
        {
            "GIT_EDITOR": ":",
            "GIT_PAGER": "cat",
            "GIT_SEQUENCE_EDITOR": ":",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    try:
        process = subprocess.Popen(
            ["git", *args],
            cwd=root,
            env=query_env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=(os.name != "nt"),
            creationflags=(subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0),
        )
    except OSError as error:
        raise GitQueryRefusal(f"Git could not be started: {error}") from error

    assert process.stdout is not None
    assert process.stderr is not None
    stdout_fd = process.stdout.fileno()
    stderr_fd = process.stderr.fileno()
    streams = {stdout_fd: bytearray(), stderr_fd: bytearray()}
    selector = selectors.DefaultSelector()
    deadline = time.monotonic() + GIT_TIMEOUT_SECONDS
    refusal: str | None = None
    try:
        for stream in (process.stdout, process.stderr):
            os.set_blocking(stream.fileno(), False)
            selector.register(stream, selectors.EVENT_READ)
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                refusal = f"Git query timed out after {GIT_TIMEOUT_SECONDS:g}s"
                break
            for key, _ in selector.select(remaining):
                fd = key.fileobj.fileno()
                captured = sum(len(stream) for stream in streams.values())
                chunk = os.read(fd, min(8192, MAX_GIT_OUTPUT + 1 - captured))
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                streams[fd].extend(chunk)
                if sum(len(stream) for stream in streams.values()) > MAX_GIT_OUTPUT:
                    refusal = f"Git query exceeded the {MAX_GIT_OUTPUT}-byte output bound"
                    break
            if refusal is not None:
                break
    except OSError as error:
        refusal = f"Git output could not be captured safely: {error}"
    finally:
        streams_drained = not selector.get_map()
        selector.close()
        if refusal is None and streams_drained:
            # EOF can be observed just before waitpid reports the leader's
            # exit. Give the successful child the remaining bounded deadline;
            # do not mistake that small race for a hung Git process.
            remaining = max(0.0, deadline - time.monotonic())
            try:
                process.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                refusal = f"Git query timed out after {GIT_TIMEOUT_SECONDS:g}s"
        if refusal is not None:
            terminate_process_group(process)
        close_pipes(process)
        if refusal is not None:
            reap_direct(process)
        elif process.poll() is None:
            # The normal path above normally reaps the leader. Keep this
            # defensive branch bounded if a platform reports a stale poll.
            reap_direct(process)

    if refusal is not None:
        raise GitQueryRefusal(refusal)
    return GitResult(process.returncode, bytes(streams[stdout_fd]), bytes(streams[stderr_fd]))


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


def command_scope_hooks_path() -> bool:
    """Return whether inherited Git command-scope config may set hooksPath."""

    count = os.environ.get("GIT_CONFIG_COUNT")
    if count is not None:
        try:
            count_value = int(count)
        except ValueError:
            return True
        if count_value < 0:
            return True
        entries = range(count_value)
        for index in entries:
            key = os.environ.get(f"GIT_CONFIG_KEY_{index}")
            if key is None:
                return True
            if key.casefold() == "core.hookspath":
                return True
    parameters = os.environ.get("GIT_CONFIG_PARAMETERS", "")
    return "core.hookspath" in parameters.casefold()


def one_line(data: bytes, description: str) -> str:
    """Decode one Git path/value without lossy Unicode conversion."""

    if b"\0" in data:
        raise GitQueryRefusal(f"Git returned an unsupported NUL in {description}")
    line = data[:-1] if data.endswith(b"\n") else data
    if b"\n" in line or b"\r" in line:
        raise GitQueryRefusal(f"Git returned ambiguous {description} output")
    return os.fsdecode(line)


def main() -> int:
    try:
        if (reason := unsupported_platform_reason()) is not None:
            return refuse(reason)
        cwd = Path.cwd()
        if command_scope_hooks_path():
            return refuse("inherited command-scope core.hooksPath is ambiguous")
        top = git(cwd, "rev-parse", "--show-toplevel")
        if top.returncode != 0 or not top.stdout.strip():
            return refuse("the current directory is not a repository")
        root = Path(one_line(top.stdout, "repository root")).resolve()

        if git(root, "rev-parse", "--is-bare-repository").stdout.strip() != b"false":
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
            path = Path(os.fsdecode(line))
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
            configured_path = Path(os.fsdecode(local_values[0]))
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
        if worktree_config.stdout.strip().lower() in {b"true", b"yes", b"on", b"1"}:
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
        nonblock = getattr(os, "O_NONBLOCK", 0)
        cloexec = getattr(os, "O_CLOEXEC", 0)
        if not all((nofollow, directory, nonblock, cloexec)):
            return refuse("this platform has no rooted no-follow nonblocking open primitive")
        directory_flags = os.O_RDONLY | cloexec | nofollow | directory
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
                            os.O_RDONLY | cloexec | nofollow | nonblock,
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
            if (opened.st_dev, opened.st_ino) != (candidate_stat.st_dev, candidate_stat.st_ino):
                return refuse("post-checkout was replaced during no-follow inspection")
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
    except GitQueryRefusal as error:
        return refuse(str(error))


if __name__ == "__main__":
    raise SystemExit(main())
