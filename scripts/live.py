#!/usr/bin/env python3
"""Stepped Linux upgrade of the real instance: build, validate, stop, back up,
install, launch. Never a readiness protocol.

Assumes a stable current-user namespace, not hostile same-UID/root processes.
/proc observations and cooperative flocks cannot exclude legacy restarts. SQLite
backup deadlines are checked between backup steps, not cancellation of kernel I/O.
The installation lives OUTSIDE the checkout (default ~/.local/lib/thegn, from
the passwd home), so a release build in the repo never replaces it. Only this
installation's processes are signalled: its binary under its state home, or a
`thegn` executable under its state home. Other installations (another binary
and another state home, or this binary under another state home) are reported
and left alone; a process that cannot be attributed is never signalled and
blocks the upgrade. Launchers in ~/.local/bin change only with
--relink-launchers.
"""

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import tomllib

if sys.platform == "linux":
    import fcntl

MAX_CONFIG = 1024 * 1024
BACKUP_SECONDS = 60
BUILD = ["cargo", "build", "--locked", "--release", "--features", "profiling", "-p", "thegn-host", "--bin", "thegn"]
LIVE_STAGE_PREFIX = ".thegn-live-build-"
LIVE_CACHE = "live-cache"
LIVE_STAGE_MARKER = ".thegn-live-build.marker"
LIVE_STAGE_LOCK = ".thegn-live-build.lock"
MAX_RETAINED_LIVE_STAGES = 2
MAX_STAGE_ENTRIES = 100_000
STOP_COUNTDOWN = 5          # seconds to Ctrl-C before anything is stopped
CONTROLLER_GRACE = 10       # SIGTERM'd UI controllers persist their session and exit
DAEMON_STOP_SECONDS = 20    # `thegn daemon stop` round-trip bound
DAEMON_GRACE = 15           # daemon drains its panes after the shutdown request
KILL_GRACE = 5              # after SIGKILL of the exact-target leftovers
DOCTOR_DELAY = 20           # let the new controller finish startup/migration first
STEPS = 6
THEGN_NAMES = ("thegn", "tg")
DEFAULT_INSTALL = ".local/lib/thegn"   # under the passwd home, never $HOME
INSTALL_ENV = "THEGN_LIVE_INSTALL_DIR"
LAUNCHER_DIR = ".local/bin"
WRAPPERS = ("tg", "tg-tui")
MAX_WRAPPER = 1024 * 1024


class Refusal(Exception):
    """Visible refusal; no automatic rollback or process signalling."""


def absolute(value):
    if not value or len(value) > 4096 or any(ord(c) < 32 for c in value):
        raise Refusal("Unsupported empty, oversized or control-containing path")
    path = Path(value)
    if not path.is_absolute() or ".." in value.split("/") or "." in value.split("/"):
        raise Refusal("Paths must be absolute, without dot components")
    return path


def regular(path, private=False, executable=False):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
        raise Refusal("Expected a current-user regular file (no symlinks)")
    if info.st_mode & (0o077 if private else 0o022):
        raise Refusal("File permissions are too broad")
    if executable and not info.st_mode & stat.S_IXUSR:
        raise Refusal("Selected artifact is not executable")
    return info


def directories(path, private=False):
    """Reject aliases and ordinary writable ancestors; root-owned /tmp is allowed.

    This is a stable-namespace check, not an immutable descriptor-bound lease.
    """
    for parent in reversed((path, *path.parents)):
        info = parent.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid not in (0, os.getuid()):
            raise Refusal("Unsupported directory owner/type or symlink ancestor")
        sticky_root = info.st_uid == 0 and info.st_mode & stat.S_ISVTX
        if info.st_mode & 0o022 and not sticky_root:
            raise Refusal("Directory is writable by another user/group")
    info = path.stat()
    if info.st_uid != os.getuid() or info.st_mode & (0o077 if private else 0o022):
        raise Refusal("Final directory must be current-user owned with private permissions")


def read_small(path, maximum=MAX_CONFIG):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > maximum:
            raise Refusal("Expected bounded regular input")
        data = stream.read(maximum + 1)
        if len(data) > maximum:
            raise Refusal("Input exceeds size limit")
        return data


def read_fd_small(fd, maximum):
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_size > maximum:
        raise Refusal("Expected bounded regular input")
    data = bytearray()
    while len(data) <= maximum:
        chunk = os.read(fd, maximum + 1 - len(data))
        if not chunk:
            return bytes(data)
        data.extend(chunk)
    raise Refusal("Input exceeds size limit")


def real_home():
    """The account's home from the passwd database.

    Not $HOME: this tool is sometimes run from a shell whose HOME is a profile
    directory, and the stable installation and launchers belong to the account.
    """
    return absolute(pwd.getpwuid(os.getuid()).pw_dir)


def install_dir_for(env, override=None):
    """--install-dir, else $THEGN_LIVE_INSTALL_DIR, else <passwd home>/.local/lib/thegn."""
    raw = str(override) if override else env.get(INSTALL_ENV, "").strip()
    return absolute(raw) if raw else real_home() / DEFAULT_INSTALL


def _nearest_existing(path):
    for candidate in (path, *path.parents):
        if os.path.lexists(candidate):
            return candidate
    return Path("/")


def settings(repo, env, install_override=None):
    if sys.platform != "linux" or os.getuid() == 0 or os.geteuid() != os.getuid():
        raise Refusal("This helper supports ordinary-user Linux upgrades only")
    user_home_path = absolute(env.get("HOME", "/"))
    roots = {key: absolute(env[key]) for key in ("XDG_STATE_HOME", "XDG_CONFIG_HOME", "THEGN_DIR", "XDG_RUNTIME_DIR") if key in env}
    state = roots.get("XDG_STATE_HOME", user_home_path / ".local/state") / "thegn"
    config = roots.get("XDG_CONFIG_HOME", user_home_path / ".config") / "thegn/config.toml"
    if env.get("THEGN_PROFILE", "") not in ("", "default"):
        raise Refusal("Named profiles are unsupported; use the ordinary launcher")
    if env.get("THEGN_ALLOW_SCHEMA_DOWNGRADE", ""):
        raise Refusal("Schema downgrade opt-in is unsupported")
    directories(state, private=True)
    install_dir = install_dir_for(env, install_override)
    account_state = real_home() / ".local/state"
    if not install_override and not env.get(INSTALL_ENV, "").strip() and state.parent != account_state:
        # A profile HOME without XDG_STATE_HOME would "upgrade" the profile's
        # state while replacing the account's binary under its running instance.
        raise Refusal(f"State home {state.parent} is not the account's state home {account_state}, but the default "
                      f"install directory {install_dir} is the account's; set XDG_STATE_HOME to the account's state "
                      "home or pass --install-dir")
    if install_dir == repo or repo in install_dir.parents:
        raise Refusal(f"Install directory {install_dir} is inside the checkout {repo}; a release build there "
                      "would replace the installation. Choose a directory outside it")
    if os.path.lexists(install_dir):
        directories(install_dir)
        existing = install_dir
    else:
        existing = _nearest_existing(install_dir)
        directories(existing)  # Created only by a real run.
    if os.statvfs(existing).f_flag & os.ST_NOEXEC:
        raise Refusal(f"Install directory {install_dir} is on a noexec filesystem ({existing}); the installed "
                      "binary could not be launched. Choose another directory")
    target, database = install_dir / "thegn", state / "thegn.db"
    alacritty, alacritty_source = install_dir / "alacritty.toml", repo / "config/alacritty.toml"
    legacy = repo / "target/release/thegn"
    if os.path.lexists(target):
        regular(target, executable=True)
        previous = target
    elif os.path.lexists(legacy):
        # First run after the move out of the checkout: the in-repo binary is
        # the installation being upgraded (process attribution and backup).
        directories(legacy.parent)
        regular(legacy, executable=True)
        previous = legacy
    else:
        raise Refusal(f"No installed executable at {target} (or the legacy {legacy}); this is not a first-install command")
    if os.path.lexists(alacritty):
        info = alacritty.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
            raise Refusal(f"Installed {alacritty} is not a current-user regular file")
    regular(alacritty_source)
    if regular(database, private=True).st_nlink != 1:
        raise Refusal("Hardlinked database paths use distinct schema locks and are unsupported")
    raw = b""
    try:
        directories(config.parent)
        regular(config)
        raw = read_small(config)
    except FileNotFoundError:
        # Only genuinely absent configuration is optional. A dangling symlink
        # fails O_NOFOLLOW; other inspection/read errors remain refusals.
        if config.is_symlink():
            raise Refusal("Config symlink is unsupported") from None
    try:
        document = tomllib.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeError):
        raise Refusal("Configuration is not valid bounded UTF-8 TOML") from None
    if document.get("profile", "") not in ("", "default"):
        raise Refusal("Config-selected profiles are unsupported")
    db = document.get("database", {})
    keys = {"migration_authority", "migration_executable"}
    if not isinstance(db, dict) or set(db) - keys:
        raise Refusal("Unsupported database configuration fields")
    allowed_env = {"THEGN_DATABASE_MIGRATION_AUTHORITY", "THEGN_DATABASE_MIGRATION_EXECUTABLE"}
    if any(key.startswith("THEGN_DATABASE_") and key not in allowed_env for key in env):
        raise Refusal("Unsupported database environment override")
    # ProcessEnv ignores blank strings; XDG var_os above deliberately does not.
    def override(key, default):
        value = env.get(key, "")
        return value if value.strip() else default

    authority = override("THEGN_DATABASE_MIGRATION_AUTHORITY", db.get("migration_authority", "controller"))
    if authority not in ("controller", "any"):
        raise Refusal("Migration authority must be canonical controller or any")
    check_migration_pin(env, db, config, target)
    return {"repo": repo, "state": state, "database": database, "target": target, "config": config,
            "install_dir": install_dir, "previous": previous, "alacritty": alacritty,
            "alacritty_source": alacritty_source}


