#!/usr/bin/env python3
"""Regression tests for the isolated development live launcher."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("dev_live_test", ROOT / "scripts/dev_live.py")
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)


@unittest.skipUnless(os.sys.platform == "linux", "dev live is Linux-only")
class DevLiveTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="thegn-dev-live-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def git(self, *args):
        return subprocess.run(["git", "-c", "commit.gpgsign=false", *args], cwd=self.repo,
                              check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=30)

    def make_repo(self):
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Dev Live Test")
        self.git("config", "user.email", "dev-live@example.invalid")
        for relative, contents in {
            "crates/a.rs": "fn a() {}\n",
            "docs/x.md": "docs\n",
            "README.md": "readme\n",
        }.items():
            path = self.repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)
        self.git("add", ".")
        self.git("commit", "-qm", "initial")

    def test_source_stamp_tracks_only_build_inputs_and_toolchain(self):
        self.make_repo()
        first = live.source_stamp(self.repo, b"rustc 1")
        self.assertEqual(first, live.source_stamp(self.repo, b"rustc 1"))

        (self.repo / "crates/a.rs").write_text("fn changed() {}\n")
        changed_source = live.source_stamp(self.repo, b"rustc 1")
        self.assertNotEqual(first["stamp"], changed_source["stamp"])
        (self.repo / "crates/a.rs").write_text("fn changed again() {}\n")
        current_source = live.source_stamp(self.repo, b"rustc 1")
        self.assertNotEqual(changed_source["stamp"], current_source["stamp"])

        (self.repo / "docs/x.md").write_text("untracked by the stamp\n")
        self.assertEqual(current_source["stamp"], live.source_stamp(self.repo, b"rustc 1")["stamp"])
        (self.repo / "README.md").write_text("changed readme\n")
        self.assertNotEqual(changed_source["stamp"], live.source_stamp(self.repo, b"rustc 1")["stamp"])

        untracked = self.repo / "crates/b.rs"
        untracked.write_text("fn b() {}\n")
        with_untracked = live.source_stamp(self.repo, b"rustc 1")
        self.assertNotEqual(changed_source["stamp"], with_untracked["stamp"])
        untracked.write_text("fn b_changed() {}\n")
        self.assertNotEqual(with_untracked["stamp"], live.source_stamp(self.repo, b"rustc 1")["stamp"])

        self.git("add", ".")
        self.git("commit", "-qm", "source changes")
        committed = live.source_stamp(self.repo, b"rustc 1")
        self.assertNotEqual(with_untracked["stamp"], committed["stamp"])
        self.assertNotEqual(committed["stamp"], live.source_stamp(self.repo, b"rustc 2")["stamp"])

    def test_up_to_date_rejects_missing_changed_and_modified_artifacts(self):
        binary = self.root / "thegn"
        binary.write_bytes(b"binary")
        binary.chmod(0o755)
        stamp = {"stamp": "stamp"}
        record = {"stamp": "stamp", "sha256": live.digest(binary)}
        self.assertFalse(live.up_to_date(None, stamp, binary))
        self.assertFalse(live.up_to_date({"stamp": "other", "sha256": record["sha256"]}, stamp, binary))
        self.assertTrue(live.up_to_date(record, stamp, binary))
        binary.write_bytes(b"modified")
        self.assertFalse(live.up_to_date(record, stamp, binary))
        binary.unlink()
        self.assertFalse(live.up_to_date(record, stamp, binary))

    def test_build_scopes_cargo_outputs_to_each_worktree(self):
        worktrees = [self.root / "worktree-a", self.root / "worktree-b"]
        for worktree in worktrees:
            worktree.mkdir()
        ambient = {
            "CARGO_TARGET_DIR": "/shared/cargo-target",
            "CARGO_BUILD_BUILD_DIR": "/shared/cargo-intermediate",
            "RUSTC_WRAPPER": "sccache",
        }
        with patch.object(live.subprocess, "run") as run:
            binaries = [live.build(worktree, ambient) for worktree in worktrees]

        expected_targets = [worktree / "target" / live.LIVE_CACHE for worktree in worktrees]
        self.assertEqual(binaries, [target / "release" / "thegn" for target in expected_targets])
        self.assertNotEqual(expected_targets[0], expected_targets[1])
        self.assertEqual(run.call_count, 2)
        for call, target in zip(run.call_args_list, expected_targets):
            build_env = call.kwargs["env"]
            self.assertEqual(build_env["CARGO_TARGET_DIR"], str(target))
            self.assertEqual(build_env["CARGO_BUILD_BUILD_DIR"], str(target / "intermediate"))
            self.assertEqual(build_env["RUSTC_WRAPPER"], "")
            self.assertEqual(call.kwargs["cwd"], target.parent.parent)

    def test_main_builds_once_then_rebuilds_after_input_edit(self):
        self.make_repo()
        home = self.root / "home"
        home.mkdir()
        source_file = self.repo / "scripts" / "dev_live.py"
        source_file.parent.mkdir()
        source_file.write_text("placeholder")
        built = self.root / "built"
        built.write_bytes(b"fake executable")
        built.chmod(0o755)
        calls = []

        def fake_build(repo, _env):
            calls.append(repo)
            return built

        execve_mock = MagicMock()
        common = [
            patch.object(live, "__file__", str(source_file)),
            patch.object(live, "real_home", return_value=home),
            patch.object(live.os, "getuid", return_value=1000),
            patch.object(live, "toolchain_id", return_value=b"rustc 1"),
            patch.object(live, "build", side_effect=fake_build),
            patch.object(live, "validate_config"),
            patch.object(live, "dev_daemons", return_value=[]),
            patch.object(live.os, "execve", new=execve_mock),
            patch.dict(live.os.environ, {"THEGN_SESSION_ID": ""}),
        ]
        paths = live.dev_paths(home)
        with common[0], common[1], common[2], common[3], common[4], common[5], common[6], common[7], common[8]:
            live.main([], home=home)
        self.assertEqual(execve_mock.call_args.args[2]["THEGN_DEVEL_BIN"], str(paths["bin"]))
        self.assertEqual(len(calls), 1)
        self.assertEqual(paths["bin"].stat().st_mode & 0o777, 0o755)
        self.assertEqual(paths["record"].stat().st_mode & 0o777, 0o600)
        self.assertEqual(live.read_record(paths["record"])["sha256"], live.digest(paths["bin"]))

        with common[0], common[1], common[2], common[3], common[4], common[5], common[6], common[7], common[8]:
            live.main([], home=home)
        self.assertEqual(len(calls), 1)
        (self.repo / "crates/a.rs").write_text("fn edited() {}\n")
        with common[0], common[1], common[2], common[3], common[4], common[5], common[6], common[7], common[8]:
            live.main([], home=home)
        self.assertEqual(len(calls), 2)

    def test_removed_flags_are_refused(self):
        with self.assertRaisesRegex(live.Refusal, "just update stable"):
            live.main(["--relink-launchers"], home=self.root / "home")

    def test_dev_env_scrubs_panes_preserves_runtime_and_maps_old_run(self):
        home = self.root / "home"
        paths = live.dev_paths(home)
        inherited = {name: "value" for name in live.PANE_VARS}
        inherited["XDG_RUNTIME_DIR"] = "/run/user/original"
        result = live.dev_env(inherited, paths, "debug", 21, 6)
        for name in live.PANE_VARS:
            self.assertNotIn(name, result)
        self.assertEqual(result["XDG_RUNTIME_DIR"], "/run/user/original")
        self.assertEqual(live.dev_env({}, paths, "info", 20, 5).get("XDG_RUNTIME_DIR"), None)
        inherited["XDG_RUNTIME_DIR"] = str(paths["runfile"])
        with patch.object(live.os, "getuid", return_value=1000):
            self.assertEqual(live.dev_env(inherited, paths, "info", 20, 5)["XDG_RUNTIME_DIR"], "/run/user/1000")
        self.assertEqual(result["XDG_STATE_HOME"], str(paths["state"]))
        self.assertEqual(result["THEGN_LOG"], "debug,log=error,tokei=error")
        self.assertEqual(result["THEGN_LOG_ROTATION_SIZE_MB"], "21")
        self.assertEqual(result["THEGN_LOG_MAX_FILES"], "6")
        self.assertEqual(result["THEGN_PERF"], "1")

    def test_launcher_environment_matches_dev_env(self):
        home = self.root / "home"
        paths = live.dev_paths(home)
        stub = self.root / "bin"
        stub.mkdir()
        getent = stub / "getent"
        getent.write_text("#!/bin/sh\nexit 1\n")
        getent.chmod(0o755)
        env = dict(os.environ, HOME=str(home), PATH=f"{stub}:{os.environ['PATH']}")
        output = subprocess.run(["bash", str(ROOT / "scripts/devel-launch.sh"), "--print-env"],
                                env=env, check=True, stdout=subprocess.PIPE, text=True,
                                timeout=30).stdout
        launcher = {}
        for line in output.splitlines():
            if line.startswith("export "):
                name, value = line[len("export "):].split("=", 1)
                launcher[name] = value.strip("'")
        expected = live.dev_env({}, paths, "info", 20, 5)
        for name in ("XDG_STATE_HOME", "THEGN_DIR", "THEGN_CONFIG_DIR", "THEGN_CHANNEL", "THEGN_NO_MIGRATE"):
            self.assertEqual(launcher[name], expected[name])

    def test_dev_daemons_filter_state_and_command(self):
        proc = self.root / "proc"
        state = self.root / "dev-state"
        stable = self.root / "stable-state"
        state.mkdir()
        stable.mkdir()
        for pid, pid_state, command, executable in (
            (31, state, b"thegn\0daemon\0", "/x/thegn (deleted)"),
            (32, stable, b"thegn\0daemon\0", "/x/thegn"),
            (33, state, b"thegn\0shell\0", "/x/thegn"),
        ):
            entry = proc / str(pid)
            entry.mkdir(parents=True)
            (entry / "cmdline").write_bytes(command)
            (entry / "environ").write_bytes(b"XDG_STATE_HOME=" + os.fsencode(pid_state) + b"\0")
            os.symlink(executable, entry / "exe")
        with patch.object(live.os, "getpid", return_value=999):
            self.assertEqual(live.dev_daemons(state, proc), [(31, "/x/thegn (deleted)")])

    def test_in_pane_requires_here_or_alacritty(self):
        self.make_repo()
        home = self.root / "home"
        home.mkdir()
        source_file = self.repo / "scripts" / "dev_live.py"
        source_file.parent.mkdir()
        source_file.write_text("placeholder")
        built = self.root / "built"
        built.write_bytes(b"fake executable")
        built.chmod(0o755)
        with patch.object(live, "__file__", str(source_file)), patch.object(live, "real_home", return_value=home), \
                patch.object(live.os, "getuid", return_value=1000), patch.dict(live.os.environ, {"THEGN_SESSION_ID": "pane", "PATH": os.environ["PATH"]}, clear=True), \
                patch.object(live, "toolchain_id", return_value=b"rustc 1"), patch.object(live, "build", return_value=built), \
                patch.object(live, "validate_config"), patch.object(live, "install_binary"), patch.object(live, "dev_daemons", return_value=[]), \
                patch.object(live.shutil, "which", return_value=None):
            with self.assertRaisesRegex(live.Refusal, "--here"):
                live.main([], home=home)


if __name__ == "__main__":
    unittest.main()
