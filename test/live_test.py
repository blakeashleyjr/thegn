#!/usr/bin/env python3
"""Private helper tests: no Cargo, live DB, application launch or live signals."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import select
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("live_helper", ROOT / "scripts/live.py")
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)


@unittest.skipUnless(sys.platform == "linux", "Live upgrade helper is Linux-only")
class LiveTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="thegn-live-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        # User-namespace test runners can expose host-owned / and /tmp as
        # unmapped uid 65534. Model only those fixture ancestors as root-owned;
        # every file/directory created by the test retains its actual metadata.
        # Production continues to refuse unmapped ancestry (tested below).
        original_lstat = Path.lstat
        unmapped = {p for p in self.root.parents if original_lstat(p).st_uid == 65534}

        def fixture_lstat(path, *args, **kwargs):
            info = original_lstat(path, *args, **kwargs)
            if path in unmapped:
                fields = list(info)
                fields[4] = 0
                return os.stat_result(fields)
            return info

        self.ancestor_patch = patch.object(Path, "lstat", fixture_lstat)
        self.ancestor_patch.start()
        self.addCleanup(self.ancestor_patch.stop)
        self.unmapped_ancestors = unmapped
        self.repo = self.root / "repo"
        self.state = self.root / "state/thegn"
        self.config = self.root / "config/thegn/config.toml"
        self.state.mkdir(parents=True, mode=0o700)
        (self.repo / "target/release").mkdir(parents=True)
        self.config.parent.mkdir(parents=True)
        self.target = self.repo / "target/release/thegn"
        self.target.write_bytes(b"old executable")
        self.target.chmod(0o755)
        self.db = self.state / "thegn.db"
        with contextlib.closing(sqlite3.connect(self.db)) as conn:
            conn.execute("CREATE TABLE sample(value TEXT)")
            conn.execute("INSERT INTO sample VALUES ('preserved')")
            conn.execute("PRAGMA user_version=3")
            conn.commit()
        self.db.chmod(0o600)
        self.env = {"XDG_STATE_HOME": str(self.state.parent), "XDG_CONFIG_HOME": str(self.config.parent.parent)}
        self.paths = {"repo": self.repo, "state": self.state, "database": self.db, "target": self.target, "config": self.config}
        self.binary = self.root / "built"
        self.binary.write_bytes(b"new executable")
        self.binary.chmod(0o755)
        self.record = {"schema": 3, "sha256": live.digest(self.binary), "observed_revision": "a" * 40}
        # Production refuses uid0. Fixtures model only this explicit ordinary
        # user guard when the test runner itself is root; all file checks still
        # use actual UID. Tests of settings run only on supported ordinary Linux.

    def settings(self, **extra):
        return live.settings(self.repo, dict(self.env, **extra))

    def test_unmapped_ancestor_remains_a_production_refusal(self):
        self.ancestor_patch.stop()
        try:
            if self.unmapped_ancestors:
                with self.assertRaisesRegex(live.Refusal, "owner/type"):
                    live.directories(self.repo)
            else:
                live.directories(self.repo)
        finally:
            self.ancestor_patch.start()

    def test_platform_and_root_refusal(self):
        with patch.object(live.sys, "platform", "darwin"), self.assertRaises(live.Refusal):
            self.settings()
        with patch.object(live.os, "getuid", return_value=0), self.assertRaises(live.Refusal):
            self.settings()
        with patch.object(live.os, "geteuid", return_value=os.getuid() + 1), self.assertRaises(live.Refusal):
            self.settings()

    def test_settings_paths_pin_and_env_blank_semantics(self):
        if sys.platform != "linux" or os.getuid() == 0:
            self.skipTest("Settings admission requires ordinary-user Linux; explicit refusal tested separately")
        self.assertEqual(self.settings(), self.paths)
        self.assertEqual(self.settings(THEGN_DIR=str(self.root / "other")), self.paths)
        self.config.write_text('[database]\nmigration_executable="/elsewhere/thegn"\n')
        for value in ("", " ", "\t"):
            with self.subTest(blank=value), self.assertRaises((live.Refusal, OSError)):
                self.settings(THEGN_DATABASE_MIGRATION_EXECUTABLE=value)
        self.config.write_text(f'[database]\nmigration_executable={json.dumps(str(self.target))}\n')
        self.assertEqual(self.settings(), self.paths)
        for key, value in (("XDG_STATE_HOME", ""), ("XDG_CONFIG_HOME", "relative"), ("THEGN_PROFILE", "named"),
                           ("THEGN_ALLOW_SCHEMA_DOWNGRADE", "1"), ("THEGN_DATABASE_MIGRATION_AUTHORITY", "disabled"),
                           ("THEGN_DATABASE_UNKNOWN", "x")):
            with self.subTest(key=key), self.assertRaises(live.Refusal):
                self.settings(**{key: value})

    def test_config_malformed_bounds_and_database_hardlink(self):
        if sys.platform != "linux" or os.getuid() == 0:
            self.skipTest("Settings admission requires ordinary-user Linux; explicit refusal tested separately")
        for content in ("[database", '[database]\nmigration_authority="off"', '[database]\nunknown=true', "x=" + " " * live.MAX_CONFIG):
            self.config.write_text(content)
            with self.subTest(content=content[:20]), self.assertRaises(live.Refusal):
                self.settings()
        self.config.write_text("")
        os.link(self.db, self.root / "alias.db")
        with self.assertRaisesRegex(live.Refusal, "Hardlinked"):
            self.settings()

    def test_special_files_and_symlinks_refused_without_blocking(self):
        fifo = self.root / "fifo"
        os.mkfifo(fifo)
        with self.assertRaises(live.Refusal):
            live.read_small(fifo)
        link = self.root / "link"
        link.symlink_to(self.db)
        with self.assertRaises(OSError):
            live.read_small(link)
        with self.assertRaises(live.Refusal):
            live.regular(link)

    def test_lock_modes_lifetime_and_hardlinks(self):
        path = self.state / "thegn.db.schema.lock"
        path.touch(mode=0o644)
        with live.locked(path, schema=True):
            with self.assertRaisesRegex(live.Refusal, "busy"):
                with live.locked(path, schema=True):
                    self.fail("Second holder admitted")
        with live.locked(path, schema=True):
            pass
        os.link(path, self.state / "other.lock")
        with self.assertRaisesRegex(live.Refusal, "Unsafe"):
            with live.locked(path, schema=True):
                self.fail("Hardlinked lock admitted")

    def test_different_states_still_contend_on_target_lock(self):
        target_lock = self.target.parent / ".thegn-live-install.lock"
        another_state = self.root / "another-state"
        another_state.mkdir(mode=0o700)
        with live.locked(target_lock), live.locked(self.state / "live-upgrade.lock"):
            with live.locked(another_state / "live-upgrade.lock"):
                with self.assertRaisesRegex(live.Refusal, "busy"):
                    with live.locked(target_lock):
                        self.fail("Distinct states admitted concurrent target replacement")

    def test_copy_mode_is_executable_even_with_restrictive_umask(self):
        destination = self.root / "installed"
        old_mask = os.umask(0o777)
        try:
            live.copy_file(self.binary, destination, time.monotonic() + 5, 0o755)
        finally:
            os.umask(old_mask)
        self.assertEqual(destination.stat().st_mode & 0o777, 0o755)
        live.regular(destination, executable=True)

    def test_backup_includes_committed_wal_and_metadata(self):
        with contextlib.closing(sqlite3.connect(self.db)) as writer:
            writer.execute("PRAGMA journal_mode=WAL")
            writer.execute("INSERT INTO sample VALUES ('in WAL')")
            writer.commit()
            self.assertTrue(Path(str(self.db) + "-wal").exists())
            recovery = live.backup(self.paths, self.record)
            with contextlib.closing(sqlite3.connect(recovery / "thegn.db")) as copied:
                self.assertEqual(copied.execute("SELECT value FROM sample ORDER BY rowid").fetchall(), [("preserved",), ("in WAL",)])
        record = json.loads((recovery / "complete.json").read_text())
        self.assertEqual(record["schema"], 3)
        self.assertEqual(record["database_sha256"], live.digest(recovery / "thegn.db"))
        self.assertEqual(record["build"], self.record)
        self.assertEqual((recovery / "thegn.previous").read_bytes(), b"old executable")
        self.assertEqual(recovery.stat().st_mode & 0o777, 0o700)

    def test_backup_deadline_newer_schema_and_malformed_leave_target(self):
        for schema in (2,):
            with self.assertRaisesRegex(live.Refusal, "newer"):
                live.backup(self.paths, dict(self.record, schema=schema))
        with patch.object(live, "BACKUP_SECONDS", -1), self.assertRaisesRegex(live.Refusal, "deadline"):
            live.backup(self.paths, self.record)
        self.db.write_bytes(b"not sqlite")
        with self.assertRaises(sqlite3.DatabaseError):
            live.backup(self.paths, self.record)
        self.assertEqual(self.target.read_bytes(), b"old executable")
        self.assertFalse(any(self.state.glob("live-backup-*/complete.json")))

    def test_sqlite_busy_backup_deadline(self):
        with contextlib.closing(sqlite3.connect(self.db)) as writer:
            writer.execute("BEGIN EXCLUSIVE")
            # Deadline creation + one old-binary copy chunk remain timely;
            # expire only at the real busy-backup progress callback. No disk
            # speed or scheduler threshold is part of this regression.
            with patch.object(live.time, "monotonic", side_effect=[0, 0, 1000]) as clock, self.assertRaisesRegex(live.Refusal, "SQLite backup deadline"):
                live.backup(self.paths, self.record)
            self.assertEqual(clock.call_count, 3)
            writer.rollback()
        self.assertEqual(self.target.read_bytes(), b"old executable")

    def test_atomic_install_digest_failure_and_last_observation(self):
        with patch.object(live, "settings", return_value=self.paths), patch.object(live, "quiescent") as observe:
            with patch.object(live.os, "replace", side_effect=OSError("injected replacement failure")):
                with self.assertRaises(OSError):
                    live.install(self.paths, self.binary, self.record, self.env)
            self.assertEqual(observe.call_count, 3)
            self.assertEqual(self.target.read_bytes(), b"old executable")
            self.binary.write_bytes(b"changed staged content")
            with self.assertRaisesRegex(live.Refusal, "artifact changed"):
                live.install(self.paths, self.binary, self.record, self.env)
            self.assertEqual(self.target.read_bytes(), b"old executable")
            self.binary.write_bytes(b"new executable")
            recovery = live.install(self.paths, self.binary, self.record, self.env)
            self.assertEqual(self.target.read_bytes(), b"new executable")
            self.assertEqual((recovery / "thegn.previous").read_bytes(), b"old executable")

    def test_last_observation_prevents_install(self):
        with patch.object(live, "settings", return_value=self.paths), patch.object(live, "quiescent", side_effect=[None, None, live.Refusal("late user")]):
            with self.assertRaisesRegex(live.Refusal, "late user"):
                live.install(self.paths, self.binary, self.record, self.env)
        self.assertEqual(self.target.read_bytes(), b"old executable")

    def test_owned_real_process_open_database_is_refused(self):
        proc = self.root / "proc"
        proc.mkdir()
        code = "import sys; f=open(sys.argv[1],'rb'); print('ready',flush=True); sys.stdin.buffer.read(1)"
        child = subprocess.Popen([sys.executable, "-B", "-c", code, str(self.db)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        try:
            self.assertTrue(select.select([child.stdout], [], [], 5)[0], "Owned fixture did not become ready")
            self.assertEqual(child.stdout.readline(), b"ready\n")
            (proc / str(child.pid)).symlink_to(Path("/proc") / str(child.pid))
            with self.assertRaisesRegex(live.Refusal, "files open"):
                live.quiescent(self.paths, proc)
        finally:
            child.stdin.close()  # Explicit private fixture release; no signals.
            child.wait(timeout=5)
            child.stdout.close()
        self.assertEqual(child.returncode, 0)

    def test_proc_unknown_refuses_other_uid_ignored(self):
        proc = self.root / "proc"
        process = proc / "99999999"
        process.mkdir(parents=True)
        (process / "status").write_text("Uid:\t" + "\t".join([str(os.getuid())] * 4) + "\n")
        with self.assertRaisesRegex(live.Refusal, "remaining process"):
            live.quiescent(self.paths, proc)
        (process / "status").write_text("Uid:\t" + "\t".join([str(os.getuid() + 1)] * 4) + "\n")
        live.quiescent(self.paths, proc)
        with patch.object(live, "read_small", side_effect=PermissionError), self.assertRaisesRegex(live.Refusal, "Cannot inspect"):
            live.quiescent(self.paths, proc)

    def _namespaced(self, proc, pid, name):
        """A process whose `status` reads but whose `exe`/`fd` are root-owned.

        This is what a rootless podman/docker container looks like from the
        host: the real uid is still ours, so the uid filter does not skip it,
        but the kernel hides `exe` and `fd` behind the user namespace.
        """
        process = proc / str(pid)
        process.mkdir(parents=True)
        (process / "status").write_text(
            f"Name:\t{name}\nUid:\t" + "\t".join([str(os.getuid())] * 4) + "\n"
        )
        return process

    def test_namespaced_container_does_not_veto_the_upgrade(self):
        """A rootless container must not refuse an unrelated upgrade.

        Regression: `buildkitd` and `garage` made every `just live` fail with
        "Cannot inspect process ownership/files", undiagnosably and forever.
        """
        proc = self.root / "proc"
        self._namespaced(proc, 99999991, "buildkitd")
        self._namespaced(proc, 99999992, "garage")

        def hidden(path, *args, **kwargs):
            if str(path).endswith("/exe"):
                raise PermissionError(13, "Permission denied", str(path))
            return os.readlink(path, *args, **kwargs)

        with patch.object(live.os, "readlink", side_effect=hidden):
            live.quiescent(self.paths, proc)  # Must not raise.

    def test_namespaced_thegn_is_still_refused_by_name(self):
        """Hiding `exe` must not smuggle a controller past the check."""
        proc = self.root / "proc"
        self._namespaced(proc, 99999993, "thegn")

        def hidden(path, *args, **kwargs):
            if str(path).endswith("/exe"):
                raise PermissionError(13, "Permission denied", str(path))
            return os.readlink(path, *args, **kwargs)

        with patch.object(live.os, "readlink", side_effect=hidden):
            with self.assertRaisesRegex(live.Refusal, "still running"):
                live.quiescent(self.paths, proc)

    def test_namespaced_status_without_a_name_is_refused(self):
        """A hidden process we cannot even name is not waved through."""
        proc = self.root / "proc"
        process = proc / "99999994"
        process.mkdir(parents=True)
        (process / "status").write_text("Uid:\t" + "\t".join([str(os.getuid())] * 4) + "\n")

        def hidden(path, *args, **kwargs):
            if str(path).endswith("/exe"):
                raise PermissionError(13, "Permission denied", str(path))
            return os.readlink(path, *args, **kwargs)

        with patch.object(live.os, "readlink", side_effect=hidden):
            with self.assertRaisesRegex(live.Refusal, "no name"):
                live.quiescent(self.paths, proc)

    def test_source_hidden_flags_dirty_and_root_refused(self):
        def answer(argv, **_kwargs):
            if "--show-toplevel" in argv:
                data = os.fsencode(self.repo) + b"\n"
            elif "ls-files" in argv:
                data = listing
            elif "status" in argv:
                data = status
            else:
                data = b"a" * 40 + b"\n"
            return subprocess.CompletedProcess(argv, 0, stdout=data)

        listing, status = b"H source\0", b""
        with patch.object(live.subprocess, "run", side_effect=answer):
            self.assertEqual(live.source_revision(self.repo, self.env), "a" * 40)
            for listing in (b"h hidden\0", b"S skipped\0", b"s both\0"):
                with self.assertRaisesRegex(live.Refusal, "index flags"):
                    live.source_revision(self.repo, self.env)
            listing, status = b"H source\0", b"?? untracked\n"
            with self.assertRaisesRegex(live.Refusal, "clean"):
                live.source_revision(self.repo, self.env)

    def test_build_isolated_locked_and_source_change_refused(self):
        source = self.repo / "crates/thegn-core/src/db.rs"
        source.parent.mkdir(parents=True)
        source.write_text("pub const SCHEMA_VERSION: i64 = 3;\n")

        def build(argv, cwd, env, check):
            self.assertEqual(argv, live.BUILD)
            self.assertEqual(env["RUSTC_WRAPPER"], "")
            self.assertIn("--locked", argv)
            self.assertEqual(cwd, self.repo)
            output = Path(env["CARGO_TARGET_DIR"])
            self.assertEqual(Path(env["CARGO_BUILD_BUILD_DIR"]).parent, output.parent)
            self.assertNotEqual(output, self.repo / "target")
            (output / "release").mkdir(parents=True)
            shutil.copy2(self.binary, output / "release/thegn")

        with patch.object(live.subprocess, "run", side_effect=build), patch.object(live, "source_revision", side_effect=["a" * 40, "b" * 40]):
            with self.assertRaisesRegex(live.Refusal, "Source changed"):
                live.build_stage(self.paths, dict(self.env, RUSTC_WRAPPER="sccache"))
        self.assertEqual(self.target.read_bytes(), b"old executable")

    def test_build_success_and_cargo_failure_leave_install_untouched(self):
        source = self.repo / "crates/thegn-core/src/db.rs"
        source.parent.mkdir(parents=True)
        source.write_text("pub const SCHEMA_VERSION: i64 = 3;\n")

        def build(argv, cwd, env, check):
            output = Path(env["CARGO_TARGET_DIR"]) / "release/thegn"
            output.parent.mkdir(parents=True)
            shutil.copy2(self.binary, output)

        with patch.object(live, "source_revision", return_value="a" * 40), patch.object(live.subprocess, "run", side_effect=build):
            stage, binary, record = live.build_stage(self.paths, self.env)
            self.assertEqual(record["sha256"], live.digest(binary))
            self.assertEqual(json.loads((stage / "build.json").read_text()), record)
            self.assertEqual(record["observed_revision"], "a" * 40)
            self.assertEqual(record["schema"], 3)
        with patch.object(live, "source_revision", return_value="a" * 40), patch.object(live.subprocess, "run", side_effect=subprocess.CalledProcessError(97, live.BUILD)):
            with self.assertRaises(subprocess.CalledProcessError):
                live.build_stage(self.paths, self.env)
        self.assertEqual(self.target.read_bytes(), b"old executable")

    def test_plan_and_confirmation_have_no_install_effects(self):
        with patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40), patch.object(live, "build_stage") as build:
            before = set(self.root.rglob("*"))
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(live.main(["--plan", "--repo", str(self.repo)]), 0)
            self.assertEqual(set(self.root.rglob("*")), before)
            build.assert_not_called()
            with patch.object(live.sys.stdin, "isatty", return_value=False), self.assertRaisesRegex(live.Refusal, "terminal"):
                live.main(["--repo", str(self.repo)])
            build.assert_not_called()
            build.return_value = (self.root, self.binary, self.record)
            with patch.object(live.sys.stdin, "isatty", return_value=True), patch.object(live.sys.stdout, "isatty", return_value=True), patch("builtins.input", return_value="no"):
                with self.assertRaisesRegex(live.Refusal, "Not confirmed"):
                    live.main(["--repo", str(self.repo)])
            self.assertEqual(self.target.read_bytes(), b"old executable")

    def test_supervisor_retains_launcher_but_releases_schema_lock(self):
        recovery = self.state / "private-recovery"
        recovery.mkdir(mode=0o700)
        (recovery / "thegn-stderr.log").touch(mode=0o600)

        def spawn(argv, **kwargs):
            self.assertEqual(argv, [str(self.target)])
            self.assertEqual(kwargs["env"]["THEGN_NO_MIGRATE"], "1")
            self.assertEqual(kwargs["env"].get("THEGN_DATABASE_MIGRATION_EXECUTABLE"), expected_pin)
            self.assertEqual(kwargs["env"]["THEGN_LOG_ROTATION_SIZE_MB"], "20")
            self.assertEqual(kwargs["env"]["THEGN_LOG"], "debug,log=error")

            class Child:
                def wait(inner):
                    for path in (self.state / "live-upgrade.lock", self.target.parent / ".thegn-live-install.lock"):
                        with self.assertRaisesRegex(live.Refusal, "busy"):
                            with live.locked(path):
                                self.fail("Supervisor lock lost")
                    with live.locked(Path(str(self.db) + ".schema.lock"), schema=True):
                        pass
                    return 7

            return Child()

        def install(*_args):
            for path, schema in ((self.state / "live-upgrade.lock", False), (self.target.parent / ".thegn-live-install.lock", False), (Path(str(self.db) + ".schema.lock"), True)):
                with self.assertRaisesRegex(live.Refusal, "busy"):
                    with live.locked(path, schema=schema):
                        self.fail("An install lock was not held")
            return recovery

        with patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40), patch.object(live, "build_stage", return_value=(self.root, self.binary, self.record)), patch.object(live, "install", side_effect=install), patch.object(live.sys.stdin, "isatty", return_value=True), patch.object(live.sys.stdout, "isatty", return_value=True), patch("builtins.input", return_value="INSTALL AND LAUNCH"), patch.object(live.subprocess, "Popen", side_effect=spawn):
            for expected_pin in (None, str(self.target)):
                # Keep HOME unchanged; test only captured migration override.
                env = dict(os.environ, **self.env)
                if expected_pin is None:
                    env.pop("THEGN_DATABASE_MIGRATION_EXECUTABLE", None)
                else:
                    env["THEGN_DATABASE_MIGRATION_EXECUTABLE"] = expected_pin
                with patch.dict(live.os.environ, env, clear=True):
                    self.assertEqual(live.main(["--repo", str(self.repo)]), 7)

    def test_just_recipe_rejects_literal_shell_injection(self):
        just = shutil.which("just")
        if just is None:
            self.skipTest("just unavailable; Python argument parser tests still run")
        marker = self.root / "must-not-exist"
        result = subprocess.run([just, "live-plan", f"$(touch {marker})"], cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(marker.exists())
        self.assertIn(b"invalid choice", result.stderr)


if __name__ == "__main__":
    unittest.main()