PIN_ENV = "THEGN_DATABASE_MIGRATION_EXECUTABLE"


def check_migration_pin(env, db, config, target):
    """A migration-executable pin must resolve to the install target.

    The refusal names where the pin comes from, its value and the fix. The
    environment wins over the config file when it is not blank, as in thegn.
    """
    raw_env = env.get(PIN_ENV, "")
    if raw_env.strip():
        pin, origin = raw_env, f"The environment variable {PIN_ENV}"
    else:
        pin, origin = db.get("migration_executable", ""), f"`[database] migration_executable` in {config}"
    fix = (f"Set it to the install target {target} (after the first install has put a binary there), "
           "or remove it")
    if not isinstance(pin, str):
        raise Refusal(f"{origin} is {pin!r}, which is not a path. {fix}")
    if not pin:
        return
    try:
        resolved = absolute(pin).resolve(strict=True)
    except Refusal as error:
        raise Refusal(f"{origin} is {pin!r}, which is not a usable path ({error}). {fix}") from None
    except FileNotFoundError:
        raise Refusal(f"{origin} is {pin!r}, which does not exist. {fix}") from None
    except (OSError, RuntimeError) as error:
        raise Refusal(f"{origin} is {pin!r}, which cannot be resolved ({error}). {fix}") from None
    if resolved != target:
        raise Refusal(f"{origin} is {pin!r}, which resolves to {resolved}, not the install target. {fix}")


@contextlib.contextmanager
def locked(path, schema=False):
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & (0o022 if schema else 0o077):
            raise Refusal("Unsafe lock file")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise Refusal("Upgrade/schema lock busy; stop controllers and disable restarts manually") from None
        yield
    finally:
        os.close(fd)


def identity(path):
    info = path.stat()
    return info.st_dev, info.st_ino


def _validate_stage_tree(fd, count):
    """Validate a stage without following links before descriptor deletion."""
    for name in os.listdir(fd):
        count[0] += 1
        if count[0] > MAX_STAGE_ENTRIES:
            return False
        try:
            info = os.stat(name, dir_fd=fd, follow_symlinks=False)
        except FileNotFoundError:
            return False
        if info.st_uid != os.getuid():
            return False
        if stat.S_ISDIR(info.st_mode):
            try:
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            except OSError:
                return False
            try:
                if not _validate_stage_tree(child, count):
                    return False
            finally:
                os.close(child)
        elif not stat.S_ISREG(info.st_mode):
            return False
    return True


def _remove_stage_tree(fd, count=None):
    for name in os.listdir(fd):
        if count is not None:
            count[0] += 1
            if count[0] > MAX_STAGE_ENTRIES:
                raise Refusal("stage cleanup bound exceeded")
        info = os.stat(name, dir_fd=fd, follow_symlinks=False)
        if stat.S_ISDIR(info.st_mode):
            child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            try:
                _remove_stage_tree(child, count)
            finally:
                os.close(child)
            os.rmdir(name, dir_fd=fd)
        elif stat.S_ISREG(info.st_mode):
            os.unlink(name, dir_fd=fd)
        else:
            raise Refusal("stage changed to an unsupported entry during cleanup")


def _recovery_linked_stages(paths):
    """Find stage paths named by bounded recovery metadata, if any."""
    linked = set()

    def strings(value, depth=0):
        if depth > 12:
            return
        if isinstance(value, str):
            yield value
        elif isinstance(value, dict):
            for child in value.values():
                yield from strings(child, depth + 1)
        elif isinstance(value, list):
            for child in value:
                yield from strings(child, depth + 1)

    stage_prefix = str(paths["repo"] / "target" / LIVE_STAGE_PREFIX)
    try:
        with os.scandir(paths["state"]) as entries:
            for index, entry in enumerate(entries, 1):
                if index > MAX_STAGE_ENTRIES:
                    return None
                if not entry.name.startswith("live-backup-"):
                    continue
                try:
                    is_directory = entry.is_dir(follow_symlinks=False)
                except OSError:
                    return None
                if not is_directory:
                    continue
                complete = Path(entry.path) / "complete.json"
                try:
                    document = json.loads(read_small(complete, 256 * 1024))
                except (FileNotFoundError, OSError, Refusal, ValueError, UnicodeError):
                    # A missing or malformed recovery descriptor may still refer to a
                    # live artifact. Preserve all stages until an operator resolves it.
                    return None
                for value in strings(document):
                    if value.startswith(stage_prefix):
                        # A recovery descriptor can name either the stage
                        # itself or an artifact inside it. Protect the root.
                        stage_name = Path(value).relative_to(paths["repo"] / "target").parts[0]
                        linked.add(str(paths["repo"] / "target" / stage_name))
                        if len(linked) > MAX_STAGE_ENTRIES:
                            return None
    except OSError:
        return None
    return linked


def _open_stage_candidate(target_fd, name, expected_inode):
    """Open and lock one stage, binding later checks to its descriptors."""
    stage_fd = None
    lock_fd = None
    success = False
    try:
        stage_fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=target_fd)
        info = os.fstat(stage_fd)
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077
                or expected_inode not in (None, (info.st_dev, info.st_ino))):
            return None
        marker_fd = os.open(LIVE_STAGE_MARKER, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=stage_fd)
        try:
            marker_info = os.fstat(marker_fd)
            if (not stat.S_ISREG(marker_info.st_mode) or marker_info.st_uid != os.getuid()
                    or marker_info.st_nlink != 1 or marker_info.st_mode & 0o077):
                return None
            if read_fd_small(marker_fd, 128) != b"thegn-live-build-v1\n":
                return None
        finally:
            os.close(marker_fd)
        lock_fd = os.open(LIVE_STAGE_LOCK, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=stage_fd)
        lock_info = os.fstat(lock_fd)
        if (not stat.S_ISREG(lock_info.st_mode) or lock_info.st_uid != os.getuid()
                or lock_info.st_nlink != 1 or lock_info.st_mode & 0o077):
            return None
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return None
        success = True
        return stage_fd, lock_fd, (info.st_dev, info.st_ino), info.st_mtime_ns
    except (FileNotFoundError, OSError, Refusal):
        return None
    finally:
        if not success and lock_fd is not None:
            os.close(lock_fd)
        if not success and stage_fd is not None:
            os.close(stage_fd)


def _stage_name_identity(target_fd, name, expected_identity):
    """Verify that the parent entry still names the opened stage directory."""
    try:
        info = os.stat(name, dir_fd=target_fd, follow_symlinks=False)
    except OSError:
        return False
    return (stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
            and not info.st_mode & 0o077
            and (info.st_dev, info.st_ino) == expected_identity)


