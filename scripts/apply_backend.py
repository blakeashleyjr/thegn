#!/usr/bin/env python3
"""Apply a backend to this repository and, for Sprites, scaffold its config.

THEGN_APPLY_BACKEND_FAIL and THEGN_APPLY_BACKEND_TOUCH_OVERLAY are test-only
fault-injection hooks. They are not used by normal invocations.
"""

import fcntl
import os
import re
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib


SPRITES_SCAFFOLD = '''\n[env.sprites]\nplacement = "provider"\ndata = "in_env"\n[env.sprites.provider]\nprovider = "sprites"\n# Per-worktree sprite name. Tokens: {repo}=repo name, {worktree}=dir basename,\n# {hash}=stable path digest (collision-defuser), {slug}=full-path. Default is\n# conflict-free across repos; "" is equivalent. A no-token literal = one shared sprite.\nid = "{repo}-{worktree}-{hash}"\napi_key_env = "SPRITES_TOKEN"\nexec = "auto"\nauto_provision = true\nauto_checkpoint = true\n# CLI bridge — only used if you set exec = "cli"; lifecycle is API (auto_provision/auto_checkpoint).\nexec_command = ["sprite", "exec", "-s", "{id}", "--"]\ninteractive_command = ["sprite", "exec", "-s", "{id}", "--tty", "--"]\n'''.encode("utf-8")
BACKENDS = ("sprites", "podman", "docker", "apple", "bwrap", "systemd", "none")


class Refusal(Exception):
    """An expected, user-facing refusal."""


def _read_regular(path: Path, message: str) -> bytes | None:
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except FileNotFoundError:
        return None
    except OSError as error:
        raise Refusal(message) from error
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode):
            raise Refusal(message)
        with os.fdopen(fd, "rb", closefd=False) as stream:
            return stream.read()
    finally:
        os.close(fd)


def _overlay_bytes(path: Path) -> bytes | None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise Refusal(f"{path} is a symlink or non-regular file — refusing to modify it")
    return _read_regular(
        path, f"{path} is a symlink or non-regular file — refusing to modify it"
    )


def _config_bytes(path: Path) -> bytes | None:
    # Match the recipe's `[ -e ]` check: a dangling symlink is treated as absent.
    if not path.exists():
        return None
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    if (
        stat.S_ISLNK(info.st_mode)
        or not stat.S_ISREG(info.st_mode)
        or not os.access(path, os.R_OK)
    ):
        raise Refusal(
            f"{path} is a symlink, unreadable, or non-regular file — refusing to modify it"
        )
    return _read_regular(
        path,
        f"{path} is a symlink, unreadable, or non-regular file — refusing to modify it",
    )


def _lock_config_writer(path: Path) -> int:
    lock_path = path.parent / ".config.toml.lock"
    flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW | os.O_CLOEXEC
    try:
        fd = os.open(lock_path, flags, 0o600)
    except FileExistsError:
        try:
            fd = os.open(lock_path, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
        except OSError as error:
            raise Refusal(f"could not open config writer lock {lock_path}: {error}") from error
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid():
            os.close(fd)
            raise Refusal(f"config writer lock is not an owned regular file: {lock_path}")
    except OSError as error:
        raise Refusal(f"could not create config writer lock {lock_path}: {error}") from error
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as error:
        os.close(fd)
        raise Refusal("config is being written by another thegn writer; retry") from error
    except OSError as error:
        os.close(fd)
        raise Refusal(f"could not lock config writer sidecar {lock_path}: {error}") from error
    return fd


def _stage(directory: Path, prefix: str, data: bytes, mode: int = 0o600) -> Path:
    fd, name = tempfile.mkstemp(dir=directory, prefix=prefix)
    path = Path(name)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
    except BaseException:
        path.unlink(missing_ok=True)
        raise
    return path


def _fsync_dir(directory: Path) -> None:
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _install(staged: Path, destination: Path) -> None:
    os.replace(staged, destination)
    _fsync_dir(destination.parent)


def _cleanup(*paths: Path | None) -> None:
    for path in paths:
        if path is not None:
            path.unlink(missing_ok=True)


def _restore_config(cfg: Path, backup: Path | None, existed: bool) -> None:
    if existed and backup is not None:
        _install(backup, cfg)
    elif not existed:
        cfg.unlink(missing_ok=True)
        _fsync_dir(cfg.parent)


def _repo_root() -> Path:
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            check=True,
            capture_output=True,
            text=True,
        )
    except subprocess.CalledProcessError as error:
        stderr = (error.stderr or "").strip()
        detail = f": {stderr}" if stderr else ""
        raise Refusal(f"could not find the repository root: {error}{detail}") from error
    except OSError as error:
        raise Refusal(f"could not find the repository root: {error}") from error
    common = Path(result.stdout.strip())
    if common.name == ".git":
        return common.parent
    return common


