"""Shared by scripts/dev_live.py and scripts/update_instance.py: find the thegn processes of ONE instance
(its daemon and UI clients) and stop them with an escalation ladder. An instance is identified by its
state dir (the process's XDG_STATE_HOME) and, when given, its binary path. A process of another
instance is never matched, so it is never signalled. Standard library only."""

import os
from pathlib import Path
import signal
import subprocess
import time
from typing import NamedTuple

DELETED = " (deleted)"


class Proc(NamedTuple):
    pid: int
    kind: str       # "daemon" or "client"
    exe: str        # readlink of /proc/<pid>/exe, with any " (deleted)" suffix
    replaced: bool  # the binary on disk is no longer the one this process runs


def thegn_processes(state: Path, binary: Path | None = None, proc: Path = Path("/proc"),
                    unset_state_ok: bool = False) -> list[Proc]:
    """Processes whose XDG_STATE_HOME is `state` (or unset, when `unset_state_ok`: the default state
    dir) and, if `binary` is given, whose exe is that path with or without ` (deleted)`. Without
    `binary` any `thegn`/`tg` executable matches (the old `dev_daemons` rule)."""
    found = []
    wanted = b"XDG_STATE_HOME=" + os.fsencode(state)
    for entry in proc.iterdir():
        if not entry.name.isdigit() or int(entry.name) == os.getpid():
            continue
        try:
            executable = os.readlink(entry / "exe")
            bare = executable.removesuffix(DELETED)
            if binary is not None:
                if bare != str(binary):
                    continue
            elif os.path.basename(bare) not in ("thegn", "tg"):
                continue
            argv = (entry / "cmdline").read_bytes().split(b"\0")
            environment = (entry / "environ").read_bytes().split(b"\0")
            has_state = any(item.startswith(b"XDG_STATE_HOME=") for item in environment)
            if wanted not in environment and not (unset_state_ok and not has_state):
                continue
            kind = "daemon" if len(argv) >= 2 and argv[1] == b"daemon" else "client"
            found.append(Proc(int(entry.name), kind, executable, executable.endswith(DELETED)))
        except OSError:
            continue
    return sorted(found)


def stop_instance(binary: Path, env: dict, find, say=print, run=subprocess.run, kill=os.kill,
                  sleep=time.sleep, clock=time.monotonic, grace: float = 20.0, term_wait: float = 5.0,
                  kill_wait: float = 3.0) -> bool:
    """`<binary> daemon stop`, then wait; SIGTERM, wait; SIGKILL, wait. `find()` re-lists the instance's
    processes each round, so only pids that still match the instance are ever signalled. True when
    none remain."""
    targets = {p.pid for p in find()}
    if not targets:
        return True

    def survivors() -> set[int]:
        return {p.pid for p in find()} & targets

    def wait(seconds: float) -> set[int]:
        deadline = clock() + seconds
        left = survivors()
        while left and clock() < deadline:
            sleep(0.25)
            left = survivors()
        return left

    try:
        result = run([str(binary), "daemon", "stop"], env=env, timeout=90, check=False)
        say(f"daemon stop exited with code {result.returncode}")
    except (OSError, subprocess.SubprocessError) as error:
        say(f"daemon stop failed to run: {error}")
    left = wait(grace)
    for name, signum, seconds in (("SIGTERM", signal.SIGTERM, term_wait), ("SIGKILL", signal.SIGKILL, kill_wait)):
        if not left:
            break
        say(f"still running after the previous step: pids {sorted(left)}; sending {name}")
        for pid in sorted(left):
            try:
                kill(pid, signum)
            except OSError:
                pass
        left = wait(seconds)
    if left:
        say(f"could not stop pids {sorted(left)}")
    return not left
