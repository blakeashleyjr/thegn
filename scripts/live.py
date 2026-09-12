#!/usr/bin/env python3
"""Operator-supervised Linux upgrade; never a shutdown or readiness protocol.

Assumes a stable current-user namespace, not hostile same-UID/root processes.
/proc observations and cooperative flocks cannot exclude legacy restarts. SQLite
backup deadlines are checked between backup steps, not cancellation of kernel I/O.
"""

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import tomllib

if sys.platform == "linux":
    import fcntl

MAX_CONFIG = 1024 * 1024
BACKUP_SECONDS = 60
BUILD = ["cargo", "build", "--locked", "--release", "--features", "profiling", "-p", "thegn-host", "--bin", "thegn"]


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


def build_stage(paths, env):
    revision = source_revision(paths["repo"], env)
    source = read_small(paths["repo"] / "crates/thegn-core/src/db.rs")
    versions = re.findall(rb"^pub const SCHEMA_VERSION: i64 = ([0-9]+);$", source, re.MULTILINE)
    if len(versions) != 1:
        raise Refusal("Cannot determine this source's database schema version")
    schema = int(versions[0])
    stage = Path(tempfile.mkdtemp(prefix=".thegn-live-build-", dir=paths["repo"] / "target"))
    print(f"Retaining staged build at {stage}", flush=True)
    build_env = dict(env, CARGO_TARGET_DIR=str(stage / "output"), CARGO_BUILD_BUILD_DIR=str(stage / "intermediate"))
    subprocess.run(BUILD, cwd=paths["repo"], env=build_env, check=True)
    if source_revision(paths["repo"], env) != revision:
        raise Refusal("Source changed during build; artifact retained but not admitted")
    binary = stage / "output/release/thegn"
    regular(binary, executable=True)
    # Observed Git metadata, not exact commit materialization/content proof.
    record = {"repo": str(paths["repo"]), "observed_revision": revision, "schema": schema, "sha256": digest(binary), "build": BUILD}
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
    with (recovery / "complete.json").open("x") as stream:
        json.dump({"database": str(paths["database"]), "target": str(paths["target"]), "schema": version,
                   "binary_sha256": digest(recovery / "thegn.previous"), "database_sha256": digest(output), "build": record}, stream)
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
    args = parser.parse_args(argv)
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
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        raise Refusal("A real terminal and explicit install confirmation are required")
    with locked(paths["target"].parent / ".thegn-live-install.lock"), locked(paths["state"] / "live-upgrade.lock"):
        _stage, binary, record = build_stage(paths, env)
        print("Save work. Manually stop ALL thegn controllers/daemons and disable automatic restarts.")
        if input("Type INSTALL AND LAUNCH to confirm backups, replacement and normal startup: ") != "INSTALL AND LAUNCH":
            raise Refusal("Not confirmed; staged build retained, installed executable unchanged")
        # Re-read policy and path constraints after the potentially long build.
        if settings(repo, env) != paths:
            raise Refusal("Upgrade paths changed while building")
        if source_revision(repo, env) != record["observed_revision"]:
            raise Refusal("Source changed after build; installation refused")
        logs = paths["state"] / "logs"
        if logs.exists() or logs.is_symlink():
            directories(logs)  # Existing normal 0755 logs are under private state.
        with locked(Path(str(paths["database"]) + ".schema.lock"), schema=True):
            recovery = install(paths, binary, record, env)
        child_env = dict(env, THEGN_NO_MIGRATE="1", RUST_BACKTRACE="full", THEGN_LOG=args.level,
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
