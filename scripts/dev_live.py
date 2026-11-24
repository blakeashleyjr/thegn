#!/usr/bin/env python3
"""`just live`: build this checkout when its build inputs changed since the last dev build, then start the DEV instance with it (scripts/devel-launch.sh). Never touches the stable instance: its binary, launchers, state, database and processes."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import pwd
import shutil
import stat
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import instance_procs  # noqa: E402

if sys.platform == "linux":
    import fcntl


BUILD = ["cargo", "build", "--locked", "--release", "--features", "profiling", "-p", "thegn-host", "--bin", "thegn"]  # live.py:42
STAMP_INPUTS = ("Cargo.toml", "Cargo.lock", ".cargo", "crates", "vendor/termwiz", "config", "extensions",
                "docs/help", "docs/cli.md", "docs/api", "README.md", "rust-toolchain", "rust-toolchain.toml", "build.rs")
LIVE_CACHE = "live-cache"   # target/live-cache, shared with the old installer's warm cache
PANE_VARS = ("THEGN_SESSION_ID", "THEGN_CONTROL_SOCKET", "THEGN_WORKTREE", "THEGN_BRANCH", "THEGN_WORKSPACE",
             "THEGN_REPO_ROOT", "THEGN_SANDBOX", "THEGN_SANDBOX_NETWORK", "THEGN_SANDBOX_ENABLED", "THEGN_FORKED_FROM",
             "THEGN_FORK_SCROLLBACK", "THEGN_ROUTE_TOKEN", "THEGN_ROUTE_ORIGIN", "THEGN_PROMPT", "THEGN_EVENT", "THEGN_BIN")  # devel-launch.sh:13 (b173)
REMOVED_FLAGS = ("--relink-launchers", "--install-dir", "--yes", "--no-scope")
MAX_RECORD = 64 * 1024


class Refusal(Exception):
    """A visible refusal to build, install, or launch the dev instance."""


def real_home() -> Path:
    return Path(pwd.getpwuid(os.getuid()).pw_dir)


def dev_paths(home: Path) -> dict:
    root = home / ".thegn-devel"
    return {
        "root": root,
        "state": root / "state",
        "thegn_dir": root / "home",
        "bin": root / "bin" / "thegn",
        "record": root / "bin" / "thegn.build.json",
        "lock": root / "bin" / ".build.lock",
        "config": home / ".config" / "thegn-dev",
        "runfile": root / "run",
    }


def git(repo: Path, *args: str) -> bytes:
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_NO_REPLACE_OBJECTS="1")
    result = subprocess.run(
        ["git", "--no-optional-locks", "--literal-pathspecs", "-c", "core.fsmonitor=false", *args],
        cwd=repo,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        timeout=30,
    )
    return result.stdout


def toolchain_id(env: dict) -> bytes:
    try:
        result = subprocess.run(["rustc", "-vV"], env=env, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, timeout=30)
    except OSError as error:
        raise Refusal("rustc not found; run just live inside nix develop (or direnv allow)") from error
    if result.returncode != 0:
        raise Refusal("rustc not found; run just live inside nix develop (or direnv allow)")
    return result.stdout


def source_stamp(repo: Path, toolchain: bytes) -> dict:
    top = Path(os.fsdecode(git(repo, "rev-parse", "--show-toplevel").rstrip(b"\n")))
    if top != repo:
        raise Refusal(f"Selected source must be the canonical Git checkout root: {repo}; run just live from the checkout root")

    head = git(repo, "rev-parse", "--verify", "HEAD")
    status = git(repo, "status", "--porcelain=v1", "-z", "--untracked-files=normal", "--", *STAMP_INPUTS)
    diff = git(repo, "diff", "HEAD", "--binary", "--no-ext-diff", "--no-textconv", "--", *STAMP_INPUTS)
    untracked = git(repo, "ls-files", "-z", "--others", "--exclude-standard", "--", *STAMP_INPUTS)
    revision = head.decode("ascii").strip()

    hasher = hashlib.sha256()

    def add(part: bytes) -> None:
        hasher.update(part)
        hasher.update(b"\0")

    add(b"thegn-dev-live-v1")
    add(json.dumps(BUILD).encode())
    add(toolchain)
    add(head)
    add(status)
    add(diff)
    for relative in sorted(path for path in untracked.split(b"\0") if path):
        add(relative)
        candidate = repo / os.fsdecode(relative)
        try:
            info = candidate.lstat()
            if stat.S_ISREG(info.st_mode):
                with candidate.open("rb") as stream:
                    add(stream.read())
            elif stat.S_ISLNK(info.st_mode):
                add(os.fsencode(os.readlink(candidate)))
        except OSError:
            continue
    return {"revision": revision, "dirty": bool(status), "stamp": hasher.hexdigest()}


def read_record(path: Path) -> dict | None:
    try:
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_RECORD:
            return None
        with path.open("rb") as stream:
            data = stream.read(MAX_RECORD + 1)
        if len(data) > MAX_RECORD:
            return None
        value = json.loads(data)
    except (OSError, UnicodeDecodeError, ValueError, TypeError):
        return None
    return value if isinstance(value, dict) else None


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def up_to_date(record: dict | None, stamp: dict, binary: Path) -> bool:
    if record is None or record.get("stamp") != stamp["stamp"]:
        return False
    try:
        info = binary.lstat()
        return (stat.S_ISREG(info.st_mode) and info.st_mode & stat.S_IXUSR
                and digest(binary) == record.get("sha256"))
    except OSError:
        return False


def build(repo: Path, env: dict) -> Path:
    cache = repo / "target" / LIVE_CACHE
    cache.mkdir(mode=0o700, parents=True, exist_ok=True)
    build_env = dict(env, RUSTC_WRAPPER="", CARGO_TARGET_DIR=str(cache),
                     CARGO_BUILD_BUILD_DIR=str(cache / "intermediate"),
                     CARGO_BUILD_JOBS=str(os.cpu_count() or 1))
    subprocess.run(BUILD, cwd=repo, env=build_env, check=True)
    return cache / "release" / "thegn"


def _atomic_copy(source: Path, destination: Path, mode: int) -> None:
    fd, temporary = tempfile.mkstemp(dir=destination.parent, prefix=".thegn.")
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as output:
            fd = -1
            with source.open("rb") as input_stream:
                while chunk := input_stream.read(1024 * 1024):
                    output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, destination)
        temporary = None
    finally:
        if fd != -1:
            os.close(fd)
        if temporary is not None:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass


def _atomic_json(value: dict, destination: Path) -> None:
    fd, temporary = tempfile.mkstemp(dir=destination.parent, prefix=".thegn.")
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            fd = -1
            output.write(json.dumps(value))
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, destination)
        temporary = None
    finally:
        if fd != -1:
            os.close(fd)
        if temporary is not None:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass


def install_binary(built: Path, paths: dict, record: dict) -> None:
    paths["bin"].parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    _atomic_copy(built, paths["bin"], 0o755)
    _atomic_json(record, paths["record"])


def dev_env(env: dict, paths: dict, level: str, size_mb: int, files: int) -> dict:
    result = dict(env)
    for name in PANE_VARS:
        result.pop(name, None)
    result.update(
        XDG_STATE_HOME=str(paths["state"]),
        THEGN_DIR=str(paths["thegn_dir"]),
        THEGN_CONFIG_DIR=str(paths["config"]),
        THEGN_NO_MIGRATE="1",
        THEGN_CHANNEL="dev",
        THEGN_DEVEL_BIN=str(paths["bin"]),
        RUST_BACKTRACE="full",
        THEGN_LOG=f"{level},log=error,tokei=error",
        THEGN_LOG_ROTATION_SIZE_MB=str(size_mb),
        THEGN_LOG_MAX_FILES=str(files),
        THEGN_PERF="1",
    )
    if env.get("XDG_RUNTIME_DIR") == str(paths["runfile"]):
        result["XDG_RUNTIME_DIR"] = f"/run/user/{os.getuid()}"
    return result


def validate_config(binary: Path, env: dict) -> None:
    config = env.get("THEGN_CONFIG_DIR")
    if not config or not Path(config).is_dir():
        return
    result = subprocess.run([str(binary), "config", "validate"], env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=60)
    output = result.stdout.decode(errors="replace") if isinstance(result.stdout, bytes) else str(result.stdout)
    lines = [line for line in output.splitlines() if line.strip()]
    for line in lines[-10:]:
        print(line)
    if result.returncode != 0:
        raise Refusal("the new build rejects the dev configuration; the previous dev binary was kept")


def dev_daemons(state: Path, proc: Path = Path("/proc")) -> list[tuple[int, str]]:
    """Daemons of the instance whose XDG_STATE_HOME is `state` (detection shared via instance_procs)."""
    return [(p.pid, p.exe) for p in instance_procs.thegn_processes(state, None, proc) if p.kind == "daemon"]


def _prebuilt(path: str) -> Path:
    binary = Path(path).absolute()
    try:
        info = binary.lstat()
    except OSError as error:
        raise Refusal(f"prebuilt binary is not usable: {binary}") from error
    if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
            or not info.st_mode & stat.S_IXUSR):
        raise Refusal(f"prebuilt binary is not a regular executable owned by this user: {binary}")
    return binary


def _record_reason(record: dict | None, stamp: dict, binary: Path) -> str:
    if record is None:
        return "no previous dev build"
    if record.get("stamp") != stamp["stamp"]:
        return f"inputs changed since {record.get('built_at', 'the previous build')}"
    return "binary missing or modified"


def main(argv: list[str] | None = None, home: Path | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--level", default="info", choices=("trace", "debug", "info", "warn", "error"))
    parser.add_argument("--size-mb", type=int, default=20)
    parser.add_argument("--files", type=int, default=5)
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--prebuilt", metavar="BINARY")
    parser.add_argument("--restart-daemon", action="store_true")
    parser.add_argument("--install-only", action="store_true", help="install the binary and stop there (just update dev restarts and launches itself)")
    parser.add_argument("--here", action="store_true", help="start in this terminal even inside a thegn pane")
    for flag in REMOVED_FLAGS:
        parser.add_argument(flag, action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(sys.argv[1:] if argv is None else list(argv))

    for flag in REMOVED_FLAGS:
        if getattr(args, flag[2:].replace("-", "_")):
            raise Refusal(f"{flag} belonged to the old in-place installer. The stable instance is installed by `just update stable` (docs/help/two-instances.md); just live only builds and starts the dev instance")
    if not 1 <= args.size_mb <= 1024 or not 1 <= args.files <= 100:
        raise Refusal("Log limits must be 1..1024 MiB and 1..100 rotations; pass limits within the stated ranges")
    if sys.platform != "linux" or os.getuid() == 0:
        raise Refusal("just live supports ordinary-user Linux only; run it as your own user on Linux, not root")

    env = dict(os.environ)
    repo = Path(__file__).resolve().parent.parent
    selected_home = home or real_home()
    paths = dev_paths(selected_home)
    account_state = real_home() / ".local" / "state"
    if paths["state"] in (account_state, account_state / "thegn"):
        raise Refusal("dev state must not be the stable state")

    stamp = None
    record = read_record(paths["record"])
    changed = True
    if args.prebuilt is None:
        stamp = source_stamp(repo, toolchain_id(env))
        changed = not up_to_date(record, stamp, paths["bin"])
    else:
        changed = True

    print(f"repo: {repo}")
    if stamp is not None:
        print(f"revision: {stamp['revision']}" + (" (uncommitted edits)" if stamp["dirty"] else ""))
        if changed:
            print(f"build: needed ({_record_reason(record, stamp, paths['bin'])})")
        else:
            print(f"build: skipped (unchanged since {record.get('built_at', 'the previous build')})")
    else:
        print("revision: prebuilt artifact")
        print("build: needed (prebuilt selected)")
    print(f"dev binary: {paths['bin']}")
    observed = dev_daemons(paths["state"])
    for pid, executable in observed:
        marker = " previous build" if executable.endswith(" (deleted)") or executable != str(paths["bin"]) else ""
        print(f"dev daemon pid {pid}: {executable or '(unknown)'}{marker}")
    print("stable instance: not touched")
    if args.plan:
        return 0

    denv = dev_env(env, paths, args.level, args.size_mb, args.files)
    paths["lock"].parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with paths["lock"].open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if args.prebuilt is not None:
            built = _prebuilt(args.prebuilt)
            install_record = {
                "stamp": None,
                "revision": stamp["revision"] if stamp else None,
                "prebuilt": str(built),
                "build": BUILD,
                "sha256": digest(built),
                "built_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            }
            validate_config(built, denv)
            install_binary(built, paths, install_record)
        elif changed:
            built = build(repo, env)
            post = source_stamp(repo, toolchain_id(env))
            stable_stamp = stamp is not None and post["stamp"] == stamp["stamp"]
            if not stable_stamp:
                print("source changed during the build; the next just live rebuilds")
            install_record = {
                "stamp": post["stamp"] if stable_stamp else None,
                "revision": stamp["revision"],
                "dirty": stamp["dirty"],
                "prebuilt": None,
                "build": BUILD,
                "sha256": digest(built),
                "built_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            }
            validate_config(built, denv)
            install_binary(built, paths, install_record)
        fcntl.flock(lock, fcntl.LOCK_UN)

    if args.install_only:
        print("installed; not stopping or launching (--install-only)")
        return 0
    observed = dev_daemons(paths["state"])
    if args.restart_daemon and observed:
        print("stopping the dev daemon: every dev pane (shells, agent workers) ends")
        stopped = instance_procs.stop_instance(
            paths["bin"], denv, lambda: instance_procs.thegn_processes(paths["state"], None), say=print)
        print("dev daemon stopped" if stopped else "dev daemon did not stop")
    elif not args.restart_daemon:
        for pid, executable in observed:
            if executable.endswith(" (deleted)") or executable != str(paths["bin"]):
                print(f"dev daemon pid {pid} still runs the previous build; its panes keep running, daemon-side changes are not live until 'just live <level> <size> <files> --restart-daemon' (ends every dev pane)")

    launcher = repo / "scripts" / "devel-launch.sh"
    if env.get("THEGN_SESSION_ID") and not args.here:
        if shutil.which("alacritty") is None:
            raise Refusal("this shell is a thegn pane; alacritty is needed to open the dev instance in its own window; rerun with --here to start it in this pane")
        subprocess.Popen([str(launcher), "--standalone"], env=denv, cwd=repo, start_new_session=True,
                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        print("dev instance opened in a new window")
        return 0
    os.execve(str(launcher), [str(launcher)], denv)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
    except (Refusal, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"just live refused/failed: {error}", file=sys.stderr)
        sys.exit(1)
