#!/usr/bin/env python3
"""Pinned-directory and atomic-promotion helpers for make-app.sh (Python 3.9+)."""

from __future__ import annotations

import argparse
import ctypes
import errno
import os
import stat
import sys

MARKER = b"thegn make-app 1\n"
O_DIRECTORY = getattr(os, "O_DIRECTORY", 0)
O_NOFOLLOW = getattr(os, "O_NOFOLLOW", 0)
O_NONBLOCK = getattr(os, "O_NONBLOCK", 0)


def open_pinned_directory(path: str) -> int:
    if not O_DIRECTORY or not O_NOFOLLOW:
        raise OSError(errno.ENOTSUP, "no-follow directory opens are unavailable")
    requested = os.path.abspath(path)
    try:
        leaf = os.lstat(requested)
    except FileNotFoundError:
        leaf = None
    if leaf is not None and not stat.S_ISDIR(leaf.st_mode):
        raise OSError(errno.ELOOP, "destination is not a real directory", requested)
    canonical_parent = os.path.realpath(os.path.dirname(requested))
    canonical = os.path.join(canonical_parent, os.path.basename(requested))
    if os.path.realpath(requested) != canonical:
        raise OSError(errno.ELOOP, "destination leaf is a symlink", requested)
    fd = os.open("/", os.O_RDONLY | O_DIRECTORY)
    try:
        for component in canonical.split("/"):
            if not component:
                continue
            try:
                before = os.stat(component, dir_fd=fd, follow_symlinks=False)
            except FileNotFoundError:
                os.mkdir(component, 0o755, dir_fd=fd)
                before = os.stat(component, dir_fd=fd, follow_symlinks=False)
            if not stat.S_ISDIR(before.st_mode):
                raise OSError(errno.ELOOP, "destination path contains a symlink or non-directory", component)
            child = os.open(component, os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW, dir_fd=fd)
            after = os.fstat(child)
            if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino):
                os.close(child)
                raise OSError(errno.EAGAIN, "destination directory changed while being pinned", component)
            os.close(fd)
            fd = child
        if leaf is not None:
            final = os.fstat(fd)
            if (leaf.st_dev, leaf.st_ino) != (final.st_dev, final.st_ino):
                raise OSError(errno.EAGAIN, "destination directory changed while being pinned", requested)
        return fd
    except BaseException:
        os.close(fd)
        raise


def pinned_exec(args: argparse.Namespace) -> None:
    fd = open_pinned_directory(args.dest)
    os.fchdir(fd)
    os.set_inheritable(fd, True)
    os.environ["THEGN_MAKE_APP_PINNED"] = str(fd)
    command = args.command
    os.execvpe(command[0], command, os.environ)


def open_child_dir(parent_fd: int, name: str) -> int:
    info = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    if not stat.S_ISDIR(info.st_mode):
        raise OSError(errno.ELOOP, "bundle path is not a real directory", name)
    return os.open(name, os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW, dir_fd=parent_fd)


