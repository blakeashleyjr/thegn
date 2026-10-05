#!/usr/bin/env python3
"""Stepped Linux upgrade of the real instance: build, validate, stop, back up,
install, launch. Never a readiness protocol.

Assumes a stable current-user namespace, not hostile same-UID/root processes.
/proc observations and cooperative flocks cannot exclude legacy restarts. SQLite
backup deadlines are checked between backup steps, not cancellation of kernel I/O.
Only processes whose live executable IS the installation target are signalled;
other `thegn`/`tg` processes block the upgrade and are named, never touched.
"""

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
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


def settings(repo, env):
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
    directories(repo / "target/release")
    target, database = repo / "target/release/thegn", state / "thegn.db"
    regular(target, executable=True)
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
    pin = override("THEGN_DATABASE_MIGRATION_EXECUTABLE", db.get("migration_executable", ""))
    if not isinstance(pin, str) or (pin and absolute(pin).resolve(strict=True) != target):
        raise Refusal("Configured migration executable must resolve to the installation target")
    return {"repo": repo, "state": state, "database": database, "target": target, "config": config}


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


def target_processes(target, proc=Path("/proc")):
    """(pid, is_daemon) for every process whose live executable IS `target`.

    Identity comes from `/proc/<pid>/exe` at the moment of the scan, never from
    a pid file or a command-line pattern, so another checkout's or another
    profile's thegn is never selected.
    """
    want_path, want_identity = str(target), identity(target)
    found = []
    with os.scandir(proc) as processes:
        for entry in processes:
            if not entry.name.isdigit() or int(entry.name) == os.getpid():
                continue
            base = Path(entry.path)
            try:
                link = os.readlink(base / "exe")
            except OSError:
                continue
            if link.removesuffix(" (deleted)") != want_path:
                continue
            try:
                if identity(base / "exe") != want_identity:
                    continue
                argv = (base / "cmdline").read_bytes().split(b"\0")
            except OSError:
                continue
            found.append((int(entry.name), len(argv) > 1 and argv[1] == b"daemon"))
    return found


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
    target = paths["target"]
    controllers = [pid for pid, daemon in target_processes(target, proc) if not daemon]
    if controllers:
        print(f"  asking {len(controllers)} thegn controller(s) to quit: {' '.join(map(str, controllers))}")
        _signal_all(controllers, signal.SIGTERM, kill)
        deadline = now() + CONTROLLER_GRACE
        while now() < deadline and any(not d for _, d in target_processes(target, proc)):
            sleep(0.25)
    if any(d for _, d in target_processes(target, proc)):
        print("  stopping the pane daemon (thegn daemon stop)")
        try:
            result = run([str(target), "daemon", "stop"], env=env, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, timeout=DAEMON_STOP_SECONDS)
            if result.returncode != 0:
                print(f"  daemon stop exited {result.returncode}; falling back to signals")
        except (OSError, subprocess.SubprocessError) as error:
            print(f"  daemon stop failed ({error}); falling back to signals")
    if wait_quiet(paths, DAEMON_GRACE, sleep, now) is None:
        return
    for sig, grace in ((signal.SIGTERM, DAEMON_GRACE), (signal.SIGKILL, KILL_GRACE)):
        leftovers = [pid for pid, _ in target_processes(target, proc)]
        if leftovers:
            print(f"  sending {sig.name} to {' '.join(map(str, leftovers))}")
            _signal_all(leftovers, sig, kill)
        error = wait_quiet(paths, grace, sleep, now)
        if error is None:
            return
    raise Refusal(f"{error}. Only processes running this installation's binary are stopped "
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


def quiescent(paths, proc=Path("/proc")):
    """Bounded observation, never atomic quiescence or future-startup exclusion."""
    watched = {identity(paths["target"]), identity(paths["database"])}
    for suffix in ("-wal", "-shm", "-journal"):
        side = Path(str(paths["database"]) + suffix)
        try:
            regular(side, private=True)
            watched.add(identity(side))
        except FileNotFoundError:
            pass
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
                    name = Path(os.readlink(executable).removesuffix(" (deleted)")).name
                    if identity(executable) in watched or name in ("thegn", "tg"):
                        raise Refusal("A controller/daemon or database user is still running; stop it manually")
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
                    if comm(status_text) in ("thegn", "tg"):
                        raise Refusal("A controller/daemon is still running; stop it manually") from None
                    continue
            except FileNotFoundError:
                if base.exists():
                    raise Refusal("Cannot inspect a remaining process (including zombies)") from None
            except (PermissionError, UnicodeError, StopIteration, ValueError):
                raise Refusal("Cannot inspect process ownership/files; stop manually and retry") from None


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def source_revision(repo, env, require_clean=True):
    clean_env = {key: value for key, value in env.items() if not key.startswith("GIT_")}
    clean_env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_NO_REPLACE_OBJECTS="1")

    def git(*args):
        return subprocess.run(["git", "--no-optional-locks", "-c", "core.fsmonitor=false", *args], cwd=repo,
                              env=clean_env, check=True, stdout=subprocess.PIPE, timeout=30).stdout

    if Path(os.fsdecode(git("rev-parse", "--show-toplevel").rstrip(b"\n"))) != repo:
        raise Refusal("Selected source must be the canonical Git checkout root")
    if require_clean and any(entry and (entry[:1].islower() or entry[:1] == b"S") for entry in git("ls-files", "-v", "-z").split(b"\0")):
        raise Refusal("Assume-unchanged/skip-worktree index flags are unsupported")
    if require_clean and git("status", "--porcelain=v1", "--untracked-files=normal"):
        raise Refusal("Selected checkout must be clean, including untracked source; commit reviewed changes first")
    revision = git("rev-parse", "--verify", "HEAD").decode("ascii").strip()
    if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", revision):
        raise Refusal("Unsupported source revision")
    return revision


