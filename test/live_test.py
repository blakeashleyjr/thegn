#!/usr/bin/env python3
"""Private helper tests: no Cargo, live DB, application launch or live signals."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
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
        # The scans skip the scanner's own pid. Fake /proc trees use small
        # pids (31, 42, ...), which a fresh pid namespace can hand to this
        # very process: pin the "own pid" to one no fixture uses.
        own_pid = patch.object(live.os, "getpid", return_value=10**7)
        own_pid.start()
        self.addCleanup(own_pid.stop)
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
        # The account home comes from passwd; never the real one in tests.
        self.home = self.root / "home"
        self.state = self.home / ".local/state/thegn"
        self.config = self.root / "config/thegn/config.toml"
        self.state.mkdir(parents=True, mode=0o700)
        (self.repo / "target/release").mkdir(parents=True)
        (self.repo / "config").mkdir()
        (self.repo / "config/alacritty.toml").write_text("[font]\nsize = 11\n")
        self.config.parent.mkdir(parents=True)
        self.install = self.home / ".local/lib/thegn"
        self.install.mkdir(parents=True)
        self.home_patch = patch.object(live, "real_home", return_value=self.home)
        self.home_patch.start()
        self.addCleanup(self.home_patch.stop)
        self.legacy = self.repo / "target/release/thegn"
        self.target = self.install / "thegn"
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
        self.paths = {"repo": self.repo, "state": self.state, "database": self.db, "target": self.target, "config": self.config,
                      "install_dir": self.install, "previous": self.target, "alacritty": self.install / "alacritty.toml",
                      "alacritty_source": self.repo / "config/alacritty.toml"}
        self.binary = self.root / "built"
        self.binary.write_bytes(b"new executable")
        self.binary.chmod(0o755)
        self.record = {"schema": 3, "sha256": live.digest(self.binary), "observed_revision": "a" * 40}
        # Production refuses uid0. Fixtures model only this explicit ordinary
        # user guard when the test runner itself is root; all file checks still
        # use actual UID. Tests of settings run only on supported ordinary Linux.

    def settings(self, **extra):
        return live.settings(self.repo, dict(self.env, **extra))

    @contextlib.contextmanager
    def private_quiescence(self):
        proc = self.root / "private-proc"
        proc.mkdir(mode=0o700)
        real_quiescent = live.quiescent

        def isolated_quiescent(paths):
            return real_quiescent(paths, proc)

        with patch.object(live, "quiescent", side_effect=isolated_quiescent):
            yield

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
            recovery = live.backup(self.paths, dict(self.record, stage=str(self.repo / "target" / ".thegn-live-build-transient")))
            with contextlib.closing(sqlite3.connect(recovery / "thegn.db")) as copied:
                self.assertEqual(copied.execute("SELECT value FROM sample ORDER BY rowid").fetchall(), [("preserved",), ("in WAL",)])
        record = json.loads((recovery / "complete.json").read_text())
        self.assertEqual(record["schema"], 3)
        self.assertEqual(record["database_sha256"], live.digest(recovery / "thegn.db"))
        self.assertEqual(record["build"], self.record)
        self.assertNotIn("stage", record["build"])
        self.assertEqual((recovery / "thegn.previous").read_bytes(), b"old executable")
        self.assertEqual(recovery.stat().st_mode & 0o777, 0o700)

    def test_repeated_success_backups_do_not_pin_owned_stages(self):
        first = live.backup(self.paths, dict(self.record, stage=str(self.repo / "target" / ".thegn-live-build-old")))
        self._staged_fixture(live.LIVE_STAGE_PREFIX + "old", 100)
        self._staged_fixture(live.LIVE_STAGE_PREFIX + "middle", 10)
        newest = self._staged_fixture(live.LIVE_STAGE_PREFIX + "newest", 1)
        second = live.backup(self.paths, dict(self.record, stage=str(newest)))

        self.assertEqual(live.retain_live_stages(self.paths), 1)
        self.assertFalse((self.repo / "target" / (live.LIVE_STAGE_PREFIX + "old")).exists())
        self.assertTrue((self.repo / "target" / (live.LIVE_STAGE_PREFIX + "middle")).exists())
        self.assertTrue(newest.exists())
        self.assertEqual((first / "thegn.previous").read_bytes(), b"old executable")
        self.assertEqual((second / "thegn.previous").read_bytes(), b"old executable")
        for recovery in (first, second):
            self.assertNotIn("stage", json.loads((recovery / "complete.json").read_text())["build"])

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
            # One persistent private cache, reused across runs, never the
            # shared target/ (whose release/thegn IS the installation).
            self.assertEqual(output, self.repo / "target" / live.LIVE_CACHE)
            self.assertEqual(Path(env["CARGO_BUILD_BUILD_DIR"]).parent, output)
            self.assertEqual(env["CARGO_BUILD_JOBS"], str(os.cpu_count() or 1))
            (output / "release").mkdir(parents=True, exist_ok=True)
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
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(self.binary, output)

        with patch.object(live, "source_revision", return_value="a" * 40), patch.object(live.subprocess, "run", side_effect=build):
            stage, binary, record = live.build_stage(self.paths, self.env)
            self.assertEqual(record["sha256"], live.digest(binary))
            # The admitted artifact is a copy in the stage, not the cache's
            # binary that the next build overwrites.
            self.assertEqual(binary, stage / "output/release/thegn")
            self.assertEqual(binary.read_bytes(), b"new executable")
            self.assertEqual(json.loads((stage / "build.json").read_text()), record)
            self.assertEqual(record["observed_revision"], "a" * 40)
            self.assertEqual(record["schema"], 3)
        with patch.object(live, "source_revision", return_value="a" * 40), patch.object(live.subprocess, "run", side_effect=subprocess.CalledProcessError(97, live.BUILD)):
            with self.assertRaises(subprocess.CalledProcessError):
                live.build_stage(self.paths, self.env)
        failed = max((path for path in (self.repo / "target").glob(live.LIVE_STAGE_PREFIX + "*")), key=lambda path: path.lstat().st_mtime_ns)
        self.assertEqual((failed / live.LIVE_STAGE_MARKER).read_bytes(), b"thegn-live-build-v1\n")
        self.assertEqual(self.target.read_bytes(), b"old executable")

    def _staged_fixture(self, name, age, marker=True):
        stage = self.repo / "target" / name
        stage.mkdir(mode=0o700)
        (stage / "output").mkdir(mode=0o700)
        (stage / "output" / "thegn").write_bytes(b"staged")
        (stage / live.LIVE_STAGE_LOCK).touch(mode=0o600)
        if marker:
            (stage / live.LIVE_STAGE_MARKER).write_bytes(b"thegn-live-build-v1\n")
            (stage / live.LIVE_STAGE_MARKER).chmod(0o600)
        stamp = time.time() - age
        os.utime(stage, (stamp, stamp), follow_symlinks=False)
        return stage

    def test_stage_retention_keeps_two_owned_newest_and_unknown_entries(self):
        stages = [self._staged_fixture(live.LIVE_STAGE_PREFIX + str(index), 100 - index) for index in range(4)]
        unknown = self._staged_fixture(live.LIVE_STAGE_PREFIX + "unmarked", 200, marker=False)
        self.assertEqual(live.retain_live_stages(self.paths), 2)
        self.assertTrue(stages[2].exists())
        self.assertTrue(stages[3].exists())
        self.assertFalse(stages[0].exists())
        self.assertFalse(stages[1].exists())
        self.assertTrue(unknown.exists())

    def test_stage_validation_and_cleanup_have_separate_entry_budgets(self):
        stages = [self._staged_fixture(live.LIVE_STAGE_PREFIX + str(index), 100 - index) for index in range(3)]
        with patch.object(live, "MAX_STAGE_ENTRIES", 6):
            self.assertEqual(live.retain_live_stages(self.paths), 1)
        self.assertFalse(stages[0].exists())
        self.assertTrue(stages[1].exists())
        self.assertTrue(stages[2].exists())

    def test_stage_retention_allows_internal_cargo_hardlinks(self):
        old = self._staged_fixture(live.LIVE_STAGE_PREFIX + "old", 100)
        self._staged_fixture(live.LIVE_STAGE_PREFIX + "middle", 10)
        self._staged_fixture(live.LIVE_STAGE_PREFIX + "newest", 1)
        outside = self.root / "cargo-deps-thegn"
        os.link(old / "output" / "thegn", outside)

        self.assertEqual(live.retain_live_stages(self.paths), 1)
        self.assertFalse(old.exists())
        self.assertEqual(outside.read_bytes(), b"staged")

    def test_stage_retention_preserves_active_recovery_linked_and_unsafe_trees(self):
        active = self._staged_fixture(live.LIVE_STAGE_PREFIX + "active", 400)
        linked = self._staged_fixture(live.LIVE_STAGE_PREFIX + "linked", 300)
        unsafe = self._staged_fixture(live.LIVE_STAGE_PREFIX + "unsafe", 200)
        retained = self._staged_fixture(live.LIVE_STAGE_PREFIX + "retained", 100)
        newest = self._staged_fixture(live.LIVE_STAGE_PREFIX + "newest", 1)
        with live.locked(active / live.LIVE_STAGE_LOCK):
            recovery = self.state / "live-backup-test"
            recovery.mkdir(mode=0o700)
            (recovery / "complete.json").write_text(json.dumps({"artifact": str(linked / "output" / "thegn")}))
            (unsafe / "output" / "escape").symlink_to(self.binary)
            self.assertEqual(live.retain_live_stages(self.paths), 0)
        self.assertTrue(active.exists())
        self.assertTrue(linked.exists())
        self.assertTrue(unsafe.exists())
        self.assertTrue(newest.exists())

    def test_stage_retention_fails_closed_for_bad_recovery_and_symlink_lock(self):
        stages = [self._staged_fixture(live.LIVE_STAGE_PREFIX + str(index), 100 - index) for index in range(3)]
        bad_recovery = self.state / "live-backup-incomplete"
        bad_recovery.mkdir(mode=0o700)
        (bad_recovery / "complete.json").write_text("{incomplete")
        self.assertEqual(live.retain_live_stages(self.paths), 0)
        self.assertTrue(all(stage.exists() for stage in stages))

        shutil.rmtree(bad_recovery)
        symlink_lock = stages[0]
        (symlink_lock / live.LIVE_STAGE_LOCK).unlink()
        (symlink_lock / live.LIVE_STAGE_LOCK).symlink_to(self.binary)
        self.assertEqual(live.retain_live_stages(self.paths), 0)
        self.assertTrue(symlink_lock.exists())

    def test_stage_retention_binds_expected_root_identity(self):
        swap = self._staged_fixture(live.LIVE_STAGE_PREFIX + "swap", 100)
        self._staged_fixture(live.LIVE_STAGE_PREFIX + "retained", 10)
        self._staged_fixture(live.LIVE_STAGE_PREFIX + "newest", 1)
        swapped = False

        original_validate = live._validate_stage_tree

        def replace_after_validate(fd, count):
            nonlocal swapped
            result = original_validate(fd, count)
            if result and not swapped:
                swapped = True
                replacement = swap.with_name("legacy-stage-replacement")
                swap.rename(replacement)
                self._staged_fixture(swap.name, 100)
            return result

        with patch.object(live, "_validate_stage_tree", side_effect=replace_after_validate):
            self.assertEqual(live.retain_live_stages(self.paths), 0)
        self.assertTrue(swapped)
        self.assertTrue(swap.exists())
        self.assertTrue(swap.with_name("legacy-stage-replacement").exists())

    def test_plan_and_refusals_have_no_install_effects(self):
        with self.private_quiescence(), patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40), patch.object(live, "build_stage") as build, patch.object(live, "thegn_ancestor", return_value=None):
            before = set(self.root.rglob("*"))
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(live.main(["--plan", "--repo", str(self.repo)]), 0)
            self.assertEqual(set(self.root.rglob("*")), before)
            build.assert_not_called()
            with patch.object(live.sys.stdin, "isatty", return_value=False), self.assertRaisesRegex(live.Refusal, "terminal"):
                live.main(["--repo", str(self.repo)])
            build.assert_not_called()
            # A new build that rejects the config stops the upgrade before
            # anything running is touched.
            build.return_value = (self.root, self.binary, self.record)
            with contextlib.redirect_stdout(io.StringIO()), patch.object(live.sys.stdin, "isatty", return_value=True), patch.object(live.sys.stdout, "isatty", return_value=True), patch.object(live, "validate_config", side_effect=live.Refusal("rejects the configuration")), patch.object(live, "stop_running") as stop:
                with self.assertRaisesRegex(live.Refusal, "rejects the configuration"):
                    live.main(["--repo", str(self.repo)])
            stop.assert_not_called()
            self.assertEqual(self.target.read_bytes(), b"old executable")

    def test_inside_thegn_relaunches_instead_of_stopping_itself(self):
        with contextlib.redirect_stdout(io.StringIO()), patch.object(live.sys.stdin, "isatty", return_value=True), patch.object(live.sys.stdout, "isatty", return_value=True), patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40), patch.object(live, "thegn_ancestor", return_value=1234), patch.object(live, "relaunch_outside") as relaunch, patch.object(live, "build_stage") as build, patch.object(live, "stop_running") as stop:
            self.assertEqual(live.main(["--repo", str(self.repo), "--level", "info"]), 0)
        relaunch.assert_called_once()
        self.assertEqual(relaunch.call_args[0][0], ["--repo", str(self.repo), "--level", "info"])
        build.assert_not_called()
        stop.assert_not_called()

    def test_a_non_interactive_caller_never_relaunches_an_upgrade(self):
        # The 2026-10-05 incident: an agent's shell (no tty) ran `just live`
        # from inside thegn and it reopened itself in a fresh, unwatched window.
        with contextlib.redirect_stdout(io.StringIO()), patch.object(live.sys.stdin, "isatty", return_value=False), patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40), patch.object(live, "thegn_ancestor", return_value=1234), patch.object(live, "relaunch_outside") as relaunch, patch.object(live, "build_stage") as build:
            with self.assertRaisesRegex(live.Refusal, "terminal"):
                live.main(["--repo", str(self.repo)])
        relaunch.assert_not_called()
        build.assert_not_called()

    def _fake_proc(self, processes, proc=None):
        """processes: {pid: (name, ppid, exe or None, argv[, environ bytes])}"""
        proc = proc or self.root / "fake-proc"
        proc.mkdir(mode=0o700, exist_ok=True)
        for pid, (name, ppid, exe, argv, *environ) in processes.items():
            base = proc / str(pid)
            base.mkdir()
            (base / "status").write_text(f"Name:\t{name}\nState:\tS (sleeping)\nPPid:\t{ppid}\nUid:\t{os.getuid()}\t{os.getuid()}\t{os.getuid()}\t{os.getuid()}\n")
            if exe is not None:
                (base / "exe").symlink_to(exe)
            (base / "cmdline").write_bytes(b"\0".join(argv) + b"\0")
            if environ:
                (base / "environ").write_bytes(environ[0])
            (base / "fd").mkdir()
        return proc

    def _env_of(self, state_home):
        return b"PATH=/bin\0XDG_STATE_HOME=" + os.fsencode(state_home) + b"\0"

    def test_thegn_ancestor_walks_the_parent_chain(self):
        proc = self._fake_proc({40: ("bash", 30, None, [b"bash"]), 30: ("just", 20, None, [b"just"]),
                                20: ("thegn", 1, None, [b"thegn", b"daemon"]), 50: ("bash", 1, None, [b"bash"])})
        self.assertEqual(live.thegn_ancestor(proc, start=40), 20)
        self.assertIsNone(live.thegn_ancestor(proc, start=50))
        self.assertIsNone(live.thegn_ancestor(proc, start=999))

    def test_target_processes_select_only_this_installations_binary(self):
        other = self.root / "other-checkout-thegn"
        other.write_bytes(b"other")
        other.chmod(0o755)
        ours, elsewhere = self._env_of(self.state.parent), self._env_of(self.root / "dev-state")
        proc = self._fake_proc({11: ("thegn", 1, self.target, [str(self.target).encode()], ours),
                                12: ("thegn", 1, self.target, [str(self.target).encode(), b"daemon", b"--socket", b"s"], ours),
                                13: ("thegn", 1, other, [str(other).encode()], elsewhere),
                                14: ("bash", 1, None, [b"bash"], ours)})
        self.assertEqual(sorted(live.target_processes(self.paths, proc)), [(11, False), (12, True)])

    def test_stop_quits_controllers_first_then_the_daemon(self):
        alive = {11: False, 12: True}  # pid -> is_daemon
        events = []

        def scan(*_args):
            return sorted(alive.items())

        def kill(pid, sig):
            events.append(("signal", pid, sig))
            if sig in (live.signal.SIGTERM, live.signal.SIGKILL):
                alive.pop(pid, None)

        def run(argv, **_kwargs):
            events.append(("run", argv[1:]))
            alive.pop(12, None)
            return subprocess.CompletedProcess(argv, 0, b"pane daemon shutting down\n")

        quiet = lambda paths: None if not alive else (_ for _ in ()).throw(live.Refusal("busy"))
        with patch.object(live, "target_processes", side_effect=scan), patch.object(live, "quiescent", side_effect=quiet), contextlib.redirect_stdout(io.StringIO()):
            live.stop_running(self.paths, self.env, sleep=lambda _s: None, run=run, kill=kill)
        self.assertEqual(events, [("signal", 11, live.signal.SIGTERM), ("run", ["daemon", "stop"])])

    def test_stop_escalates_only_on_target_and_names_foreign_blockers(self):
        clock = [0.0]
        signalled = []

        def sleep(seconds):
            clock[0] += seconds

        def kill(pid, sig):
            signalled.append((pid, sig))

        def run(argv, **_kwargs):
            return subprocess.CompletedProcess(argv, 1, b"")

        # The target daemon ignores everything, and the scan keeps refusing:
        # escalation reaches SIGKILL for the target pid only, then the
        # refusal names the blocker instead of widening what gets signalled.
        with patch.object(live, "target_processes", return_value=[(12, True)]), patch.object(live, "quiescent", side_effect=live.Refusal("A controller/daemon is still running (pid 77)")), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(live.Refusal, "pid 77.*staged build is kept"):
                live.stop_running(self.paths, self.env, sleep=sleep, now=lambda: clock[0], run=run, kill=kill)
        self.assertEqual(signalled, [(12, live.signal.SIGTERM), (12, live.signal.SIGKILL)])

    # --- install location (outside the checkout) ---------------------------

    def test_default_install_dir_is_outside_the_repo_and_overridable(self):
        paths = self.settings()
        self.assertEqual(paths["target"], self.home / ".local/lib/thegn/thegn")
        self.assertNotIn(self.repo, paths["target"].parents)
        self.assertEqual(paths["alacritty"], paths["target"].parent / "alacritty.toml")
        other = self.root / "elsewhere"
        other.mkdir()
        (other / "thegn").write_bytes(b"other install")
        (other / "thegn").chmod(0o755)
        self.assertEqual(self.settings(THEGN_LIVE_INSTALL_DIR=str(other))["target"], other / "thegn")
        flag = live.settings(self.repo, dict(self.env, THEGN_LIVE_INSTALL_DIR=str(self.install)), str(other))
        self.assertEqual(flag["target"], other / "thegn")  # --install-dir beats the environment
        for inside in (self.repo / "target/release", self.repo):
            with self.subTest(inside=inside), self.assertRaisesRegex(live.Refusal, "inside the checkout"):
                self.settings(THEGN_LIVE_INSTALL_DIR=str(inside))
        with self.assertRaises(live.Refusal):
            self.settings(THEGN_LIVE_INSTALL_DIR="relative/dir")

    def test_home_comes_from_passwd_not_HOME(self):
        self.home_patch.stop()
        try:
            account = type("Pw", (), {"pw_dir": str(self.home)})()
            with patch.object(live.pwd, "getpwuid", return_value=account) as lookup:
                paths = self.settings(HOME=str(self.root / "profile-home"))
            lookup.assert_called_with(os.getuid())
        finally:
            self.home_patch.start()
        self.assertEqual(paths["install_dir"], self.home / ".local/lib/thegn")

    def test_profile_HOME_without_XDG_STATE_HOME_is_refused(self):
        profile = self.root / "profile-home"
        (profile / ".local/state/thegn").mkdir(parents=True, mode=0o700)
        shutil.copy2(self.db, profile / ".local/state/thegn/thegn.db")
        env = {"HOME": str(profile), "XDG_CONFIG_HOME": self.env["XDG_CONFIG_HOME"]}
        with self.assertRaisesRegex(live.Refusal, "set XDG_STATE_HOME to the account's state home or pass --install-dir") as caught:
            live.settings(self.repo, env)
        self.assertIn(str(profile / ".local/state"), str(caught.exception))
        self.assertIn(str(self.home / ".local/state"), str(caught.exception))
        # An explicit install directory (flag or environment) is the opt-out.
        self.assertEqual(live.settings(self.repo, env, str(self.install))["state"], profile / ".local/state/thegn")
        self.assertEqual(live.settings(self.repo, dict(env, THEGN_LIVE_INSTALL_DIR=str(self.install)))["target"], self.target)

    def test_noexec_install_dir_is_refused(self):
        real_statvfs = os.statvfs
        seen = []

        def noexec(path):
            seen.append(Path(path))
            result = real_statvfs(path)
            return type("V", (), {"f_flag": result.f_flag | os.ST_NOEXEC})()

        with patch.object(live.os, "statvfs", side_effect=noexec), self.assertRaisesRegex(live.Refusal, "noexec"):
            self.settings()
        self.assertEqual(seen, [self.install])
        shutil.rmtree(self.install)  # not yet created: its nearest existing ancestor is checked
        self.legacy.write_bytes(b"legacy executable")
        self.legacy.chmod(0o755)
        seen.clear()
        with patch.object(live.os, "statvfs", side_effect=noexec), self.assertRaisesRegex(live.Refusal, "noexec"):
            self.settings()
        self.assertEqual(seen, [self.home / ".local/lib"])

    def test_install_places_alacritty_config_beside_the_binary(self):
        with patch.object(live, "settings", return_value=self.paths), patch.object(live, "quiescent"):
            live.install(self.paths, self.binary, self.record, self.env)
        self.assertEqual(self.target.read_bytes(), b"new executable")
        installed = self.install / "alacritty.toml"
        self.assertEqual(installed.read_text(), "[font]\nsize = 11\n")
        self.assertEqual(installed.stat().st_mode & 0o777, 0o644)
        self.assertEqual(sorted(p.name for p in self.install.iterdir()), ["alacritty.toml", "thegn"])

    def test_first_run_migrates_from_the_in_repo_binary(self):
        shutil.rmtree(self.install)
        self.legacy.write_bytes(b"legacy executable")
        self.legacy.chmod(0o755)
        paths = self.settings()
        self.assertEqual(paths["previous"], self.legacy)
        self.assertEqual(paths["target"], self.install / "thegn")
        self.assertFalse(self.install.exists())  # settings never creates it
        # Processes of the legacy binary under this state home are this
        # installation's; the same binary elsewhere is not.
        proc = self._fake_proc({21: ("thegn", 1, self.legacy, [str(self.legacy).encode()], self._env_of(self.state.parent)),
                                22: ("thegn", 1, self.legacy, [str(self.legacy).encode()], self._env_of(self.root / "x"))})
        self.assertEqual(live.target_processes(paths, proc), [(21, False)])
        out = io.StringIO()
        with contextlib.redirect_stdout(out), patch.object(live, "settings", return_value=paths), patch.object(live, "source_revision", return_value="a" * 40), patch.object(live, "survey", return_value=[]):
            self.assertEqual(live.main(["--plan", "--repo", str(self.repo)]), 0)
        self.assertIn(f"First run at this location: {self.legacy} is treated as the previous installation", out.getvalue())
        self.assertFalse(self.install.exists())
        self.install.mkdir(parents=True)
        with patch.object(live, "settings", return_value=paths), patch.object(live, "quiescent"):
            recovery = live.install(paths, self.binary, self.record, self.env)
        self.assertEqual((recovery / "thegn.previous").read_bytes(), b"legacy executable")
        self.assertEqual(json.loads((recovery / "complete.json").read_text())["previous"], str(self.legacy))
        self.assertEqual((self.install / "thegn").read_bytes(), b"new executable")
        self.assertEqual(self.legacy.read_bytes(), b"legacy executable")  # never rewritten
        self.assertEqual(live.settings(self.repo, self.env)["previous"], self.install / "thegn")

    def test_state_home_of_follows_xdg_then_home(self):
        self.assertEqual(live.state_home_of(b"HOME=/h\0XDG_STATE_HOME=/s\0"), Path("/s"))
        self.assertEqual(live.state_home_of(b"HOME=/h\0"), Path("/h/.local/state"))
        self.assertEqual(live.state_home_of(b"XDG_STATE_HOME=rel\0HOME=/h\0"), Path("/h/.local/state"))
        self.assertEqual(live.state_home_of(b"XDG_STATE_HOME=/a\0XDG_STATE_HOME=/b\0"), Path("/a"))
        self.assertIsNone(live.state_home_of(b"PATH=/bin\0"))

    # --- process scope: only this installation ------------------------------

    def _other_binary(self, name="thegn"):
        other = self.root / "dev-checkout/target/debug" / name
        other.parent.mkdir(parents=True, exist_ok=True)
        other.write_bytes(b"dev build")
        other.chmod(0o755)
        return other

    def test_another_installation_is_neither_signalled_nor_blocking(self):
        other = self._other_binary()
        dev_state = self.root / "dev-state"
        proc = self._fake_proc({31: ("thegn", 1, other, [str(other).encode()], self._env_of(dev_state)),
                                32: ("thegn", 1, other, [str(other).encode(), b"daemon"], self._env_of(dev_state))})
        self.assertEqual(live.target_processes(self.paths, proc), [])
        live.quiescent(self.paths, proc)  # does not block
        lines = live.report_processes(live.survey(self.paths, proc))
        self.assertIn("2 other thegn process(es) belong to another installation and are left alone", lines)
        self.assertIn(f"  pid 31, {other}, state home {dev_state}", lines)
        signalled = []
        with patch.object(live, "quiescent", side_effect=lambda paths: None), contextlib.redirect_stdout(io.StringIO()):
            live.stop_running(self.paths, self.env, proc=proc, sleep=lambda _s: None,
                              run=lambda *a, **k: self.fail("daemon stop ran"), kill=lambda *a: signalled.append(a))
        self.assertEqual(signalled, [])

    def test_same_binary_other_state_home_is_reported_not_signalled(self):
        isolated = self.root / "isolated-state"
        proc = self._fake_proc({41: ("thegn", 1, self.target, [str(self.target).encode()], self._env_of(isolated)),
                                42: ("thegn", 1, self.target, [str(self.target).encode()], self._env_of(self.state.parent))})
        self.assertEqual(live.target_processes(self.paths, proc), [(42, False)])
        (proc / "42").rename(self.root / "gone-42")
        live.quiescent(self.paths, proc)  # the isolated one does not block
        lines = "\n".join(live.report_processes(live.survey(self.paths, proc)))
        self.assertIn("run this installation's binary under another state home", lines)
        self.assertIn("keep running the old binary until restarted", lines)
        self.assertIn(f"pid 41, {self.target}, state home {isolated}", lines)

    def test_unreadable_environ_is_never_signalled(self):
        proc = self._fake_proc({51: ("thegn", 1, self.target, [str(self.target).encode()])})  # no environ
        self.assertEqual(live.target_processes(self.paths, proc), [])
        with self.assertRaisesRegex(live.Refusal, "cannot be determined is still running"):
            live.quiescent(self.paths, proc)
        clock, signalled, ran = [0.0], [], []

        def sleep(seconds):
            clock[0] += seconds

        real_quiescent = live.quiescent
        with patch.object(live, "quiescent", side_effect=lambda paths: real_quiescent(paths, proc)), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(live.Refusal, "cannot be determined"):
                live.stop_running(self.paths, self.env, proc=proc, sleep=sleep, now=lambda: clock[0],
                                  run=lambda *a, **k: ran.append(a), kill=lambda *a: signalled.append(a))
        self.assertEqual((signalled, ran), ([], []))
        self.assertIn("never signalled", "\n".join(live.report_processes(live.survey(self.paths, proc))))

    def test_state_home_users_that_are_not_thegn_block_but_are_not_signalled(self):
        stranger = self._other_binary("tg")  # e.g. an unrelated `tg` CLI inheriting HOME
        proc = self._fake_proc({61: ("tg", 1, stranger, [b"tg"], self._env_of(self.state.parent))})
        self.assertEqual(live.target_processes(self.paths, proc), [])
        with self.assertRaisesRegex(live.Refusal, "this installation is still running"):
            live.quiescent(self.paths, proc)
        self.assertIn("never signalled and block the upgrade", "\n".join(live.report_processes(live.survey(self.paths, proc))))

    def test_other_installation_holding_our_database_still_blocks(self):
        other = self._other_binary()
        proc = self._fake_proc({71: ("thegn", 1, other, [b"thegn"], self._env_of(self.root / "dev-state"))})
        (proc / "71/fd/3").symlink_to(self.db)
        with self.assertRaisesRegex(live.Refusal, "files open"):
            live.quiescent(self.paths, proc)

    # --- launchers -----------------------------------------------------------

    def _launchers(self):
        bin_dir = self.home / ".local/bin"
        bin_dir.mkdir(parents=True)
        (bin_dir / "thegn").symlink_to(self.legacy)
        old_config = self.repo / "config/alacritty.toml"
        (bin_dir / "tg").write_text(f"#!/bin/sh\nexec alacritty --config-file {old_config} -e {bin_dir}/tg-tui\n"
                                    f"# near miss stays: {self.legacy}-old {old_config}.orig\n")
        (bin_dir / "tg-tui").write_text(f"#!/bin/sh\nexport THEGN_ALACRITTY_CONFIG={old_config}\nexec {self.legacy} \"$@\"\n")
        for name in ("tg", "tg-tui"):
            (bin_dir / name).chmod(0o755)
        return bin_dir

    def _tree(self, directory):
        return {p.name: (os.readlink(p) if p.is_symlink() else p.read_bytes(), p.lstat().st_mode)
                for p in sorted(directory.iterdir())}

    def test_relink_launchers_repoints_symlink_and_rewrites_exact_paths(self):
        bin_dir = self._launchers()
        before = {name: (bin_dir / name).read_text() for name in ("tg", "tg-tui")}
        state = "\n".join(live.launcher_state(self.paths, self.home))
        self.assertIn("does NOT resolve to the install target", state)
        self.assertIn(f"{bin_dir / 'tg'} mentions the old repo path(s): {self.repo / 'config/alacritty.toml'}", state)
        actions = live.relink_plan(self.paths, self.home)
        with contextlib.redirect_stdout(io.StringIO()):
            live.apply_relink(self.paths, actions)
        self.assertEqual(os.readlink(bin_dir / "thegn"), str(self.target))
        new_config = self.install / "alacritty.toml"
        tg = (bin_dir / "tg").read_text()
        self.assertIn(f"--config-file {new_config} -e", tg)
        self.assertIn(f"{self.legacy}-old {self.repo / 'config/alacritty.toml'}.orig", tg)  # not exact: untouched
        self.assertEqual((bin_dir / "tg-tui").read_text(),
                         f"#!/bin/sh\nexport THEGN_ALACRITTY_CONFIG={new_config}\nexec {self.target} \"$@\"\n")
        for name in ("tg", "tg-tui"):
            self.assertEqual((bin_dir / (name + ".bak")).read_text(), before[name])
            self.assertEqual((bin_dir / name).stat().st_mode & 0o777, 0o755)
        self.assertIn("resolves to the install target", "\n".join(live.launcher_state(self.paths, self.home)))
        self.assertEqual(live.relink_plan(self.paths, self.home), [])  # idempotent

    def test_relink_refuses_a_wrapper_with_neither_path(self):
        bin_dir = self._launchers()
        (bin_dir / "tg").write_text("#!/bin/sh\nexec /opt/thegn/bin/thegn \"$@\"\n")
        before = self._tree(bin_dir)
        with self.assertRaisesRegex(live.Refusal, "mentions neither"):
            live.relink_plan(self.paths, self.home)
        (bin_dir / "tg").unlink()
        (bin_dir / "tg").symlink_to("/nix/store/wrapper")
        with self.assertRaisesRegex(live.Refusal, "not a current-user regular file"):
            live.relink_plan(self.paths, self.home)
        (bin_dir / "tg").unlink()
        (bin_dir / "tg").write_bytes(b"\xff\xfe binary")
        with self.assertRaisesRegex(live.Refusal, "not a UTF-8 text file"):
            live.relink_plan(self.paths, self.home)
        self.assertEqual(os.readlink(bin_dir / "thegn"), str(self.legacy))
        self.assertFalse(any(name.endswith(".bak") for name in self._tree(bin_dir)))
        self.assertEqual(before["tg-tui"], self._tree(bin_dir)["tg-tui"])

    def _follow_target(self, bin_dir):
        (bin_dir / "thegn").unlink()
        (bin_dir / "thegn").symlink_to(self.target)
        (bin_dir / "tg").write_text(f"#!/bin/sh\nexec alacritty --config-file {self.install / 'alacritty.toml'} -e {bin_dir}/tg-tui\n")
        (bin_dir / "tg-tui").write_text(f"#!/bin/sh\nexec {self.target} \"$@\"\n")

    def _main_mocks(self, recovery):
        class Child:
            def wait(self):
                return 0

        return (patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40),
                patch.object(live, "survey", return_value=[]), patch.object(live, "build_stage", return_value=(self.root, self.binary, self.record)),
                patch.object(live, "install", return_value=recovery), patch.object(live.sys.stdin, "isatty", return_value=True),
                patch.object(live.sys.stdout, "isatty", return_value=True), patch.object(live, "thegn_ancestor", return_value=None),
                patch.object(live, "validate_config"), patch.object(live, "stop_running"), patch.object(live, "doctor_later"),
                patch.object(live, "countdown"), patch.object(live.subprocess, "Popen", return_value=Child()))

    def test_launchers_on_the_checkout_refuse_a_run_without_relink(self):
        bin_dir = self._launchers()
        before = self._tree(bin_dir)
        recovery = self.state / "private-recovery"
        recovery.mkdir(mode=0o700)
        stale = f"launchers still run {self.legacy}; rerun with `python3 -B scripts/live.py --relink-launchers`"
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.ExitStack() as stack:
            mocks = [stack.enter_context(item) for item in self._main_mocks(recovery)]
            build, stop = mocks[3], mocks[9]
            self.assertEqual(live.main(["--plan", "--repo", str(self.repo)]), 0)
            self.assertIn(f"WARNING: {stale}", out.getvalue())
            with self.assertRaisesRegex(live.Refusal, re.escape(stale)):
                live.main(["--repo", str(self.repo)])
            # A wrapper alone (symlink already relinked) still refuses.
            (bin_dir / "thegn").unlink()
            (bin_dir / "thegn").symlink_to(self.target)
            with self.assertRaisesRegex(live.Refusal, "launchers still run"):
                live.main(["--repo", str(self.repo)])
        build.assert_not_called()
        stop.assert_not_called()
        (bin_dir / "thegn").unlink()
        (bin_dir / "thegn").symlink_to(self.legacy)
        self.assertEqual(self._tree(bin_dir), before)

    def test_run_without_relink_never_touches_launchers(self):
        bin_dir = self._launchers()
        self._follow_target(bin_dir)
        before = self._tree(bin_dir)
        recovery = self.state / "private-recovery"
        recovery.mkdir(mode=0o700)
        (recovery / "thegn-stderr.log").touch(mode=0o600)

        class Child:
            def wait(self):
                return 0

        common = lambda: (patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40),
                  patch.object(live, "survey", return_value=[]), patch.object(live, "build_stage", return_value=(self.root, self.binary, self.record)),
                  patch.object(live, "install", return_value=recovery), patch.object(live.sys.stdin, "isatty", return_value=True),
                  patch.object(live.sys.stdout, "isatty", return_value=True), patch.object(live, "thegn_ancestor", return_value=None),
                  patch.object(live, "validate_config"), patch.object(live, "stop_running"), patch.object(live, "doctor_later"),
                  patch.object(live, "countdown"), patch.object(live.subprocess, "Popen", return_value=Child()))
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.ExitStack() as stack:
            for item in common():
                stack.enter_context(item)
            self.assertEqual(live.main(["--plan", "--relink-launchers", "--repo", str(self.repo)]), 0)
            # Launchers that already follow the install target pass through.
            self.assertEqual(live.main(["--repo", str(self.repo)]), 0)
        self.assertEqual(self._tree(bin_dir), before)
        self.assertIn("Launchers are not changed without --relink-launchers.", out.getvalue())
        self.assertIn("--relink-launchers: launchers already follow the install target", out.getvalue())
        # With the flag, a run relinks stale launchers after the install.
        shutil.rmtree(bin_dir)
        bin_dir = self._launchers()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.ExitStack() as stack:
            for item in common():
                stack.enter_context(item)
            self.assertEqual(live.main(["--repo", str(self.repo), "--relink-launchers"]), 0)
        self.assertEqual(os.readlink(bin_dir / "thegn"), str(self.target))
        self.assertTrue((bin_dir / "tg.bak").exists())

    def test_relink_refusal_happens_before_anything_is_stopped(self):
        bin_dir = self._launchers()
        (bin_dir / "tg-tui").write_text("#!/bin/sh\nexec something-else\n")
        with contextlib.redirect_stdout(io.StringIO()), patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40), patch.object(live, "survey", return_value=[]), patch.object(live.sys.stdin, "isatty", return_value=True), patch.object(live.sys.stdout, "isatty", return_value=True), patch.object(live, "thegn_ancestor", return_value=None), patch.object(live, "build_stage") as build, patch.object(live, "stop_running") as stop:
            with self.assertRaisesRegex(live.Refusal, "mentions neither"):
                live.main(["--repo", str(self.repo), "--relink-launchers"])
        build.assert_not_called()
        stop.assert_not_called()

    def test_validate_config_refuses_on_nonzero_exit(self):
        def run(argv, **_kwargs):
            self.assertEqual(argv[1:], ["config", "validate"])
            return subprocess.CompletedProcess(argv, 2, b"thegn: config: noise\nconfig.toml: unknown key `bogus`\n")

        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaisesRegex(live.Refusal, "nothing was stopped"):
            live.validate_config(self.binary, self.env, run=run)
        self.assertIn("unknown key", out.getvalue())
        self.assertNotIn("noise", out.getvalue())

    def test_supervisor_retains_launcher_but_releases_schema_lock(self):
        recovery = self.state / "private-recovery"
        recovery.mkdir(mode=0o700)
        (recovery / "thegn-stderr.log").touch(mode=0o600)

        def spawn(argv, **kwargs):
            self.assertEqual(argv, [str(self.target)])
            self.assertEqual(kwargs["env"]["THEGN_NO_MIGRATE"], "1")
            self.assertEqual(kwargs["env"].get("THEGN_DATABASE_MIGRATION_EXECUTABLE"), expected_pin)
            self.assertEqual(kwargs["env"]["THEGN_LOG_ROTATION_SIZE_MB"], "20")
            self.assertEqual(kwargs["env"]["THEGN_LOG"], expected_log)

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

        with contextlib.redirect_stdout(io.StringIO()), self.private_quiescence(), patch.object(live, "settings", return_value=self.paths), patch.object(live, "source_revision", return_value="a" * 40), patch.object(live, "build_stage", return_value=(self.root, self.binary, self.record)), patch.object(live, "install", side_effect=install), patch.object(live.sys.stdin, "isatty", return_value=True), patch.object(live.sys.stdout, "isatty", return_value=True), patch.object(live, "thegn_ancestor", return_value=None), patch.object(live, "validate_config"), patch.object(live, "stop_running"), patch.object(live, "doctor_later"), patch.object(live, "countdown"), patch.object(live.subprocess, "Popen", side_effect=spawn):
            for expected_pin in (None, str(self.target)):
                for argv, expected_log in (
                    (["--repo", str(self.repo)], "info,log=error,tokei=error"),
                    (["--repo", str(self.repo), "--level", "debug"], "debug,log=error,tokei=error"),
                ):
                    # Keep HOME unchanged; test only captured migration override.
                    env = dict(os.environ, **self.env)
                    if expected_pin is None:
                        env.pop("THEGN_DATABASE_MIGRATION_EXECUTABLE", None)
                    else:
                        env["THEGN_DATABASE_MIGRATION_EXECUTABLE"] = expected_pin
                    with patch.dict(live.os.environ, env, clear=True):
                        self.assertEqual(live.main(argv), 7)

    def test_default_log_level_is_info_and_debug_is_opt_in(self):
        source = Path(live.__file__).read_text()
        level_line = next(line for line in source.splitlines() if 'parser.add_argument("--level"' in line)
        self.assertIn('default="info"', level_line)

        justfile = (ROOT / "justfile").read_text()
        self.assertIn('live level="info" ', justfile)
        self.assertIn('live-plan level="info" ', justfile)
        self.assertNotIn('live level="debug" ', justfile)
        self.assertNotIn('live-plan level="debug" ', justfile)

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