def read_regular_at(parent_fd: int, name: str, limit: int = 1_000_000) -> bytes:
    fd = os.open(name, os.O_RDONLY | O_NOFOLLOW | O_NONBLOCK, dir_fd=parent_fd)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_size > limit:
            raise OSError(errno.EINVAL, "invalid bundle ownership metadata", name)
        chunks = []
        remaining = limit + 1
        while remaining:
            chunk = os.read(fd, min(65536, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        data = b"".join(chunks)
        if len(data) > limit:
            raise OSError(errno.EFBIG, "bundle metadata is too large", name)
        return data
    finally:
        os.close(fd)


def owned_bundle(parent_fd: int, name: str, bundle_id: str, legacy_bundle_id: str):
    try:
        bundle_fd = open_child_dir(parent_fd, name)
    except FileNotFoundError:
        return False, None
    try:
        identity = os.fstat(bundle_fd)
        contents_fd = open_child_dir(bundle_fd, "Contents")
        try:
            resources_fd = open_child_dir(contents_fd, "Resources")
            try:
                try:
                    valid = read_regular_at(resources_fd, ".thegn-make-app") == MARKER
                except FileNotFoundError:
                    common = read_regular_at(resources_fd, "common.sh")
                    plist = read_regular_at(contents_fd, "Info.plist")
                    expected = tuple(f"<string>{value}</string>".encode() for value in (bundle_id, legacy_bundle_id))
                    valid = bool(common) and any(value in plist for value in expected)
                return valid, identity
            finally:
                os.close(resources_fd)
        finally:
            os.close(contents_fd)
    finally:
        os.close(bundle_fd)


def rename_with_flags(parent_fd: int, source: str, destination: str, flags: int) -> None:
    libc = ctypes.CDLL(None, use_errno=True)
    if sys.platform == "darwin":
        function = getattr(libc, "renameatx_np", None)
        native_flags = {"exchange": 0x00000002, "exclusive": 0x00000004}
    else:
        function = getattr(libc, "renameat2", None)
        native_flags = {"exchange": 0x00000002, "exclusive": 0x00000001}
    if function is None:
        raise OSError(errno.ENOTSUP, "atomic bundle rename is unavailable")
    function.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    function.restype = ctypes.c_int
    mode = "exchange" if flags == 2 else "exclusive"
    result = function(parent_fd, os.fsencode(source), parent_fd, os.fsencode(destination), native_flags[mode])
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), destination)


def remove_owned(parent_fd: int, name: str, bundle_id: str, legacy_bundle_id: str, expected_identity=None) -> None:
    bundle_fd = open_child_dir(parent_fd, name)
    try:
        actual = os.fstat(bundle_fd)
        if expected_identity is not None and (actual.st_dev, actual.st_ino) != (expected_identity.st_dev, expected_identity.st_ino):
            raise OSError(errno.EAGAIN, "bundle changed before safe cleanup", name)
        valid, identity = owned_bundle(parent_fd, name, bundle_id, legacy_bundle_id)
        if not valid or (identity.st_dev, identity.st_ino) != (actual.st_dev, actual.st_ino):
            raise OSError(errno.EPERM, "refusing to remove an unowned app bundle", name)
    finally:
        os.close(bundle_fd)
    remove_tree_at(parent_fd, name, actual)


def remove_tree_at(parent_fd: int, name: str, expected_identity) -> None:
    root_fd = open_child_dir(parent_fd, name)
    try:
        opened = os.fstat(root_fd)
        if (opened.st_dev, opened.st_ino) != (expected_identity.st_dev, expected_identity.st_ino):
            raise OSError(errno.EAGAIN, "bundle changed before recursive cleanup", name)
        remove_tree_contents(root_fd)
    finally:
        os.close(root_fd)
    current = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    if (current.st_dev, current.st_ino) != (expected_identity.st_dev, expected_identity.st_ino):
        raise OSError(errno.EAGAIN, "bundle path changed during recursive cleanup", name)
    os.rmdir(name, dir_fd=parent_fd)


def remove_tree_contents(directory_fd: int) -> None:
    for name in os.listdir(directory_fd):
        before = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if stat.S_ISDIR(before.st_mode):
            child_fd = os.open(name, os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW, dir_fd=directory_fd)
            try:
                opened = os.fstat(child_fd)
                if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
                    raise OSError(errno.EAGAIN, "bundle entry changed during cleanup", name)
                remove_tree_contents(child_fd)
            finally:
                os.close(child_fd)
            current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != (before.st_dev, before.st_ino):
                raise OSError(errno.EAGAIN, "bundle entry changed during cleanup", name)
            os.rmdir(name, dir_fd=directory_fd)
        else:
            current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != (before.st_dev, before.st_ino):
                raise OSError(errno.EAGAIN, "bundle entry changed during cleanup", name)
            os.unlink(name, dir_fd=directory_fd)