def _apply(backend: str) -> None:
    if backend == "sprites":
        if not os.environ.get("SPRITES_TOKEN"):
            raise Refusal("SPRITES_TOKEN not set — export it before using backend=sprites")
        overlay_body = b'env = "sprites"'
    elif backend in BACKENDS[1:]:
        overlay_body = f'[sandbox]\nbackend = "{backend}"'.encode()
    else:
        raise Refusal(
            f"unknown backend '{backend}' — expected: sprites | podman | docker | apple | bwrap | systemd | none"
        )

    root = _repo_root()
    overlay = root / ".thegn.toml"
    cfg_home = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    cfg = Path(cfg_home) / "thegn" / "config.toml"
    overlay_before = _overlay_bytes(overlay)
    cfg_original = _config_bytes(cfg)
    known = {f'[sandbox]\nbackend = "{name}"'.encode() + b"\n" for name in BACKENDS[1:]}
    known.add(b'env = "sprites"\n')
    if overlay_before is not None and overlay_before not in known:
        raise Refusal(
            f"{overlay} contains user-managed content — edit it by hand to set the backend"
        )
    overlay_current = overlay_before == overlay_body + b"\n"
    if overlay_current and backend != "sprites":
        return

    lock_fd: int | None = None
    cfg_stage: Path | None = None
    cfg_backup: Path | None = None
    overlay_stage: Path | None = None
    cfg_mode = 0o600
    cfg_installed = False
    scaffold = False
    overlay_install_attempted = False
    try:
        if backend == "sprites":
            has_sprites = cfg_original is not None and re.search(
                rb"(?m)^\[env\.sprites\]", cfg_original
            ) is not None
            scaffold = not has_sprites
            if scaffold:
                if cfg_original is not None:
                    try:
                        tomllib.loads(cfg_original.decode("utf-8"))
                    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as error:
                        raise Refusal(f"{cfg} is not valid TOML — not modified") from error
                    cfg_mode = stat.S_IMODE(cfg.stat().st_mode)
                try:
                    cfg.parent.mkdir(parents=True, exist_ok=True)
                except OSError as error:
                    raise Refusal(f"could not create config directory for {cfg}") from error
                lock_fd = _lock_config_writer(cfg)
                cfg_locked = _config_bytes(cfg)
                if cfg_locked != cfg_original:
                    raise Refusal(f"{cfg} changed while staging — not modified")
            if overlay_current and not scaffold:
                return
        if scaffold:
            if shutil.which("sprite") is None:
                print(
                    "note: sprite CLI not on PATH — fine for native exec (the default exec=auto attaches over the WSS API with just SPRITES_TOKEN); the CLI is only the fallback transport",
                    file=sys.stderr,
                )
            if cfg_original is not None:
                try:
                    cfg_backup = _stage(cfg.parent, ".config.toml.restore.", cfg_original, cfg_mode)
                    shutil.copystat(cfg, cfg_backup)
                except OSError as error:
                    raise Refusal(f"could not stage a restore copy of {cfg}") from error
            try:
                cfg_stage = _stage(
                    cfg.parent,
                    ".config.toml.",
                    (cfg_original or b"") + SPRITES_SCAFFOLD,
                )
            except OSError as error:
                raise Refusal(f"could not stage {cfg}") from error
            try:
                tomllib.loads(cfg_stage.read_bytes().decode("utf-8"))
            except (tomllib.TOMLDecodeError, UnicodeDecodeError) as error:
                raise Refusal(
                    f"{cfg} is not valid TOML after adding [env.sprites] — not modified"
                ) from error

        if not overlay_current:
            try:
                overlay_stage = _stage(root, ".thegn.toml.", overlay_body + b"\n")
            except OSError as error:
                raise Refusal(f"could not stage {overlay}") from error
        if scaffold:
            cfg_now = _config_bytes(cfg)
            if cfg_now != cfg_original:
                raise Refusal(f"{cfg} changed while staging — not modified")
            os.replace(cfg_stage, cfg)
            cfg_stage = None
            cfg_installed = True
            _fsync_dir(cfg.parent)
            print(f"scaffolded [env.sprites] into {cfg}")

        if (
            overlay_stage is not None
            and os.environ.get("THEGN_APPLY_BACKEND_TOUCH_OVERLAY") == "1"
        ):
            with open(overlay, "ab") as stream:
                stream.write(b"# concurrent edit\n")
                stream.flush()
                os.fsync(stream.fileno())
        if overlay_stage is not None:
            try:
                overlay_now = _overlay_bytes(overlay)
            except Refusal:
                overlay_now = object()
            if overlay_now != overlay_before:
                if cfg_installed:
                    _restore_config(cfg, cfg_backup, cfg_original is not None)
                    cfg_installed = False
                raise Refusal(f"{overlay} changed while staging — not modified")

            overlay_install_attempted = True
            if os.environ.get("THEGN_APPLY_BACKEND_FAIL") == "overlay-install":
                raise OSError("injected overlay install failure")
            os.replace(overlay_stage, overlay)
            overlay_stage = None
            try:
                if os.environ.get("THEGN_APPLY_BACKEND_FAIL") == "overlay-dir-fsync":
                    raise OSError("injected overlay directory fsync failure")
                _fsync_dir(overlay.parent)
            except OSError as error:
                print(
                    "warning: overlay was installed but its directory could not be synced: "
                    f"{error}",
                    file=sys.stderr,
                )
            print(f"set {overlay} -> {overlay_body.decode()} (applies to all worktrees under {root})")
    except Refusal:
        if cfg_installed:
            _restore_config(cfg, cfg_backup, cfg_original is not None)
        raise
    except OSError as error:
        if cfg_installed:
            try:
                _restore_config(cfg, cfg_backup, cfg_original is not None)
            except OSError as restore_error:
                raise Refusal(
                    f"could not install staged overlay {overlay} or restore {cfg}: {restore_error}"
                ) from error
        if scaffold and not overlay_install_attempted:
            raise Refusal(f"could not install staged config {cfg}") from error
        if scaffold:
            raise Refusal(
                f"could not install staged overlay {overlay}; restored the prior config state"
            ) from error
        raise Refusal(
            f"could not install staged overlay {overlay}; restored the prior config state"
        ) from error
    finally:
        _cleanup(cfg_stage, cfg_backup, overlay_stage)
        if lock_fd is not None:
            os.close(lock_fd)


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if not args or args[0] == "":
        return 0
    try:
        _apply(args[0])
    except Refusal as error:
        print(error, file=sys.stderr)
        return 1
    except OSError as error:
        print(f"could not apply backend: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