def retain_live_stages(paths, current=None):
    """Keep the newest owned stages; preserve unverifiable entries."""
    target = paths["repo"] / "target"
    linked = _recovery_linked_stages(paths)
    if linked is None:
        return 0
    target_fd = None
    try:
        target_fd = os.open(target, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        target_info = os.fstat(target_fd)
        if not stat.S_ISDIR(target_info.st_mode) or target_info.st_uid != os.getuid():
            return 0
        metadata = []
        with os.scandir(target_fd) as entries:
            for index, entry in enumerate(entries, 1):
                if index > MAX_STAGE_ENTRIES:
                    return 0
                if (not entry.name.startswith(LIVE_STAGE_PREFIX)
                        or (current is not None and target / entry.name == current)):
                    continue
                try:
                    if not entry.is_dir(follow_symlinks=False):
                        continue
                    info = entry.stat(follow_symlinks=False)
                    metadata.append((info.st_mtime_ns, entry.name, (info.st_dev, info.st_ino)))
                except OSError:
                    continue
        metadata.sort(key=lambda candidate: (candidate[0], candidate[1]), reverse=True)
        removed = 0
        eligible = 0
        for _mtime, name, stage_identity in metadata:
            stage = target / name
            if str(stage) in linked:
                continue
            candidate = _open_stage_candidate(target_fd, name, stage_identity)
            if candidate is None:
                continue
            stage_fd, lock_fd, opened_identity, _opened_mtime = candidate
            try:
                if opened_identity != stage_identity or not _stage_name_identity(target_fd, name, opened_identity):
                    continue
                if eligible < MAX_RETAINED_LIVE_STAGES:
                    eligible += 1
                    continue
                count = [0]
                if not _validate_stage_tree(stage_fd, count):
                    continue
                if not _stage_name_identity(target_fd, name, opened_identity):
                    continue
                _remove_stage_tree(stage_fd, [0])
                if not _stage_name_identity(target_fd, name, opened_identity):
                    continue
                os.rmdir(name, dir_fd=target_fd)
                removed += 1
            except (FileNotFoundError, OSError, Refusal):
                # A changed or unverifiable candidate stays for manual inspection.
                continue
            finally:
                os.close(lock_fd)
                os.close(stage_fd)
        return removed
    except (FileNotFoundError, OSError):
        return 0
    finally:
        if target_fd is not None:
            os.close(target_fd)


BACKUP_KEEP = 3
BACKUP_KEEP_DAYS = 7
BACKUP_PREFIX = "live-backup-"
MAX_BACKUP_DESCRIPTOR = 256 * 1024


def _backup_completed_at(state_fd, name):
    """mtime of a well-formed complete.json in one backup dir, else None.

    Opened through descriptors without following links, so a symlinked dir or
    descriptor never counts as complete (and is therefore never removed)."""
    dir_fd = None
    file_fd = None
    try:
        dir_fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=state_fd)
        file_fd = os.open("complete.json", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=dir_fd)
        info = os.fstat(file_fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
            return None
        document = json.loads(read_fd_small(file_fd, MAX_BACKUP_DESCRIPTOR))
        if not isinstance(document, dict) or not all(
                isinstance(document.get(key), str) for key in ("binary_sha256", "database_sha256")):
            return None
        return info.st_mtime
    except (OSError, Refusal, ValueError, UnicodeError):
        return None
    finally:
        if file_fd is not None:
            os.close(file_fd)
        if dir_fd is not None:
            os.close(dir_fd)


def prune_backups(paths, current, now=time.time, keep=BACKUP_KEEP, keep_days=BACKUP_KEEP_DAYS):
    """Delete old live-backup-* dirs in paths["state"].

    Keep: `current`, the newest `keep` dirs that have a well-formed complete.json,
    every dir younger than keep_days, and every dir without a readable
    complete.json (failed or in-progress: it may be the only copy). Never follow
    symlinks; only real dirs owned by this uid. Removal reuses the existing
    bounded _remove_stage_tree walker (MAX_STAGE_ENTRIES), so a huge tree
    aborts instead of blocking. Returns the removed paths.

    A backup's age is the mtime of its complete.json (written once, at the end
    of the backup), not the dir's, which later doctor/stderr files touch."""
    state = paths["state"]
    current_name = Path(current).name if current is not None and Path(current).parent == state else None
    removed = []
    state_fd = None
    try:
        state_fd = os.open(state, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        state_info = os.fstat(state_fd)
        if not stat.S_ISDIR(state_info.st_mode) or state_info.st_uid != os.getuid():
            return removed
        complete = []
        with os.scandir(state_fd) as entries:
            for index, entry in enumerate(entries, 1):
                if index > MAX_STAGE_ENTRIES:
                    return removed
                if not entry.name.startswith(BACKUP_PREFIX) or entry.name == current_name:
                    continue
                try:
                    info = entry.stat(follow_symlinks=False)
                except OSError:
                    continue
                if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
                    continue
                completed = _backup_completed_at(state_fd, entry.name)
                if completed is None:
                    continue  # Failed or in progress: possibly the only copy.
                complete.append((completed, entry.name, (info.st_dev, info.st_ino)))
        complete.sort(key=lambda candidate: (candidate[0], candidate[1]), reverse=True)
        cutoff = now() - keep_days * 86400
        for completed, name, expected in complete[keep:]:
            if completed > cutoff:
                continue
            backup_fd = None
            try:
                backup_fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=state_fd)
                info = os.fstat(backup_fd)
                if (info.st_dev, info.st_ino) != expected or info.st_uid != os.getuid():
                    continue
                if not _validate_stage_tree(backup_fd, [0]):
                    continue
                _remove_stage_tree(backup_fd, [0])
                again = os.stat(name, dir_fd=state_fd, follow_symlinks=False)
                if not stat.S_ISDIR(again.st_mode) or (again.st_dev, again.st_ino) != expected:
                    continue
                os.rmdir(name, dir_fd=state_fd)
                removed.append(state / name)
            except (FileNotFoundError, OSError, Refusal):
                # A changed, oversized or unverifiable backup stays for an operator.
                continue
            finally:
                if backup_fd is not None:
                    os.close(backup_fd)
    except OSError:
        return removed
    finally:
        if state_fd is not None:
            os.close(state_fd)
    return removed


SCOPE_PROBE_SECONDS = 5


def scope_argv(target, env, which=shutil.which, run=subprocess.run):
    """["systemd-run", "--user", "--scope", "--quiet", "--collect",
        "-p", "CPUWeight=1000", "-p", "IOWeight=1000", "--", target] when
    systemd-run exists and `systemd-run --user --scope --quiet true` succeeds
    within 5 s; otherwise [target].

    Why the weight helps: launched from a terminal, the UI shares that
    terminal's cgroup with whatever builds run beside it (this very helper's
    cargo, `just` recipes). Its own scope with CPU and IO weight 1000 (default
    100) wins contention against them. Pane children that join `thegn.slice`
    (weight 50) are unaffected; uncapped panes inherit the scope. `--scope`
    execs in place, so the launched PID is thegn's own."""
    target = str(target)
    systemd_run = which("systemd-run", path=env.get("PATH"))
    if not systemd_run:
        return [target]
    try:
        probe = run([systemd_run, "--user", "--scope", "--quiet", "true"], env=env, stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=SCOPE_PROBE_SECONDS, check=False)
    except (OSError, subprocess.SubprocessError):
        return [target]
    if probe.returncode != 0:
        return [target]
    return [systemd_run, "--user", "--scope", "--quiet", "--collect",
            "-p", "CPUWeight=1000", "-p", "IOWeight=1000", "--", target]


def step(number, text):
    print(f"\n==> [{number}/{STEPS}] {text}", flush=True)


def _status_field(status_text, field):
    for line in status_text.splitlines():
        if line.startswith(field + ":"):
            parts = line.split(maxsplit=1)
            return parts[1].strip() if len(parts) == 2 else ""
    return ""


def thegn_ancestor(proc=Path("/proc"), start=None):
    """The pid of a `thegn`/`tg` ancestor of this process, or None.

    Stopping the instance would kill a shell running inside one of its panes,
    and with it this upgrade halfway through.
    """
    pid = os.getppid() if start is None else start
    for _ in range(64):
        if pid <= 1:
            return None
        try:
            status_text = read_small(proc / str(pid) / "status", 65536).decode("ascii", "replace")
        except (OSError, Refusal):
            return None
        if comm(status_text) in ("thegn", "tg"):
            return pid
        try:
            pid = int(_status_field(status_text, "PPid"))
        except ValueError:
            return None
    return None


def relaunch_outside(argv, env, repo):
    """Re-run this upgrade in a fresh terminal that does not belong to thegn.

    The new window gets its own session (setsid) and, where available, its own
    systemd scope, so neither the pane's process group nor its cgroup takes it
    down when the daemon stops.
    """
    terminal = shutil.which("ghostty", path=env.get("PATH"))
    if terminal is None:
        raise Refusal("This shell runs inside thegn, which the upgrade must stop; run `just live` "
                      "from a terminal outside thegn (ghostty was not found to open one)")
    child_env = {key: value for key, value in env.items() if key not in ("THEGN_SESSION_ID", "THEGN_CONTROL_SOCKET")}
    command = [terminal, "--config-default-files=false", f"--config-file={repo / 'config/ghostty.config'}",
               "--wait-after-command=true", "-e", sys.executable, "-B", str(Path(__file__).resolve()), *argv]
    systemd_run = shutil.which("systemd-run", path=env.get("PATH"))
    if systemd_run is not None:
        command = [systemd_run, "--user", "--scope", "--collect", "--quiet", *command]
    subprocess.Popen(command, cwd=repo, env=child_env, start_new_session=True,
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print("This shell runs inside thegn, which the upgrade has to stop.")
    print("Continuing in a new terminal window; this one can be closed.")


def validate_config(binary, env, run=subprocess.run):
    """Fail before anything is stopped if the NEW build rejects the config."""
    try:
        result = run([str(binary), "config", "validate"], env=env, stdout=subprocess.PIPE,
                     stderr=subprocess.STDOUT, timeout=60)
    except (OSError, subprocess.SubprocessError) as error:
        raise Refusal(f"Could not run the new build's config validation: {error}") from None
    lines = [line for line in result.stdout.decode("utf-8", "replace").splitlines()
             if line.strip() and not line.startswith(("thegn: config:", "WARN "))]
    for line in lines[-10:]:
        print(f"  {line}")
    if result.returncode != 0:
        raise Refusal("The new build rejects the configuration; nothing was stopped")


def state_home_of(environ):
    """The state home a process with NUL-separated ENVIRON resolves, or None.

    $XDG_STATE_HOME when absolute, else $HOME/.local/state, which is how thegn
    itself picks it. The first definition of a key wins, as with getenv(3).
    """
    values = {}
    for item in environ.split(b"\0"):
        key, sep, value = item.partition(b"=")
        if sep and key in (b"XDG_STATE_HOME", b"HOME"):
            values.setdefault(key, value)
    for key, suffix in ((b"XDG_STATE_HOME", ""), (b"HOME", ".local/state")):
        value = values.get(key, b"")
        if value.startswith(b"/"):
            return Path(os.fsdecode(value)) / suffix
    return None


def _process_state_home(base):
    try:
        return state_home_of(read_small(base / "environ", MAX_CONFIG))
    except (OSError, Refusal):
        return None


def _installed_identities(paths):
    found = {}
    for key in ("target", "previous"):
        with contextlib.suppress(FileNotFoundError):
            found[str(paths[key])] = identity(paths[key])
    return found


def attribute(paths, installed, pid, base, exe_path, exe_id):
    """Which installation a `thegn`/`tg` process belongs to.

    The rule of `inst_is_ours` in test/lib/instance.sh, widened by the binary:
    a process is THIS installation's when its environment resolves this run's
    state home. Running this installation's binary under ANOTHER state home is
    `same-binary` (not ours, keeps the old binary until restarted); another
    binary under another state home is `other`. An unreadable environment is
    `unknown`: never signalled, and the refusal check treats it as blocking.
    Only an `ours` process whose live executable is this installation's binary
    or is named `thegn` may be signalled; anything else ours is left to the
    operator (the refusal names it).
    """
    exe_match = exe_id is not None and installed.get(exe_path) == exe_id
    state = _process_state_home(base)
    if state is None:
        kind = "unknown"
    elif state == paths["state"].parent:
        kind = "ours"
    else:
        kind = "same-binary" if exe_match else "other"
    signal_ok = kind == "ours" and exe_path is not None and (exe_match or Path(exe_path).name == "thegn")
    return {"pid": pid, "exe": exe_path, "state": state, "kind": kind, "signal": signal_ok}


def survey(paths, proc=Path("/proc")):
    """Every same-user `thegn`/`tg` process, attributed; tolerant of races."""
    installed = _installed_identities(paths)
    records = []
    with os.scandir(proc) as processes:
        for index, entry in enumerate(processes):
            if index > 32768:
                break
            if not entry.name.isdigit() or int(entry.name) == os.getpid():
                continue
            base = Path(entry.path)
            try:
                status_text = read_small(base / "status", 65536).decode("ascii", "replace")
                uids = next(line.split()[1:] for line in status_text.splitlines() if line.startswith("Uid:"))
                if os.getuid() not in map(int, uids):
                    continue
                if any(line.split()[1:2] in (["Z"], ["X"]) for line in status_text.splitlines()
                       if line.startswith("State:")):
                    continue
                try:
                    exe_path = os.readlink(base / "exe").removesuffix(" (deleted)")
                    exe_id = identity(base / "exe")
                    named = exe_path in installed or Path(exe_path).name in THEGN_NAMES
                except PermissionError:
                    exe_path = exe_id = None
                    named = comm(status_text) in THEGN_NAMES
                if not named:
                    continue
                record = attribute(paths, installed, int(entry.name), base, exe_path, exe_id)
                argv = (base / "cmdline").read_bytes().split(b"\0")
                record["daemon"] = len(argv) > 1 and argv[1] == b"daemon"
                records.append(record)
            except (OSError, Refusal, StopIteration, ValueError):
                continue
    return sorted(records, key=lambda record: record["pid"])


def target_processes(paths, proc=Path("/proc")):
    """(pid, is_daemon) for every process this installation may signal.

    Identity comes from `/proc/<pid>/exe` and `/proc/<pid>/environ` at the
    moment of the scan, never from a pid file or a command-line pattern, so
    another installation's or another state home's thegn is never selected.
    """
    return [(record["pid"], record["daemon"]) for record in survey(paths, proc) if record["signal"]]


def report_processes(records):
    """Plan lines describing every attributed thegn process."""
    def row(record):
        return f"pid {record['pid']}, {record['exe'] or '<exe hidden>'}, state home {record['state'] or '<unreadable>'}"

    groups = {kind: [r for r in records if r["kind"] == kind] for kind in ("ours", "same-binary", "other", "unknown")}
    stoppable = [r for r in groups["ours"] if r["signal"]]
    stuck = [r for r in groups["ours"] if not r["signal"]]
    lines = [f"{len(stoppable)} thegn process(es) of this installation will be stopped"]
    lines += [f"  {row(r)}" for r in stoppable]
    if stuck:
        lines.append(f"{len(stuck)} process(es) use this state home but are not this installation's thegn binary; "
                     "they are never signalled and block the upgrade until you stop them")
        lines += [f"  {row(r)}" for r in stuck]
    if groups["other"]:
        lines.append(f"{len(groups['other'])} other thegn process(es) belong to another installation and are left alone")
        lines += [f"  {row(r)}" for r in groups["other"]]
    if groups["same-binary"]:
        lines.append(f"{len(groups['same-binary'])} thegn process(es) run this installation's binary under another "
                     "state home: not stopped, and they keep running the old binary until restarted")
        lines += [f"  {row(r)}" for r in groups["same-binary"]]
    if groups["unknown"]:
        lines.append(f"{len(groups['unknown'])} thegn process(es) cannot be attributed (environment unreadable): "
                     "never signalled, and the upgrade refuses while they run")
        lines += [f"  {row(r)}" for r in groups["unknown"]]
    return lines


def wait_quiet(paths, seconds, sleep=time.sleep, now=time.monotonic):
    """None once nothing observable uses the install/DB, else the last refusal."""
    deadline = now() + seconds
    while True:
        try:
            quiescent(paths)
            return None
        except Refusal as error:
            if now() >= deadline:
                return error
            sleep(0.25)


def _signal_all(pids, sig, kill):
    for pid in pids:
        with contextlib.suppress(ProcessLookupError):
            kill(pid, sig)


def stop_running(paths, env, proc=Path("/proc"), sleep=time.sleep, now=time.monotonic,
                 run=subprocess.run, kill=os.kill):
    """Stop this installation's controllers, then its daemon; escalate only on them.

    Order matters: controllers are asked first (SIGTERM is their graceful-quit
    path, which persists the session layout) while the daemon still serves
    them; then the daemon gets its own shutdown request.
    """
    controllers = [pid for pid, daemon in target_processes(paths, proc) if not daemon]
    if controllers:
        print(f"  asking {len(controllers)} thegn controller(s) to quit: {' '.join(map(str, controllers))}")
        _signal_all(controllers, signal.SIGTERM, kill)
        deadline = now() + CONTROLLER_GRACE
        while now() < deadline and any(not d for _, d in target_processes(paths, proc)):
            sleep(0.25)
    if any(d for _, d in target_processes(paths, proc)):
        print("  stopping the pane daemon (thegn daemon stop)")
        try:
            # The binary currently installed (the legacy in-repo one on a first
            # run); the environment selects this state home's daemon.
            result = run([str(paths["previous"]), "daemon", "stop"], env=env, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, timeout=DAEMON_STOP_SECONDS)
            if result.returncode != 0:
                print(f"  daemon stop exited {result.returncode}; falling back to signals")
        except (OSError, subprocess.SubprocessError) as error:
            print(f"  daemon stop failed ({error}); falling back to signals")
    if wait_quiet(paths, DAEMON_GRACE, sleep, now) is None:
        return
    for sig, grace in ((signal.SIGTERM, DAEMON_GRACE), (signal.SIGKILL, KILL_GRACE)):
        leftovers = [pid for pid, _ in target_processes(paths, proc)]
        if leftovers:
            print(f"  sending {sig.name} to {' '.join(map(str, leftovers))}")
            _signal_all(leftovers, sig, kill)
        error = wait_quiet(paths, grace, sleep, now)
        if error is None:
            return
    raise Refusal(f"{error}. Only this installation's own thegn processes are stopped "
                  "automatically; stop the named one yourself and rerun (the staged build is kept)")


def countdown(seconds, sleep=time.sleep):
    for remaining in range(seconds, 0, -1):
        print(f"\r  stopping thegn in {remaining}s; Ctrl-C aborts and keeps the staged build ", end="", flush=True)
        sleep(1)
    print(flush=True)


def doctor_later(target, env, output, delay=DOCTOR_DELAY, run=subprocess.run, sleep=time.sleep):
    """Capture `thegn doctor` once the new controller has had time to start."""
    def work():
        sleep(delay)
        try:
            result = run([str(target), "doctor"], env=env, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, timeout=120)
            text = result.stdout
        except (OSError, subprocess.SubprocessError) as error:
            text = f"doctor did not run: {error}\n".encode()
        with contextlib.suppress(OSError):  # best-effort: diagnostics only
            fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "wb") as stream:
                stream.write(text)

    thread = threading.Thread(target=work, name="thegn-live-doctor", daemon=True)
    thread.start()
    return thread


@contextlib.contextmanager
def _stage_build_lock(stage):
    """Create and hold a private lock for the lifetime of a staged build."""
    fd = os.open(stage / LIVE_STAGE_LOCK, os.O_CREAT | os.O_EXCL | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            raise Refusal("Unsafe staged-build lock")
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def _mark_stage(stage):
    """Mark a newly-created stage before any build process starts."""
    fd = os.open(stage / LIVE_STAGE_MARKER, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    try:
        marker = b"thegn-live-build-v1\n"
        if os.write(fd, marker) != len(marker):
            raise Refusal("Could not write complete staged-build marker")
        os.fsync(fd)
    finally:
        os.close(fd)


def comm(status_text):
    """The `Name:` field of /proc/<pid>/status.

    Readable even when `exe` and `fd` are namespace-hidden, so it is the only
    identification left for a process in a user namespace. Truncated to 15
    characters by the kernel, which both names we match against fit inside.
    """
    for line in status_text.splitlines():
        if line.startswith("Name:"):
            fields = line.split(maxsplit=1)
            return fields[1].strip() if len(fields) == 2 else ""
    raise Refusal("Process status has no name")


def _refuse_unless_foreign(record):
    where = f"pid {record['pid']}, {record['exe'] or '<exe hidden>'}"
    if record["kind"] == "ours":
        raise Refusal(f"A controller/daemon of this installation is still running ({where}); stop it manually")
    if record["kind"] == "unknown":
        raise Refusal(f"A thegn process whose installation cannot be determined is still running ({where}: "
                      "environment unreadable, so it is never signalled); stop it manually")


def quiescent(paths, proc=Path("/proc")):
    """Bounded observation, never atomic quiescence or future-startup exclusion."""
    installed = _installed_identities(paths)
    data = {identity(paths["database"])}
    for suffix in ("-wal", "-shm", "-journal"):
        side = Path(str(paths["database"]) + suffix)
        try:
            regular(side, private=True)
            data.add(identity(side))
        except FileNotFoundError:
            pass
    watched = data | set(installed.values())
    count = 0
    with os.scandir(proc) as processes:
        for entry in processes:
            if not entry.name.isdigit() or int(entry.name) == os.getpid():
                continue
            count += 1
            if count > 32768:
                raise Refusal("Process inspection bound exceeded")
            base = Path(entry.path)
            try:
                status_text = read_small(base / "status", 65536).decode("ascii")
                uids = next(line.split()[1:] for line in status_text.splitlines() if line.startswith("Uid:"))
                if len(uids) != 4:
                    raise Refusal("Ambiguous process UID")
                if os.getuid() not in map(int, uids):
                    continue
                # A zombie (or dying) process has already released its address
                # space and descriptors, so it can hold neither the database nor
                # the install; its `exe` is unreadable, which used to veto every
                # upgrade -- an unrelated `speech-dispatcher` leaves defunct
                # `sd_*` children for days.
                if any(line.split()[1:2] in (["Z"], ["X"]) for line in status_text.splitlines()
                       if line.startswith("State:")):
                    continue
                try:
                    executable = base / "exe"
                    exe_path = os.readlink(executable).removesuffix(" (deleted)")
                    exe_id = identity(executable)
                    if exe_id in data:
                        raise Refusal("A database user is still running; stop it manually")
                    if exe_id in watched or Path(exe_path).name in THEGN_NAMES:
                        # Another installation, or this binary under another
                        # state home, is left alone and does not block; the
                        # descriptor scan below still refuses one holding OUR
                        # database or installed files open.
                        _refuse_unless_foreign(attribute(paths, installed, int(entry.name), base, exe_path, exe_id))
                    with os.scandir(base / "fd") as handles:
                        for index, handle in enumerate(handles):
                            if index >= 4096:
                                raise Refusal("Process descriptor inspection bound exceeded")
                            try:
                                if identity(Path(handle.path)) in watched:
                                    raise Refusal("A process still has database/install files open; stop it manually")
                            except FileNotFoundError:
                                continue  # Descriptor closed during observation.
                except PermissionError:
                    # A process in a user namespace keeps its real uid but gets a
                    # root-owned `exe`/`fd` (rootless podman/docker: a container
                    # shows `Uid: 1000 1000 1000 1000` and a subuid `Groups:`
                    # range). Neither check above can run on it. Refusing here
                    # instead made every upgrade impossible on a host running any
                    # rootless container -- an unrelated `buildkitd` or `garage`
                    # would veto the install, repeatedly and undiagnosably.
                    #
                    # `status` is still readable, so fall back to the `Name:`
                    # comm, which is what the exe-basename check wants anyway.
                    # The narrowing is deliberate and worth stating: a namespaced
                    # process NOT named thegn that holds the database open is no
                    # longer caught. Reaching that state takes a bind-mount of
                    # the state directory into a container -- a deliberate act,
                    # not an accident -- and the alternative is a tool that can
                    # never run here at all.
                    if comm(status_text) in THEGN_NAMES:
                        _refuse_unless_foreign(attribute(paths, installed, int(entry.name), base, None, None))
                    continue
            except FileNotFoundError:
                if base.exists():
                    raise Refusal("Cannot inspect a remaining process (including zombies)") from None
            except (PermissionError, UnicodeError, StopIteration, ValueError):
                raise Refusal("Cannot inspect process ownership/files; stop manually and retry") from None


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


REVISION = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
# Paths whose content decides the release binary. A prebuilt artifact is
# admitted only when none of them differs between its revision and HEAD, and
# only over a checkout with no uncommitted or untracked change under them.
BUILD_INPUTS = ("crates", "Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml", ".cargo", "build.rs")
SIDECAR_SUFFIX = ".build.json"
MAX_SIDECAR = 64 * 1024


def _git(repo, env):
    """Hardened git: no optional locks, fsmonitor off, cleaned GIT_ env, literal pathspecs."""
    clean_env = {key: value for key, value in env.items() if not key.startswith("GIT_")}
    clean_env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_NO_REPLACE_OBJECTS="1")

    def git(*args, check=True, quiet=False):
        return subprocess.run(["git", "--no-optional-locks", "--literal-pathspecs", "-c", "core.fsmonitor=false", *args],
                              cwd=repo, env=clean_env, check=check, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL if quiet else None, timeout=30)

    return git


def source_revision(repo, env, require_clean=True, inputs_only=False):
    """HEAD of the canonical checkout root, after the cleanliness rule.

    inputs_only (prebuilt mode only): uncommitted or untracked changes are
    allowed outside BUILD_INPUTS, since the binary is not built from this
    working tree. The default stays the whole-tree rule.
    """
    run = _git(repo, env)

    def git(*args):
        return run(*args).stdout

    if Path(os.fsdecode(git("rev-parse", "--show-toplevel").rstrip(b"\n"))) != repo:
        raise Refusal("Selected source must be the canonical Git checkout root")
    if require_clean and any(entry and (entry[:1].islower() or entry[:1] == b"S") for entry in git("ls-files", "-v", "-z").split(b"\0")):
        raise Refusal("Assume-unchanged/skip-worktree index flags are unsupported")
    if require_clean and inputs_only:
        if git("status", "--porcelain=v1", "--untracked-files=normal", "--", *BUILD_INPUTS):
            raise Refusal(f"The checkout has uncommitted or untracked changes under the build inputs "
                          f"({', '.join(BUILD_INPUTS)}), so its schema and sources do not match a prebuilt artifact; "
                          "commit or stash them first (changes elsewhere, such as docs, are allowed)")
    elif require_clean and git("status", "--porcelain=v1", "--untracked-files=normal"):
        raise Refusal("Selected checkout must be clean, including untracked source; commit reviewed changes first")
    revision = git("rev-parse", "--verify", "HEAD").decode("ascii").strip()
    if not REVISION.fullmatch(revision):
        raise Refusal("Unsupported source revision")
    return revision


def prebuilt_provenance(repo, env, revision):
    """The artifact revision is HEAD or an ancestor, with identical build inputs."""
    git = _git(repo, env)
    if git("cat-file", "-e", f"{revision}^{{commit}}", check=False, quiet=True).returncode != 0:
        raise Refusal(f"Prebuilt revision {revision} is not a commit in the local repository {repo}; "
                      "fetch it (git fetch) and rerun")
    ancestor = git("merge-base", "--is-ancestor", revision, "HEAD", check=False, quiet=True).returncode
    if ancestor == 1:
        raise Refusal(f"Prebuilt revision {revision} is not an ancestor of the checkout's HEAD; check out a "
                      "commit that contains it, or rebuild on the build box at this HEAD")
    if ancestor != 0:
        raise Refusal(f"Could not compare prebuilt revision {revision} with HEAD (git merge-base exit {ancestor})")
    changed = git("diff", "--quiet", "--no-ext-diff", "--no-textconv", revision, "HEAD", "--", *BUILD_INPUTS,
                  check=False, quiet=True).returncode
    if changed == 1:
        raise Refusal(f"Build inputs ({', '.join(BUILD_INPUTS)}) changed between prebuilt revision {revision} and "
                      "HEAD; rebuild on the build box at this HEAD")
    if changed != 0:
        raise Refusal(f"Could not diff prebuilt revision {revision} against HEAD (git diff exit {changed})")


def inspect_prebuilt(value):
    """Checks 1 and 2: the binary and its sidecar, without any Git access."""
    try:
        path = absolute(value)
    except Refusal as error:
        raise Refusal(f"--prebuilt {value!r} is not usable ({error}); pass the absolute path of the copied binary") from None
    try:
        regular(path, executable=True)
    except FileNotFoundError:
        raise Refusal(f"--prebuilt {path} does not exist; copy the binary and its {path.name}{SIDECAR_SUFFIX} "
                      "from the build box first") from None
    except Refusal as error:
        raise Refusal(f"--prebuilt {path} is refused ({error}); copy it as a plain executable file you own, "
                      "not a symlink, writable only by you") from None
    sidecar = path.with_name(path.name + SIDECAR_SUFFIX)
    try:
        regular(sidecar)
        raw = read_small(sidecar, MAX_SIDECAR)
    except FileNotFoundError:
        raise Refusal(f"Prebuilt sidecar {sidecar} is missing; copy the build box's {path.name}{SIDECAR_SUFFIX} "
                      "next to the binary") from None
    except (Refusal, OSError) as error:
        raise Refusal(f"Prebuilt sidecar {sidecar} is refused ({error}); it must be a small regular file you own, "
                      "not a symlink") from None
    try:
        document = json.loads(raw)
    except (ValueError, UnicodeError):
        raise Refusal(f"Prebuilt sidecar {sidecar} is not valid JSON; copy it again from the build box") from None
    if not isinstance(document, dict):
        raise Refusal(f"Prebuilt sidecar {sidecar} is not a JSON object; copy it again from the build box")
    revision = document.get("revision")
    if not isinstance(revision, str) or not REVISION.fullmatch(revision):
        raise Refusal(f"Prebuilt sidecar {sidecar} has no valid revision (40 or 64 lowercase hex); "
                      "regenerate it on the build box")
    if document.get("build") != BUILD:
        raise Refusal(f"Prebuilt sidecar {sidecar} records the build command {document.get('build')!r}, not the "
                      f"installer's {BUILD!r}; rebuild on the build box with exactly that command")
    if document.get("dirty") is not False:
        raise Refusal(f"Prebuilt sidecar {sidecar} has dirty={document.get('dirty')!r}, not false; rebuild on the "
                      "build box from a clean checkout")
    expected, actual = document.get("sha256"), digest(path)
    if expected != actual:
        raise Refusal(f"Prebuilt binary {path} has sha256 {actual}, but its sidecar records {expected!r}; the copy "
                      "is incomplete or the files do not belong together. Copy both again from the build box")
    return {"path": path, "sidecar": sidecar, "revision": revision, "sha256": actual}


def admit_prebuilt(repo, env, value):
    """Checks 1 to 3. Check 4 is source_revision(..., inputs_only=True)."""
    artifact = inspect_prebuilt(value)
    prebuilt_provenance(repo, env, artifact["revision"])
    return artifact


def live_cache(paths):
    """The persistent private Cargo target dir reused by every `just live`."""
    cache = paths["repo"] / "target" / LIVE_CACHE
    try:
        cache.mkdir(mode=0o700)
    except FileExistsError:
        pass
    directories(cache, private=True)
    return cache


def source_schema(repo):
    source = read_small(repo / "crates/thegn-core/src/db.rs")
    versions = re.findall(rb"^pub const SCHEMA_VERSION: i64 = ([0-9]+);$", source, re.MULTILINE)
    if len(versions) != 1:
        raise Refusal("Cannot determine this source's database schema version")
    return int(versions[0])


def stage_prebuilt(paths, env, value):
    """Admit a binary built elsewhere into a fresh stage; Cargo never runs.

    The checkout still supplies the schema version: the provenance rule makes
    its crates/ identical to the artifact's revision.
    """
    repo = paths["repo"]
    revision = source_revision(repo, env, inputs_only=True)
    artifact = admit_prebuilt(repo, env, value)
    schema = source_schema(repo)
    retain_live_stages(paths)
    (repo / "target").mkdir(mode=0o755, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=LIVE_STAGE_PREFIX, dir=repo / "target"))
    print(f"Retaining staged prebuilt artifact at {stage}", flush=True)
    with _stage_build_lock(stage):
        _mark_stage(stage)
        binary = stage / "output/release/thegn"
        binary.parent.mkdir(parents=True, mode=0o700)
        copy_file(artifact["path"], binary, time.monotonic() + BACKUP_SECONDS, 0o755)
        regular(binary, executable=True)
        staged = digest(binary)
        if staged != artifact["sha256"]:
            raise Refusal(f"Prebuilt artifact {artifact['path']} changed while it was copied into {binary}; "
                          "copy it again from the build box and rerun")
        if source_revision(repo, env, inputs_only=True) != revision:
            raise Refusal("Source changed while staging the prebuilt artifact; artifact retained but not admitted")
        record = {"repo": str(repo), "stage": str(stage), "observed_revision": revision,
                  "artifact_revision": artifact["revision"], "prebuilt": True, "source": str(artifact["path"]),
                  "schema": schema, "sha256": staged, "build": BUILD}
        (stage / "build.json").write_text(json.dumps(record))
    return stage, binary, record


def build_stage(paths, env, prebuilt=None):
    if prebuilt is not None:
        return stage_prebuilt(paths, env, prebuilt)
    revision = source_revision(paths["repo"], env)
    schema = source_schema(paths["repo"])
    retain_live_stages(paths)
    cache = live_cache(paths)
    stage = Path(tempfile.mkdtemp(prefix=".thegn-live-build-", dir=paths["repo"] / "target"))
    print(f"Retaining staged build at {stage}", flush=True)
    # One persistent, private build cache for `just live` only: a rebuild after a
    # few commits recompiles just what changed instead of all ~600 crates. It is
    # still isolated from every other worktree's and profile's Cargo output, and
    # from target/release (no longer the installation). Serialized by the launcher
    # locks and Cargo's own build-dir lock. All cores: the build is the wait.
    build_env = dict(env, RUSTC_WRAPPER="", CARGO_TARGET_DIR=str(cache), CARGO_BUILD_BUILD_DIR=str(cache / "intermediate"),
                     CARGO_BUILD_JOBS=str(os.cpu_count() or 1))
    with _stage_build_lock(stage):
        _mark_stage(stage)
        started = time.monotonic()
        subprocess.run(BUILD, cwd=paths["repo"], env=build_env, check=True)
        print(f"  built in {time.monotonic() - started:.0f}s", flush=True)
        if source_revision(paths["repo"], env) != revision:
            raise Refusal("Source changed during build; artifact retained but not admitted")
        built = cache / "release/thegn"
        regular(built, executable=True)
        # The stage holds the admitted artifact; the cache's copy is overwritten
        # by the next build, so the checksum binds this copy, not that one.
        binary = stage / "output/release/thegn"
        binary.parent.mkdir(parents=True, mode=0o700)
        copy_file(built, binary, time.monotonic() + BACKUP_SECONDS, 0o755)
        regular(binary, executable=True)
        # Observed Git metadata, not exact commit materialization/content proof.
        record = {"repo": str(paths["repo"]), "stage": str(stage), "observed_revision": revision,
                  "artifact_revision": revision, "prebuilt": False,
                  "schema": schema, "sha256": digest(binary), "build": BUILD}
        (stage / "build.json").write_text(json.dumps(record))
    print("Build retained for inspection; automated artifact reuse is unsupported.", flush=True)
    return stage, binary, record


def copy_file(source, destination, deadline, mode=0o600):
    fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    with os.fdopen(fd, "wb") as out, source.open("rb") as inp:
        os.fchmod(out.fileno(), mode)  # Exact owned-file mode, independent of umask.
        while chunk := inp.read(1024 * 1024):
            if time.monotonic() >= deadline:
                raise Refusal("Backup/copy deadline exceeded; installation not attempted")
            out.write(chunk)
        out.flush()
        os.fsync(out.fileno())


def backup(paths, record):
    recovery = Path(tempfile.mkdtemp(prefix="live-backup-", dir=paths["state"]))
    print(f"Recovery directory retained: {recovery}", flush=True)
    deadline = time.monotonic() + BACKUP_SECONDS
    copy_file(paths["previous"], recovery / "thegn.previous", deadline, 0o700)
    output = recovery / "thegn.db"
    fd = os.open(output, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    os.close(fd)

    def progress(_status, _remaining, _total):
        if time.monotonic() >= deadline:
            raise Refusal("SQLite backup deadline exceeded; installation not attempted")

    # mode=ro does not mean no WAL/SHM side effects. This is a normal SQLite
    # connection, not immutable=1: committed WAL contents must be backed up.
    with contextlib.closing(sqlite3.connect(paths["database"].as_uri() + "?mode=ro", uri=True, timeout=0.1)) as source:
        with contextlib.closing(sqlite3.connect(output, timeout=0.1)) as destination:
            source.backup(destination, pages=128, progress=progress, sleep=0.05)
            version = destination.execute("PRAGMA user_version").fetchone()[0]
            if version > record["schema"]:
                raise Refusal("Database is newer than selected source; downgrade refused")
    with output.open("rb") as stream:
        os.fsync(stream.fileno())
    # The staged path is transient build provenance. Persisting it in a
    # recovery descriptor would make every successful build permanently
    # ineligible for retention. Legacy or explicitly supplied recovery
    # references remain protected by _recovery_linked_stages.
    persisted_record = {key: value for key, value in record.items() if key != "stage"}
    with (recovery / "complete.json").open("x") as stream:
        json.dump({"database": str(paths["database"]), "target": str(paths["target"]),
                   "previous": str(paths["previous"]), "schema": version,
                   "binary_sha256": digest(recovery / "thegn.previous"), "database_sha256": digest(output), "build": persisted_record}, stream)
        stream.flush()
        os.fsync(stream.fileno())
    # Reserve a fresh per-launch stderr file before replacing anything. Never
    # truncate a pre-existing log (which could be a hardlink or special file).
    fd = os.open(recovery / "thegn-stderr.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    os.close(fd)
    sync_directory(recovery)
    sync_directory(paths["state"])
    return recovery


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _maybe_identity(path):
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    return info.st_dev, info.st_ino


def _snapshot(paths):
    return (identity(paths["previous"]), identity(paths["database"]),
            _maybe_identity(paths["target"]), _maybe_identity(paths["alacritty"]))


def install(paths, binary, record, env):
    """Called under both locks, after confirmation; failures never roll DB back."""
    before = _snapshot(paths)
    quiescent(paths)
    regular(binary, executable=True)
    recovery = backup(paths, record)
    quiescent(paths)
    if _snapshot(paths) != before:
        raise Refusal("Install/database identity changed; retained backup, refusing install")
    temporary = paths["install_dir"] / (".thegn-install-" + recovery.name)
    deadline = time.monotonic() + BACKUP_SECONDS
    copy_file(binary, temporary, deadline, 0o755)
    if digest(temporary) != record["sha256"]:
        raise Refusal("Staged artifact changed; installation refused")
    config_temporary = paths["install_dir"] / (".thegn-alacritty-" + recovery.name)
    copy_file(paths["alacritty_source"], config_temporary, deadline, 0o644)
    # Copies may take time: repeat observations at the last replacement point.
    # This still does not exclude a legacy process starting after this scan.
    if settings(paths["repo"], env, paths["install_dir"]) != paths:
        raise Refusal("Policy/path selection changed before replacement")
    quiescent(paths)
    if _snapshot(paths) != before:
        raise Refusal("Install/database identity changed before replacement")
    # The alacritty config first: a failure there leaves the binary untouched.
    os.replace(config_temporary, paths["alacritty"])
    os.replace(temporary, paths["target"])
    sync_directory(paths["install_dir"])
    return recovery


# --- launchers ---------------------------------------------------------------
# ~/.local/bin is the owner's; it changes only with --relink-launchers, and the
# whole change is validated (relink_plan) before anything is stopped.

_PATH_CHAR = r"[\w./-]"


def _exact(path):
    return re.compile(rf"(?<!{_PATH_CHAR}){re.escape(str(path))}(?!{_PATH_CHAR})")


def _read_wrapper(path):
    """(text, mode) of a current-user regular UTF-8 text file, else Refusal."""
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
        raise Refusal(f"Launcher {path} is not a current-user regular file; relink it yourself")
    try:
        text = read_small(path, MAX_WRAPPER).decode("utf-8")
    except UnicodeError:
        raise Refusal(f"Launcher {path} is not a UTF-8 text file; relink it yourself") from None
    if "\0" in text:
        raise Refusal(f"Launcher {path} is not a text file; relink it yourself")
    return text, stat.S_IMODE(info.st_mode)


def launcher_state(paths, home):
    """Read-only lines describing ~/.local/bin/{thegn,tg,tg-tui}."""
    bin_dir = home / LAUNCHER_DIR
    old_binary, old_config = paths["repo"] / "target/release/thegn", paths["alacritty_source"]
    lines = []
    link = bin_dir / "thegn"
    if link.is_symlink():
        resolved = Path(os.path.realpath(link))
        verdict = "resolves to the install target" if resolved == paths["target"] else "does NOT resolve to the install target"
        lines.append(f"{link} -> {os.readlink(link)} ({verdict} {paths['target']})")
    elif os.path.lexists(link):
        lines.append(f"{link} is not a symlink; it does not follow the install target")
    else:
        lines.append(f"{link} is absent")
    for name in WRAPPERS:
        wrapper = bin_dir / name
        if not os.path.lexists(wrapper):
            lines.append(f"{wrapper} is absent")
            continue
        try:
            text, _mode = _read_wrapper(wrapper)
        except (OSError, Refusal) as error:
            lines.append(f"{wrapper}: unreadable as a text launcher ({error})")
            continue
        mentions = [str(path) for path in (old_binary, old_config) if _exact(path).search(text)]
        lines.append(f"{wrapper} mentions the old repo path(s): {', '.join(mentions)}" if mentions
                     else f"{wrapper} mentions no old repo path")
    return lines


def launchers_run_checkout(paths, home):
    """True when ~/.local/bin/thegn, tg or tg-tui still runs the checkout's binary or config."""
    bin_dir = home / LAUNCHER_DIR
    link = bin_dir / "thegn"
    if link.is_symlink() and Path(os.path.realpath(link)) != paths["target"]:
        return True
    old = (_exact(paths["repo"] / "target/release/thegn"), _exact(paths["alacritty_source"]))
    for name in WRAPPERS:
        wrapper = bin_dir / name
        if not os.path.lexists(wrapper):
            continue
        try:
            text, _mode = _read_wrapper(wrapper)
        except (OSError, Refusal):
            continue
        if any(pattern.search(text) for pattern in old):
            return True
    return False


def relink_plan(paths, home):
    """Validate every launcher change up front; [] or Refusal, changing nothing."""
    bin_dir = home / LAUNCHER_DIR
    info = bin_dir.lstat() if os.path.lexists(bin_dir) else None
    if info is None or not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise Refusal(f"{bin_dir} is not a current-user directory; relink the launchers yourself")
    actions = []
    link = bin_dir / "thegn"
    if link.is_symlink():
        if Path(os.path.realpath(link)) != paths["target"]:
            actions.append(("symlink", link, os.readlink(link)))
    elif os.path.lexists(link):
        raise Refusal(f"{link} is not a symlink; refusing to replace it (nothing changed)")
    else:
        actions.append(("symlink", link, None))
    replacements = ((paths["repo"] / "target/release/thegn", paths["target"]),
                    (paths["alacritty_source"], paths["alacritty"]))
    for name in WRAPPERS:
        wrapper = bin_dir / name
        if not os.path.lexists(wrapper):
            continue
        text, mode = _read_wrapper(wrapper)
        updated = text
        for old, new in replacements:
            updated = _exact(old).sub(lambda _match, new=new: str(new), updated)
        if updated != text:
            actions.append(("wrapper", wrapper, (updated, mode)))
        elif not any(_exact(new).search(text) for _old, new in replacements):
            raise Refusal(f"{wrapper} mentions neither {replacements[0][0]} nor {replacements[1][0]}; "
                          "refusing to edit it (nothing changed)")
    return actions


def _backup_name(path):
    for index in range(100):
        candidate = path.with_name(path.name + (".bak" if index == 0 else f".bak.{index}"))
        if not os.path.lexists(candidate):
            return candidate
    raise Refusal(f"Too many backups beside {path}")


def apply_relink(paths, actions):
    for kind, path, detail in actions:
        temporary = path.with_name(f".{path.name}.thegn-live-{os.getpid()}")
        if kind == "symlink":
            os.symlink(paths["target"], temporary)
            os.replace(temporary, path)
            print(f"  {path} -> {paths['target']} (was {detail or 'absent'})")
            continue
        text, mode = detail
        backup_path = _backup_name(path)
        shutil.copy2(path, backup_path, follow_symlinks=False)
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        print(f"  rewrote {path} (previous copy: {backup_path})")
    if actions:
        sync_directory(actions[0][1].parent)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--repo", help="Absolute Git root to build (the installation is chosen by --install-dir)")
    parser.add_argument("--level", default="info", choices=("trace", "debug", "info", "warn", "error"))
    parser.add_argument("--size-mb", type=int, default=20)
    parser.add_argument("--files", type=int, default=5)
    parser.add_argument("--yes", action="store_true", help="Skip the Ctrl-C window before thegn is stopped")
    parser.add_argument("--install-dir", help=f"Absolute install directory outside the checkout (default: ${INSTALL_ENV}, "
                        f"else <passwd home>/{DEFAULT_INSTALL})")
    parser.add_argument("--relink-launchers", action="store_true",
                        help="Repoint ~/.local/bin/thegn at the install target and rewrite the old repo paths in "
                             "~/.local/bin/tg and tg-tui (a .bak copy is kept beside each)")
    parser.add_argument("--no-scope", action="store_true",
                        help="Launch thegn directly instead of in a systemd user scope with higher CPU/IO weight")
    parser.add_argument("--prebuilt", metavar="BINARY",
                        help="Install this absolute-path release binary built elsewhere (with its BINARY.build.json "
                             "sidecar) instead of building locally; see docs/live-upgrades.md")
    raw_argv = sys.argv[1:] if argv is None else list(argv)
    args = parser.parse_args(raw_argv)
    if not 1 <= args.size_mb <= 1024 or not 1 <= args.files <= 100:
        raise Refusal("Log limits must be 1..1024 MiB and 1..100 rotations")
    env = dict(os.environ)
    repo = absolute(args.repo) if args.repo else Path(__file__).resolve().parent.parent
    directories(repo)
    paths = settings(repo, env, args.install_dir)
    home = real_home()
    prebuilt = args.prebuilt is not None
    # A prebuilt plan runs every admission check, so it answers whether the
    # real run would be admitted; a local-build plan tolerates dirty source.
    revision = source_revision(repo, env, require_clean=not args.plan or prebuilt, inputs_only=prebuilt)
    artifact = admit_prebuilt(repo, env, args.prebuilt) if prebuilt else None
    print(json.dumps({key: str(value) for key, value in paths.items()}, indent=2))
    if prebuilt:
        print(f"Build: none here; prebuilt artifact {artifact['path']} (built elsewhere with:", " ".join(BUILD) + ")")
        print(f"Prebuilt: would install {artifact['path']}, sidecar revision {artifact['revision']}, "
              f"sha256 {artifact['sha256'][:16]}... (admitted: build inputs match HEAD)")
    else:
        print("Build: local,", " ".join(BUILD), "(isolated output + intermediate directories)")
    print("Selected revision:", revision, "(plan does not admit dirty source for a build)")
    print("Brand moves disabled with THEGN_NO_MIGRATE=1; database migration policy is not overridden.")
    print(f"Install: {paths['target']} and {paths['alacritty']} (outside the checkout)")
    if paths["previous"] != paths["target"]:
        print(f"First run at this location: {paths['previous']} is treated as the previous installation "
              f"(its processes are this installation's, and it is the backed-up binary); "
              f"the new build installs to {paths['target']}"
              + ("" if paths["install_dir"].exists() else f", creating {paths['install_dir']}"))
    for line in report_processes(survey(paths)):
        print(line)
    print("Launchers:")
    for line in launcher_state(paths, home):
        print(f"  {line}")
    if args.relink_launchers:
        actions = relink_plan(paths, home)
        print(f"--relink-launchers will change {len(actions)} launcher(s) after the install:"
              if actions else "--relink-launchers: launchers already follow the install target")
        for _kind, path, _detail in actions:
            print(f"  {path}")
    elif not args.plan:
        print("Launchers are not changed without --relink-launchers.")
    if not args.relink_launchers and launchers_run_checkout(paths, home):
        # Otherwise every later start (and every `thegn` CLI call from the
        # panes) still runs the in-repo binary, which any release build replaces.
        stale = (f"launchers still run {paths['repo'] / 'target/release/thegn'}; rerun with "
                 "`python3 -B scripts/live.py --relink-launchers` (or relink them yourself)")
        if not args.plan:
            raise Refusal(stale)
        print(f"WARNING: {stale}")
    if args.plan:
        return 0
    # The terminal check comes FIRST, before the relaunch below: a script or an
    # agent (no tty) must never be able to start an upgrade, including by
    # having it reopen itself in a fresh window that nobody is watching.
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        raise Refusal("A real terminal is required: the new controller launches in the foreground")
    if thegn_ancestor() is not None:
        relaunch_outside(raw_argv, env, repo)
        return 0
    if not paths["install_dir"].exists():
        paths["install_dir"].mkdir(mode=0o755, parents=True)
    directories(paths["install_dir"])
    with contextlib.ExitStack() as locks:
        locks.enter_context(locked(paths["target"].parent / ".thegn-live-install.lock"))
        if paths["previous"].parent != paths["target"].parent:
            # First run: an older helper serializes on the legacy location.
            locks.enter_context(locked(paths["previous"].parent / ".thegn-live-install.lock"))
        locks.enter_context(locked(paths["state"] / "live-upgrade.lock"))
        step(1, "Preflight: clean source and supported paths")
        source_revision(repo, env, inputs_only=prebuilt)
        # Every launcher change is validated before anything is stopped.
        relink = relink_plan(paths, home) if args.relink_launchers else []
        if prebuilt:
            step(2, "Stage the prebuilt artifact (no local build)")
            stage, binary, record = build_stage(paths, env, args.prebuilt)
        else:
            step(2, "Build (the running instance keeps working meanwhile)")
            stage, binary, record = build_stage(paths, env)
        step(3, "Validate the configuration with the NEW build")
        validate_config(binary, env)
        step(4, "Stop the running thegn (controllers, then the pane daemon)")
        # Re-surveyed after the build: show exactly what is about to be stopped.
        for line in report_processes(survey(paths)):
            print(line)
        if not args.yes:
            countdown(STOP_COUNTDOWN)
        stop_running(paths, env)
        # Re-read policy and path constraints after the potentially long build.
        if settings(repo, env, args.install_dir) != paths:
            raise Refusal("Upgrade paths changed while building")
        if source_revision(repo, env, inputs_only=prebuilt) != record["observed_revision"]:
            raise Refusal("Source changed after build; installation refused")
        if prebuilt:
            prebuilt_provenance(repo, env, record["artifact_revision"])
        step(5, "Back up the database and old binary, then install")
        logs = paths["state"] / "logs"
        if logs.exists() or logs.is_symlink():
            directories(logs)  # Existing normal 0755 logs are under private state.
        with locked(Path(str(paths["database"]) + ".schema.lock"), schema=True):
            recovery = install(paths, binary, record, env)
            # Still under the launcher and schema locks, and only after a
            # successful install: this run's backup is `recovery` and is kept.
            pruned = prune_backups(paths, recovery)
            for path in pruned:
                print(f"  removed old backup {path}")
            print(f"  removed {len(pruned)} old backup(s) (kept the newest {BACKUP_KEEP}, those under "
                  f"{BACKUP_KEEP_DAYS} days, incomplete ones and this run's)")
            # Release stages that only the removed backups pinned.
            retain_live_stages(paths, current=stage)
        print(f"  recovery (database backup + previous binary): {recovery}")
        if relink:
            try:
                apply_relink(paths, relink)
            except OSError as error:
                # Installed already: report, never abort the launch over it.
                print(f"  relinking the launchers failed ({error}); fix them by hand")
        step(6, "Launch (the database migrates on this first start)")
        doctor_report = recovery / "doctor.txt"
        print(f"  `thegn doctor` will be captured to {doctor_report} about {DOCTOR_DELAY}s after launch")
        doctor_later(paths["target"], env, doctor_report)
        child_env = dict(env, THEGN_NO_MIGRATE="1", RUST_BACKTRACE="full", THEGN_LOG=f"{args.level},log=error,tokei=error",
                         THEGN_LOG_ROTATION_SIZE_MB=str(args.size_mb), THEGN_LOG_MAX_FILES=str(args.files), THEGN_PERF="1")
        # Retain the launcher lock in this supervisor for the entire foreground
        # lifetime; schema lock is released so normal startup can migrate.
        fd = os.open(recovery / "thegn-stderr.log", os.O_WRONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            os.close(fd)
            raise Refusal("Reserved launch stderr file changed")
        with os.fdopen(fd, "wb") as stderr:
            # A transient user scope gives the UI CPU/IO weight 1000 over builds
            # in this terminal's cgroup (see scope_argv); falls back to a direct
            # launch. systemd-run --scope execs in place: the PID is thegn's.
            argv = [str(paths["target"])] if args.no_scope else scope_argv(paths["target"], child_env)
            child = subprocess.Popen(argv, cwd=repo, env=child_env, stderr=stderr)
            while True:
                try:
                    code = child.wait()
                    break
                except KeyboardInterrupt:
                    print("Foreground child still supervised; waiting for its exit.", file=sys.stderr)
        print(f"Child exited {code}; readiness was not verified. Recovery retained at {recovery}")
        print("Launchers:")
        for line in launcher_state(paths, home):
            print(f"  {line}")
        return code if code >= 0 else 128 - code


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("Interrupted; no automatic rollback. Inspect retained artifacts.", file=sys.stderr)
        sys.exit(130)
    except (Refusal, OSError, ValueError, EOFError, sqlite3.Error, subprocess.SubprocessError) as error:
        print(f"Live upgrade refused/failed: {error}. No automatic rollback. Stop manually and inspect retained artifacts.", file=sys.stderr)
        sys.exit(1)