def live_cache(paths):
    """The persistent private Cargo target dir reused by every `just live`."""
    cache = paths["repo"] / "target" / LIVE_CACHE
    try:
        cache.mkdir(mode=0o700)
    except FileExistsError:
        pass
    directories(cache, private=True)
    return cache


def build_stage(paths, env):
    revision = source_revision(paths["repo"], env)
    source = read_small(paths["repo"] / "crates/thegn-core/src/db.rs")
    versions = re.findall(rb"^pub const SCHEMA_VERSION: i64 = ([0-9]+);$", source, re.MULTILINE)
    if len(versions) != 1:
        raise Refusal("Cannot determine this source's database schema version")
    schema = int(versions[0])
    retain_live_stages(paths)
    cache = live_cache(paths)
    stage = Path(tempfile.mkdtemp(prefix=".thegn-live-build-", dir=paths["repo"] / "target"))
    print(f"Retaining staged build at {stage}", flush=True)
    # One persistent, private build cache for `just live` only: a rebuild after a
    # few commits recompiles just what changed instead of all ~600 crates. It is
    # still isolated from every other worktree's and profile's Cargo output, and
    # from target/release (the installed binary). Serialized by the launcher
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
    copy_file(paths["target"], recovery / "thegn.previous", deadline, 0o700)
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
        json.dump({"database": str(paths["database"]), "target": str(paths["target"]), "schema": version,
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


def install(paths, binary, record, env):
    """Called under both locks, after confirmation; failures never roll DB back."""
    target_id, db_id = identity(paths["target"]), identity(paths["database"])
    quiescent(paths)
    regular(binary, executable=True)
    recovery = backup(paths, record)
    quiescent(paths)
    if (identity(paths["target"]), identity(paths["database"])) != (target_id, db_id):
        raise Refusal("Install/database identity changed; retained backup, refusing install")
    temporary = paths["target"].parent / (".thegn-install-" + recovery.name)
    copy_file(binary, temporary, time.monotonic() + BACKUP_SECONDS, 0o755)
    if digest(temporary) != record["sha256"]:
        raise Refusal("Staged artifact changed; installation refused")
    # Copies may take time: repeat observations at the last replacement point.
    # This still does not exclude a legacy process starting after this scan.
    if settings(paths["repo"], env) != paths:
        raise Refusal("Policy/path selection changed before replacement")
    quiescent(paths)
    if (identity(paths["target"]), identity(paths["database"])) != (target_id, db_id):
        raise Refusal("Install/database identity changed before replacement")
    os.replace(temporary, paths["target"])
    sync_directory(paths["target"].parent)
    return recovery


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--repo", help="Absolute Git root: source and installation are always selected together")
    parser.add_argument("--level", default="debug", choices=("trace", "debug", "info", "warn", "error"))
    parser.add_argument("--size-mb", type=int, default=20)
    parser.add_argument("--files", type=int, default=5)
    parser.add_argument("--yes", action="store_true", help="Skip the Ctrl-C window before thegn is stopped")
    raw_argv = sys.argv[1:] if argv is None else list(argv)
    args = parser.parse_args(raw_argv)
    if not 1 <= args.size_mb <= 1024 or not 1 <= args.files <= 100:
        raise Refusal("Log limits must be 1..1024 MiB and 1..100 rotations")
    env = dict(os.environ)
    repo = absolute(args.repo) if args.repo else Path(__file__).resolve().parent.parent
    directories(repo)
    paths = settings(repo, env)
    revision = source_revision(repo, env, require_clean=not args.plan)
    print(json.dumps({key: str(value) for key, value in paths.items()}, indent=2))
    print("Build:", " ".join(BUILD), "(isolated output + intermediate directories)")
    print("Selected revision:", revision, "(plan does not admit dirty source for a build)")
    print("Brand moves disabled with THEGN_NO_MIGRATE=1; database migration policy is not overridden.")
    if args.plan:
        return 0
    if thegn_ancestor() is not None:
        relaunch_outside(raw_argv, env, repo)
        return 0
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        raise Refusal("A real terminal is required: the new controller launches in the foreground")
    with locked(paths["target"].parent / ".thegn-live-install.lock"), locked(paths["state"] / "live-upgrade.lock"):
        step(1, "Preflight: clean source and supported paths")
        source_revision(repo, env)
        step(2, "Build (the running instance keeps working meanwhile)")
        _stage, binary, record = build_stage(paths, env)
        step(3, "Validate the configuration with the NEW build")
        validate_config(binary, env)
        step(4, "Stop the running thegn (controllers, then the pane daemon)")
        if not args.yes:
            countdown(STOP_COUNTDOWN)
        stop_running(paths, env)
        # Re-read policy and path constraints after the potentially long build.
        if settings(repo, env) != paths:
            raise Refusal("Upgrade paths changed while building")
        if source_revision(repo, env) != record["observed_revision"]:
            raise Refusal("Source changed after build; installation refused")
        step(5, "Back up the database and old binary, then install")
        logs = paths["state"] / "logs"
        if logs.exists() or logs.is_symlink():
            directories(logs)  # Existing normal 0755 logs are under private state.
        with locked(Path(str(paths["database"]) + ".schema.lock"), schema=True):
            recovery = install(paths, binary, record, env)
        print(f"  recovery (database backup + previous binary): {recovery}")
        step(6, "Launch (the database migrates on this first start)")
        doctor_report = recovery / "doctor.txt"
        print(f"  `thegn doctor` will be captured to {doctor_report} about {DOCTOR_DELAY}s after launch")
        doctor_later(paths["target"], env, doctor_report)
        child_env = dict(env, THEGN_NO_MIGRATE="1", RUST_BACKTRACE="full", THEGN_LOG=f"{args.level},log=error",
                         THEGN_LOG_ROTATION_SIZE_MB=str(args.size_mb), THEGN_LOG_MAX_FILES=str(args.files), THEGN_PERF="1")
        # Retain the launcher lock in this supervisor for the entire foreground
        # lifetime; schema lock is released so normal startup can migrate.
        fd = os.open(recovery / "thegn-stderr.log", os.O_WRONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            os.close(fd)
            raise Refusal("Reserved launch stderr file changed")
        with os.fdopen(fd, "wb") as stderr:
            child = subprocess.Popen([str(paths["target"])], cwd=repo, env=child_env, stderr=stderr)
            while True:
                try:
                    code = child.wait()
                    break
                except KeyboardInterrupt:
                    print("Foreground child still supervised; waiting for its exit.", file=sys.stderr)
        print(f"Child exited {code}; readiness was not verified. Recovery retained at {recovery}")
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
