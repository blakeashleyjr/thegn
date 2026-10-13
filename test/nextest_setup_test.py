#!/usr/bin/env python3
"""Regression tests for the cargo-nextest isolate-state setup script."""

import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / ".config" / "nextest.toml"


def setup_command():
    with CONFIG.open("rb") as config_file:
        config = tomllib.load(config_file)
    command = config["scripts"]["setup"]["isolate-state"]["command"]
    if (not isinstance(command, list) or len(command) != 3 or
            command[:2] != ["sh", "-c"] or not isinstance(command[2], str)):
        raise AssertionError("isolate-state command must be ['sh', '-c', <script>]")
    return command[2]


def run(script, tmpdir, path_prefix=None):
    env = os.environ.copy()
    path = env.get("PATH", "")
    if path_prefix is not None:
        path = f"{path_prefix}{os.pathsep}{path}"
    env.update({
        "PATH": path,
        "TMPDIR": str(tmpdir),
        "NEXTEST_ENV": str(Path(tmpdir) / "env"),
    })
    return subprocess.run(
        ["sh", "-c", script], env=env, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, timeout=10,
    )


class NextestSetupTests(unittest.TestCase):
    def temporary_directory(self):
        directory = tempfile.TemporaryDirectory()
        Path(directory.name).chmod(0o700)
        self.addCleanup(directory.cleanup)
        return Path(directory.name)

    def test_vanishing_entry_does_not_fail_setup(self):
        tmpdir = self.temporary_directory()
        stub_dir = tmpdir / "bin"
        stub_dir.mkdir()
        find_stub = stub_dir / "find"
        find_stub.write_text(
            "#!/bin/sh\n"
            'echo "find: \'$TMPDIR/vanished\': No such file or directory" >&2\n'
            "exit 1\n",
            encoding="utf-8",
        )
        find_stub.chmod(0o700)

        result = run(setup_command(), tmpdir, stub_dir)

        self.assertEqual(result.returncode, 0, result.stderr.decode())
        lines = (tmpdir / "env").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 1)
        self.assertTrue(lines[0].startswith("XDG_STATE_HOME="))
        state_dir = Path(lines[0].removeprefix("XDG_STATE_HOME="))
        self.assertEqual(state_dir.parent, tmpdir)
        self.assertTrue(state_dir.name.startswith("thegn-nextest-state."))
        self.assertTrue(state_dir.is_dir())

    def test_stale_dirs_are_still_swept(self):
        tmpdir = self.temporary_directory()
        stale_dir = tmpdir / "thegn-nextest-state.old"
        fresh_dir = tmpdir / "thegn-nextest-state.new"
        stale_dir.mkdir()
        fresh_dir.mkdir()
        old_time = stale_dir.stat().st_mtime - 2 * 24 * 60 * 60
        os.utime(stale_dir, (old_time, old_time))

        result = run(setup_command(), tmpdir)

        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertFalse(stale_dir.exists())
        self.assertTrue(fresh_dir.is_dir())
        created_dirs = list(tmpdir.glob("thegn-nextest-state.*"))
        self.assertEqual(len(created_dirs), 2)
        env_lines = (tmpdir / "env").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(env_lines), 1)

    def test_mktemp_failure_still_fails(self):
        parent = self.temporary_directory()
        missing_tmpdir = parent / "missing"

        result = run(setup_command(), missing_tmpdir)

        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((parent / "env").exists())


if __name__ == "__main__":
    unittest.main()
