#!/usr/bin/env python3
"""Race and refusal checks for the macOS bundle filesystem helper."""

import argparse
import contextlib
import errno
import importlib.util
import io
import os
import tempfile
import unittest


ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HELPER_PATH = os.path.join(ROOT, "packaging", "macos", "app_bundle_fs.py")
SPEC = importlib.util.spec_from_file_location("app_bundle_fs", HELPER_PATH)
HELPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPER)


def make_bundle(parent, name, identifier, marker=True):
    resources = os.path.join(parent, name, "Contents", "Resources")
    os.makedirs(resources)
    with open(os.path.join(parent, name, "Contents", "Info.plist"), "wb") as handle:
        handle.write(("<string>%s</string>\n" % identifier).encode())
    with open(os.path.join(resources, "common.sh"), "wb") as handle:
        handle.write(b"owned common launcher\n")
    if marker:
        with open(os.path.join(resources, ".thegn-make-app"), "wb") as handle:
            handle.write(HELPER.MARKER)


def read_bytes(path):
    with open(path, "rb") as handle:
        return handle.read()


class BundleFilesystemTests(unittest.TestCase):
    def test_displaced_inode_is_restored_if_app_changes_before_exchange(self):
        with tempfile.TemporaryDirectory() as parent:
            bundle_id = "com.github.blakeashleyjr.thegn"
            legacy_id = "com.github.blakeashleyjr.thegn"
            make_bundle(parent, "thegn.app", bundle_id)
            make_bundle(parent, "stage.app", bundle_id)
            make_bundle(parent, "foreign.app", "com.example.foreign", marker=False)
            with open(os.path.join(parent, "thegn.app", "Contents", "Resources", "keep"), "wb") as handle:
                handle.write(b"original")
            with open(os.path.join(parent, "foreign.app", "Contents", "Resources", "keep"), "wb") as handle:
                handle.write(b"foreign")

            original_rename = HELPER.rename_with_flags
            injected = [False]

            def race(parent_fd, source, destination, flags):
                if flags == 2 and not injected[0]:
                    os.rename(destination, "original.app", src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
                    os.rename("foreign.app", destination, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
                    injected[0] = True
                return original_rename(parent_fd, source, destination, flags)

            HELPER.rename_with_flags = race
            old_cwd = os.getcwd()
            try:
                os.chdir(parent)
                with self.assertRaises(OSError) as caught:
                    HELPER.promote(argparse.Namespace(stage="stage.app", app="thegn.app", bundle_id=bundle_id, legacy_bundle_id=legacy_id))
            finally:
                os.chdir(old_cwd)
                HELPER.rename_with_flags = original_rename

            self.assertEqual(caught.exception.errno, errno.EAGAIN)
            self.assertEqual(read_bytes(os.path.join(parent, "thegn.app", "Contents", "Resources", "keep")), b"foreign")
            self.assertEqual(read_bytes(os.path.join(parent, "original.app", "Contents", "Resources", "keep")), b"original")
            self.assertTrue(os.path.isfile(os.path.join(parent, "stage.app", "Contents", "Resources", ".thegn-make-app")))

    def test_unavailable_atomic_primitive_preserves_existing_app(self):
        with tempfile.TemporaryDirectory() as parent:
            bundle_id = "com.github.blakeashleyjr.thegn"
            make_bundle(parent, "thegn.app", bundle_id)
            make_bundle(parent, "stage.app", bundle_id)
            original = read_bytes(os.path.join(parent, "thegn.app", "Contents", "Info.plist"))
            original_rename = HELPER.rename_with_flags

            def unavailable(*_args):
                raise OSError(errno.ENOTSUP, "atomic exchange unavailable")

            HELPER.rename_with_flags = unavailable
            old_cwd = os.getcwd()
            try:
                os.chdir(parent)
                with self.assertRaises(OSError):
                    HELPER.promote(argparse.Namespace(stage="stage.app", app="thegn.app", bundle_id=bundle_id, legacy_bundle_id=bundle_id))
            finally:
                os.chdir(old_cwd)
                HELPER.rename_with_flags = original_rename
            self.assertEqual(read_bytes(os.path.join(parent, "thegn.app", "Contents", "Info.plist")), original)

    def test_first_install_never_replaces_a_racing_foreign_path(self):
        with tempfile.TemporaryDirectory() as parent:
            bundle_id = "com.github.blakeashleyjr.thegn"
            make_bundle(parent, "stage.app", bundle_id)
            original_rename = HELPER.rename_with_flags

            def race_create(parent_fd, source, destination, flags):
                if flags == 1:
                    os.mkdir(destination, dir_fd=parent_fd)
                    foreign_fd = os.open(destination, os.O_RDONLY | HELPER.O_DIRECTORY, dir_fd=parent_fd)
                    try:
                        fd = os.open("sentinel", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=foreign_fd)
                        try:
                            os.write(fd, b"foreign")
                        finally:
                            os.close(fd)
                    finally:
                        os.close(foreign_fd)
                return original_rename(parent_fd, source, destination, flags)

            HELPER.rename_with_flags = race_create
            old_cwd = os.getcwd()
            try:
                os.chdir(parent)
                with self.assertRaises(OSError):
                    HELPER.promote(argparse.Namespace(stage="stage.app", app="thegn.app", bundle_id=bundle_id, legacy_bundle_id=bundle_id))
            finally:
                os.chdir(old_cwd)
                HELPER.rename_with_flags = original_rename
            self.assertEqual(read_bytes(os.path.join(parent, "thegn.app", "sentinel")), b"foreign")
            self.assertTrue(os.path.isfile(os.path.join(parent, "stage.app", "Contents", "Resources", ".thegn-make-app")))

    def test_partial_old_bundle_cleanup_keeps_new_bundle_active(self):
        with tempfile.TemporaryDirectory() as parent:
            bundle_id = "com.github.blakeashleyjr.thegn"
            make_bundle(parent, "thegn.app", bundle_id)
            make_bundle(parent, "stage.app", bundle_id)
            with open(os.path.join(parent, "thegn.app", "Contents", "Resources", "generation"), "wb") as handle:
                handle.write(b"old")
            with open(os.path.join(parent, "stage.app", "Contents", "Resources", "generation"), "wb") as handle:
                handle.write(b"new")
            original_remove = HELPER.remove_tree_contents

            def partial_remove(directory_fd):
                contents_fd = os.open("Contents", os.O_RDONLY | HELPER.O_DIRECTORY, dir_fd=directory_fd)
                try:
                    os.unlink("Info.plist", dir_fd=contents_fd)
                finally:
                    os.close(contents_fd)
                raise OSError(errno.EIO, "injected failure after partial retirement")

            HELPER.remove_tree_contents = partial_remove
            old_cwd = os.getcwd()
            error_text = io.StringIO()
            try:
                os.chdir(parent)
                with contextlib.redirect_stderr(error_text), self.assertRaises(SystemExit) as caught:
                    HELPER.promote(argparse.Namespace(stage="stage.app", app="thegn.app", bundle_id=bundle_id, legacy_bundle_id=bundle_id))
            finally:
                os.chdir(old_cwd)
                HELPER.remove_tree_contents = original_remove

            self.assertEqual(caught.exception.code, 75)
            self.assertIn("cleanup incomplete", error_text.getvalue())
            self.assertIn("stage.app", error_text.getvalue())
            self.assertEqual(read_bytes(os.path.join(parent, "thegn.app", "Contents", "Resources", "generation")), b"new")
            self.assertTrue(os.path.isdir(os.path.join(parent, "stage.app")))
            self.assertFalse(os.path.exists(os.path.join(parent, "stage.app", "Contents", "Info.plist")))

    def test_lock_release_does_not_follow_replaced_lock_symlink(self):
        with tempfile.TemporaryDirectory() as parent:
            lock_path = os.path.join(parent, ".thegn.app.lock")
            moved_path = os.path.join(parent, "moved-lock")
            foreign_path = os.path.join(parent, "foreign-lock")
            os.mkdir(lock_path)
            with open(os.path.join(lock_path, "pid"), "wb") as handle:
                handle.write(b"12345\n")
            os.mkdir(foreign_path)
            with open(os.path.join(foreign_path, "pid"), "wb") as handle:
                handle.write(b"12345\n")
            with open(os.path.join(foreign_path, "sentinel"), "wb") as handle:
                handle.write(b"foreign")
            identity = os.stat(lock_path, follow_symlinks=False)
            os.rename(lock_path, moved_path)
            os.symlink(foreign_path, lock_path)
            old_cwd = os.getcwd()
            try:
                os.chdir(parent)
                with self.assertRaises(OSError):
                    HELPER.release_lock(argparse.Namespace(path=".thegn.app.lock", pid="12345", identity=f"{identity.st_dev}:{identity.st_ino}"))
            finally:
                os.chdir(old_cwd)
            self.assertEqual(read_bytes(os.path.join(foreign_path, "pid")), b"12345\n")
            self.assertEqual(read_bytes(os.path.join(foreign_path, "sentinel")), b"foreign")
            self.assertEqual(read_bytes(os.path.join(moved_path, "pid")), b"12345\n")


if __name__ == "__main__":
    unittest.main()