def promote(args: argparse.Namespace) -> None:
    parent_fd = os.open(".", os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW)
    try:
        stage_info = os.stat(args.stage, dir_fd=parent_fd, follow_symlinks=False)
        stage_owned, stage_identity = owned_bundle(parent_fd, args.stage, args.bundle_id, args.legacy_bundle_id)
        if not stat.S_ISDIR(stage_info.st_mode) or not stage_owned or (stage_info.st_dev, stage_info.st_ino) != (stage_identity.st_dev, stage_identity.st_ino):
            raise OSError(errno.EPERM, "staged bundle ownership validation failed", args.stage)
        try:
            app_info = os.stat(args.app, dir_fd=parent_fd, follow_symlinks=False)
        except FileNotFoundError:
            app_info = None
        if app_info is None:
            rename_with_flags(parent_fd, args.stage, args.app, 1)
            return
        app_owned, app_identity = owned_bundle(parent_fd, args.app, args.bundle_id, args.legacy_bundle_id)
        if not stat.S_ISDIR(app_info.st_mode) or not app_owned or (app_info.st_dev, app_info.st_ino) != (app_identity.st_dev, app_identity.st_ino):
            raise OSError(errno.EPERM, "existing app bundle is not installer-owned", args.app)
        rename_with_flags(parent_fd, args.stage, args.app, 2)
        displaced = os.stat(args.stage, dir_fd=parent_fd, follow_symlinks=False)
        installed = os.stat(args.app, dir_fd=parent_fd, follow_symlinks=False)
        if (displaced.st_dev, displaced.st_ino) != (app_info.st_dev, app_info.st_ino):
            if (installed.st_dev, installed.st_ino) == (stage_info.st_dev, stage_info.st_ino):
                rename_with_flags(parent_fd, args.stage, args.app, 2)
                raise OSError(errno.EAGAIN, "existing app changed before atomic exchange; original app path restored", args.app)
            print(f"make-app: existing app changed during exchange; displaced bundle preserved at {args.stage}", file=sys.stderr)
            raise SystemExit(75)
        if (installed.st_dev, installed.st_ino) != (stage_info.st_dev, stage_info.st_ino):
            print(f"make-app: installed bundle path changed during exchange; displaced bundle preserved at {args.stage}", file=sys.stderr)
            raise SystemExit(75)
        try:
            remove_owned(parent_fd, args.stage, args.bundle_id, args.legacy_bundle_id, app_info)
        except OSError:
            try:
                rename_with_flags(parent_fd, args.stage, args.app, 2)
            except OSError as rollback_error:
                print(f"make-app: atomic promotion succeeded but rollback and cleanup failed; old bundle remains at {args.stage}: {rollback_error}", file=sys.stderr)
                raise SystemExit(75)
            raise
    finally:
        os.close(parent_fd)


def remove(args: argparse.Namespace) -> None:
    parent_fd = os.open(".", os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW)
    try:
        remove_owned(parent_fd, args.path, args.bundle_id, args.legacy_bundle_id)
    finally:
        os.close(parent_fd)


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="operation", required=True)
    pin_parser = sub.add_parser("pin")
    pin_parser.add_argument("--dest", required=True)
    pin_parser.add_argument("command", nargs=argparse.REMAINDER)
    promote_parser = sub.add_parser("promote")
    promote_parser.add_argument("--stage", required=True)
    promote_parser.add_argument("--app", required=True)
    promote_parser.add_argument("--bundle-id", required=True)
    promote_parser.add_argument("--legacy-bundle-id", required=True)
    remove_parser = sub.add_parser("remove")
    remove_parser.add_argument("--path", required=True)
    remove_parser.add_argument("--bundle-id", required=True)
    remove_parser.add_argument("--legacy-bundle-id", required=True)
    args = parser.parse_args()
    try:
        if args.operation == "pin":
            command = args.command[1:] if args.command and args.command[0] == "--" else args.command
            if not command:
                raise OSError(errno.EINVAL, "missing pinned command")
            args.command = command
            pinned_exec(args)
        elif args.operation == "promote":
            promote(args)
        else:
            remove(args)
    except OSError as exc:
        print(f"make-app filesystem helper: {exc}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
