#!/usr/bin/env python3
"""Reproduce host-Git helper execution with synthetic config and marker files.
Uses production status argv shape, not a compiled Thegn UI invocation.
"""
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile


with tempfile.TemporaryDirectory(prefix="thegn-hostgit-audit-") as directory:
    root = Path(directory)
    home = root / "home"
    repo = root / "repo"
    home.mkdir()
    repo.mkdir()
    env = {"PATH": os.environ["PATH"], "HOME": str(home),
           "XDG_CONFIG_HOME": str(home / "config"), "LC_ALL": "C",
           "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
           "GIT_OPTIONAL_LOCKS": "0", "GIT_TERMINAL_PROMPT": "0"}

    def git(*args):
        return subprocess.run(["git", "-c", "user.name=Audit",
                               "-c", "user.email=audit@example.invalid",
                               "-c", "commit.gpgSign=false", "-C", str(repo), *args],
                              env=env, capture_output=True, check=True, timeout=20)

    git("init", "-q", "-b", "main")
    (repo / "tracked").write_text("synthetic baseline\n")
    git("add", ".")
    git("commit", "-qm", "baseline")
    monitor_marker = root / "fsmonitor-marker"
    monitor = root / "monitor.sh"
    monitor.write_text("#!/bin/sh\nprintf marker > " + shlex.quote(str(monitor_marker)) +
                       "\nprintf 'audit-token\\000'\n")
    monitor.chmod(0o700)
    git("config", "core.fsmonitor", str(monitor))
    git("config", "core.fsmonitorHookVersion", "2")
    status = git("status", "--porcelain=v1", "-z", "--no-renames")
    assert monitor_marker.read_text() == "marker"
    hook_marker = root / "postcommit-marker"
    hook = repo / ".git/hooks/post-commit"
    hook.write_text("#!/bin/sh\nprintf marker > " + shlex.quote(str(hook_marker)) + "\n")
    hook.chmod(0o700)
    (repo / "tracked").write_text("synthetic change\n")
    git("add", "tracked")
    commit = git("commit", "--no-verify", "--no-gpg-sign", "-m", "canary")
    assert hook_marker.read_text() == "marker"
    result = {"status": {"returncode": status.returncode, "fsmonitor_canary_written": True},
              "commit_no_verify": {"returncode": commit.returncode, "post_commit_canary_written": True},
              "scope": "Direct Git primitive test; production local status argv; synthetic HOME/config; owned temporary markers; no network or Thegn UI invocation"}
print(json.dumps(result, indent=2, sort_keys=True))
