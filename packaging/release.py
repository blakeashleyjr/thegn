#!/usr/bin/env python3
"""Validate release assets and render deterministic package-manager metadata.

This module deliberately uses only the Python standard library. It performs no
network access, invokes no package manager or project binary, and reads no
credentials.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import re
import shutil
import stat
import sys
import tarfile
import tempfile
import tomllib
import zipfile
import zlib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterable, Mapping, Sequence


PACKAGING_DIR = Path(__file__).resolve().parent
REPOSITORY_ROOT = PACKAGING_DIR.parent
MANIFEST_PATH = PACKAGING_DIR / "release.json"
SHA256_RE = re.compile(r"[0-9a-f]{64}")
TRUSTED_NAME_RE = re.compile(r"thegn[A-Za-z0-9._+-]*")
TARGET_RE = re.compile(r"[a-z0-9_]+(?:-[a-z0-9_]+){2,}")
PLACEHOLDER_RE = re.compile(r"@@([A-Z][A-Z0-9_]*)@@")
RELEASE_TAG_RE = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.]+)?")
FORBIDDEN_OUTPUT = ("REPLACE_WITH_", "thegn-dev", ".SRCINFO")
WINDOWS_ERROR = "Windows release lane is not enabled"
MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_TRUSTED_DIGESTS_BYTES = 64 * 1024
# Bound all assets selected by one release download. The current three enabled
# target archives total about 150 MiB in v0.1.0-alpha.103; twice the existing
# per-archive ceiling leaves headroom for package growth while bounding a full
# draft download to 1 GiB. The owner still needs to confirm this operational cap.
MAX_SELECTED_DOWNLOAD_BYTES = 2 * MAX_ARCHIVE_BYTES
TEXT_ASSET_SUFFIXES = (".sha256", "-release.json", "-manifest.json")
RELEASE_MANIFEST_NAME = "thegn-{tag}-manifest.json"
MAX_MEMBERS = 64
MAX_MEMBER_BYTES = 256 * 1024 * 1024
MAX_EXPANDED_BYTES = 512 * 1024 * 1024
MAX_PATH_CHARS = 255
# Expanded bytes may be at most this multiple of the archive's on-disk size.
# Release archives compress about 3-4x; gzip's ceiling is about 1032x, so 100
# refuses decompression bombs with wide headroom.
MAX_COMPRESSION_RATIO = 100
POSIX_PAX_HEADERS = frozenset(
    {
        "path",
        "linkpath",
        "size",
        "uid",
        "gid",
        "uname",
        "gname",
        "mtime",
        "atime",
        "ctime",
        "charset",
        "comment",
        "hdrcharset",
    }
)
DRAFT_RELEASE_ERROR = "release is already published; refusing to upload or replace assets"
MANAGER_PLACEHOLDERS = {
    "homebrew": {
        "VERSION",
        "DESCRIPTION",
        "HOMEPAGE",
        "ARCHIVE_URL",
        "SHA256",
        "OPTIONAL_DEPENDENCIES",
    },
    "aur": {
        "PKGVER",
        "DESCRIPTION",
        "HOMEPAGE",
        "ARCHIVE_URL",
        "SHA256",
        "OPTIONAL_DEPENDENCIES",
    },
    "nfpm": {
        "VERSION",
        "DESCRIPTION",
        "HOMEPAGE",
        "ARCHITECTURE",
        "DEPENDENCIES",
    },
    "scoop": {"VERSION", "DESCRIPTION", "HOMEPAGE", "ARCHIVE_URL", "SHA256"},
    "winget": {"VERSION", "DESCRIPTION", "HOMEPAGE", "ARCHIVE_URL", "SHA256_UPPER"},
}


class ReleaseError(ValueError):
    """A deterministic, user-actionable release input error."""


@dataclass(frozen=True)
class Asset:
    target: str
    archive: Path
    checksum_file: Path
    sha256: str
    url: str


def _object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ReleaseError(f"{label} must be a JSON object")
    return value


def _string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise ReleaseError(f"{label} must be a non-empty string")
    return value


def _safe_relative(value: Any, label: str) -> Path:
    text = _string(value, label)
    path = PurePosixPath(text)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        raise ReleaseError(f"{label} must be a safe relative path: {text!r}")
    return Path(*path.parts)


def load_manifest(path: Path = MANIFEST_PATH) -> dict[str, Any]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ReleaseError(f"cannot read release manifest {path}: {error}") from error
    manifest = _object(manifest, "release manifest")
    _validate_manifest(manifest, path.parent)
    return manifest


def workspace_version(root: Path = REPOSITORY_ROOT) -> str:
    cargo_path = root / "Cargo.toml"
    try:
        cargo = tomllib.loads(cargo_path.read_text(encoding="utf-8"))
        version = cargo["workspace"]["package"]["version"]
    except (OSError, tomllib.TOMLDecodeError, KeyError, TypeError) as error:
        raise ReleaseError(
            f"cannot read workspace.package.version from {cargo_path}: {error}"
        ) from error
    return _string(version, "workspace.package.version")


def require_matching_tag(tag: str, version: str) -> None:
    expected = f"v{version}"
    if tag != expected:
        raise ReleaseError(
            f"release tag {tag!r} does not match workspace version; expected {expected!r}"
        )


def require_draft(is_draft: str) -> None:
    if is_draft != "true":
        raise ReleaseError(DRAFT_RELEASE_ERROR)


def _validate_manifest(manifest: Mapping[str, Any], packaging_dir: Path) -> None:
    if manifest.get("schema_version") != 1:
        raise ReleaseError("release manifest schema_version must be 1")

    project = _object(manifest.get("project"), "project")
    for key in ("binary", "alias", "description", "homepage", "repository", "channel"):
        _string(project.get(key), f"project.{key}")
    if project["channel"] != "stable":
        raise ReleaseError("project.channel must remain 'stable'")

    archive = _object(manifest.get("archive"), "archive")
    stem = _string(archive.get("stem"), "archive.stem")
    if stem != "thegn-{tag}-{target}":
        raise ReleaseError("archive.stem must preserve the binstall archive contract")
    if archive.get("checksum_suffix") != ".sha256":
        raise ReleaseError("archive.checksum_suffix must be '.sha256'")
    formats = _object(archive.get("formats"), "archive.formats")
    if formats != {"unix": "tar.gz", "windows": "zip"}:
        raise ReleaseError("archive.formats must map unix to tar.gz and windows to zip")
    root_files = archive.get("root_files")
    if root_files != ["thegn", "LICENSE-MIT", "LICENSE-APACHE", "README.md"]:
        raise ReleaseError("archive.root_files does not match the release archive layout")

    targets = _object(manifest.get("targets"), "targets")
    if not targets:
        raise ReleaseError("targets must not be empty")
    for target, raw in targets.items():
        if not isinstance(target, str) or TARGET_RE.fullmatch(target) is None:
            raise ReleaseError(f"invalid Rust target name: {target!r}")
        config = _object(raw, f"targets.{target}")
        if not isinstance(config.get("enabled"), bool):
            raise ReleaseError(f"targets.{target}.enabled must be a boolean")
        platform = _string(config.get("platform"), f"targets.{target}.platform")
        if platform not in formats:
            raise ReleaseError(f"targets.{target}.platform is unknown: {platform!r}")
        binary = config.get("binary", project["binary"])
        _string(binary, f"targets.{target}.binary")

    dependencies = _object(manifest.get("runtime_dependencies"), "runtime_dependencies")
    for name in ("homebrew_optional", "aur_optional", "deb", "rpm"):
        values = dependencies.get(name)
        if not isinstance(values, list) or not values or not all(
            isinstance(item, str) and item for item in values
        ):
            raise ReleaseError(f"runtime_dependencies.{name} must be a non-empty string list")

    managers = _object(manifest.get("managers"), "managers")
    required = {"homebrew", "aur", "nfpm", "scoop", "winget"}
    if set(managers) != required:
        raise ReleaseError(f"managers must contain exactly {sorted(required)}")
    outputs: set[Path] = set()
    for name, raw in managers.items():
        config = _object(raw, f"managers.{name}")
        if not isinstance(config.get("enabled"), bool):
            raise ReleaseError(f"managers.{name}.enabled must be a boolean")
        target = _string(config.get("target"), f"managers.{name}.target")
        if target not in targets:
            raise ReleaseError(f"managers.{name} refers to unknown target {target!r}")
        template = _safe_relative(config.get("template"), f"managers.{name}.template")
        if name == "nfpm":
            architectures = _object(config.get("architectures"), "managers.nfpm.architectures")
            manager_outputs = _object(config.get("outputs"), "managers.nfpm.outputs")
            if set(architectures) != {"deb", "rpm"} or set(manager_outputs) != {"deb", "rpm"}:
                raise ReleaseError("nfpm must define deb and rpm architectures and outputs")
            for packager in ("deb", "rpm"):
                _string(architectures[packager], f"managers.nfpm.architectures.{packager}")
                output = _safe_relative(
                    manager_outputs[packager], f"managers.nfpm.outputs.{packager}"
                )
                if output in outputs:
                    raise ReleaseError(f"duplicate manager output path: {output}")
                outputs.add(output)
        else:
            output = _safe_relative(config.get("output"), f"managers.{name}.output")
            if output in outputs:
                raise ReleaseError(f"duplicate manager output path: {output}")
            outputs.add(output)
        template_path = packaging_dir / template
        if not template_path.is_file():
            raise ReleaseError(f"required template does not exist: {template_path}")
        _validate_template(template_path, MANAGER_PLACEHOLDERS[name])


def _validate_template(path: Path, expected: set[str]) -> None:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ReleaseError(f"cannot read template {path}: {error}") from error
    if "REPLACE_WITH_" in text or "thegn-dev" in text or ".SRCINFO" in text:
        raise ReleaseError(f"template contains forbidden release state: {path}")
    placeholders = set(PLACEHOLDER_RE.findall(text))
    if placeholders != expected:
        raise ReleaseError(
            f"template placeholders do not match the {path.name} renderer: "
            f"expected {sorted(expected)}, got {sorted(placeholders)}"
        )


def selected_managers(
    manifest: Mapping[str, Any], requested: Sequence[str] | None
) -> list[str]:
    managers = _object(manifest.get("managers"), "managers")
    if requested:
        unknown = sorted(set(requested) - set(managers))
        if unknown:
            raise ReleaseError(f"unknown package manager(s): {', '.join(unknown)}")
        names = list(dict.fromkeys(requested))
    else:
        names = [name for name, config in managers.items() if config["enabled"]]

    targets = _object(manifest.get("targets"), "targets")
    for name in names:
        config = managers[name]
        target = config["target"]
        if not config["enabled"] or not targets[target]["enabled"]:
            if name in {"scoop", "winget"} or "windows" in target:
                raise ReleaseError(f"{WINDOWS_ERROR}: cannot render {name}")
            raise ReleaseError(f"package manager {name!r} is not enabled")
    return names


def _asset_names(
    manifest: Mapping[str, Any], tag: str, target: str
) -> tuple[str, str]:
    archive = manifest["archive"]
    platform = manifest["targets"][target]["platform"]
    extension = archive["formats"][platform]
    stem = archive["stem"].format(tag=tag, target=target)
    return f"{stem}.{extension}", f"{stem}{archive['checksum_suffix']}"


def _read_checksum(path: Path, archive_name: str) -> str:
    try:
        line = path.read_text(encoding="utf-8").strip()
    except OSError as error:
        raise ReleaseError(f"cannot read checksum file {path}: {error}") from error
    fields = line.split()
    if not fields or SHA256_RE.fullmatch(fields[0]) is None:
        raise ReleaseError(f"checksum in {path} must be lowercase 64-hex SHA-256")
    if len(fields) > 2:
        raise ReleaseError(f"checksum file {path} must contain exactly one checksum")
    if len(fields) == 2 and fields[1].lstrip("*") != archive_name:
        raise ReleaseError(
            f"checksum file {path} names {fields[1]!r}, expected {archive_name!r}"
        )
    return fields[0]


def _sha256(path: Path) -> str:
    try:
        if path.stat().st_size > MAX_ARCHIVE_BYTES:
            raise ReleaseError(
                f"release archive {path} exceeds {MAX_ARCHIVE_BYTES} bytes"
            )
    except OSError as error:
        raise ReleaseError(f"cannot inspect release archive {path}: {error}") from error
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
    except OSError as error:
        raise ReleaseError(f"cannot read release archive {path}: {error}") from error
    return digest.hexdigest()


def load_trusted_digests(paths: Sequence[Path]) -> dict[str, str]:
    trusted: dict[str, str] = {}

    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for name, digest in pairs:
            if name in value:
                raise ReleaseError(f"trusted digest for {name} given twice")
            value[name] = digest
        return value

    for path in paths:
        try:
            metadata = path.lstat()
        except OSError as error:
            raise ReleaseError(f"cannot inspect trusted digests {path}: {error}") from error
        if not stat.S_ISREG(metadata.st_mode):
            raise ReleaseError(f"trusted digests are not a regular file: {path}")
        if metadata.st_size > MAX_TRUSTED_DIGESTS_BYTES:
            raise ReleaseError(
                f"trusted digests {path} exceed {MAX_TRUSTED_DIGESTS_BYTES} bytes"
            )
        try:
            value = json.loads(
                path.read_text(encoding="utf-8"), object_pairs_hook=unique_object
            )
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ReleaseError(f"cannot read trusted digests {path}: {error}") from error
        value = _object(value, f"trusted digests in {path}")
        for name, digest in value.items():
            if (
                not isinstance(name, str)
                or name in {".", ".."}
                or TRUSTED_NAME_RE.fullmatch(name) is None
            ):
                raise ReleaseError(f"invalid trusted digest asset name: {name!r}")
            if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
                raise ReleaseError(
                    f"trusted digest for {name} must be lowercase 64-hex SHA-256"
                )
            if name in trusted:
                raise ReleaseError(f"trusted digest for {name} given twice")
            trusted[name] = digest
    if not trusted:
        raise ReleaseError("trusted digests must not be empty")
    return trusted


def require_trusted_digests(
    assets_dir: Path, names: Iterable[str], trusted: Mapping[str, str]
) -> None:
    for name in names:
        if name not in trusted:
            raise ReleaseError(f"no trusted digest for {name}")
        actual = _sha256(assets_dir / name)
        expected = trusted[name]
        if actual != expected:
            raise ReleaseError(
                f"{name} does not match the producer job's digest: "
                f"expected {expected}, got {actual}"
            )


def _archive_required_files(manifest: Mapping[str, Any], target: str) -> list[str]:
    project = manifest["project"]
    target_config = manifest["targets"][target]
    required = list(manifest["archive"]["root_files"])
    required[0] = target_config.get("binary", project["binary"])
    return required


def _hash_open_file(handle: Any, path: Path, label: str = "release archive") -> str:
    digest = hashlib.sha256()
    total = 0
    try:
        handle.seek(0)
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            total += len(block)
            if total > MAX_ARCHIVE_BYTES:
                raise ReleaseError(
                    f"{label} {path} exceeds {MAX_ARCHIVE_BYTES} bytes"
                )
            digest.update(block)
        handle.seek(0)
    except OSError as error:
        raise ReleaseError(f"cannot read {label} {path}: {error}") from error
    return digest.hexdigest()


def _hash_payload_file(path: Path) -> str:
    descriptor: int | None = None
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise ReleaseError(f"payload entry is not a regular file: {path}")
        if metadata.st_size > MAX_ARCHIVE_BYTES:
            raise ReleaseError(f"payload file {path} exceeds {MAX_ARCHIVE_BYTES} bytes")
        with os.fdopen(descriptor, "rb") as handle:
            descriptor = None
            return _hash_open_file(handle, path, "payload file")
    except OSError as error:
        raise ReleaseError(f"cannot read payload file {path}: {error}") from error
    finally:
        if descriptor is not None:
            os.close(descriptor)


def stage_archive(
    manifest: Mapping[str, Any],
    tag: str,
    assets_dir: Path,
    target: str,
    output_dir: Path,
    trusted: Mapping[str, str],
) -> dict[str, str]:
    target_config = manifest["targets"].get(target)
    if target_config is None or target_config.get("platform") != "unix":
        raise ReleaseError(f"archive staging supports unix targets only: {target}")
    archive_name, _ = _asset_names(manifest, tag, target)
    archive_path = assets_dir / archive_name
    require_trusted_digests(assets_dir, [archive_name], trusted)
    required = _archive_required_files(manifest, target)
    try:
        layout_stat = archive_path.lstat()
    except OSError as error:
        raise ReleaseError(f"cannot inspect release archive {archive_path}: {error}") from error
    if not stat.S_ISREG(layout_stat.st_mode):
        raise ReleaseError(f"release archive is not a regular file: {archive_path}")
    _validate_archive_layout(archive_path, "unix", required)

    parent = output_dir.parent
    try:
        parent_mode = parent.lstat().st_mode
    except OSError as error:
        raise ReleaseError(f"cannot inspect staging parent {parent}: {error}") from error
    if not stat.S_ISDIR(parent_mode):
        raise ReleaseError(f"staging parent is not a real directory: {parent}")
    if os.path.lexists(output_dir):
        raise ReleaseError(f"staging output already exists: {output_dir}")

    temp_path: Path | None = None
    temp_name: str | None = None
    parent_fd: int | None = None
    temp_fd: int | None = None
    archive_fd: int | None = None
    try:
        parent_fd = os.open(parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        parent_stat = os.fstat(parent_fd)
        current_parent = parent.lstat()
        if not stat.S_ISDIR(current_parent.st_mode) or (parent_stat.st_dev, parent_stat.st_ino) != (
            current_parent.st_dev,
            current_parent.st_ino,
        ):
            raise ReleaseError(f"staging parent changed during validation: {parent}")
        temp_path = Path(tempfile.mkdtemp(prefix=".stage-", dir=parent))
        temp_name = temp_path.name
        temp_fd = os.open(temp_path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        temp_stat = os.fstat(temp_fd)
        current_parent = parent.lstat()
        current_temp = temp_path.lstat()
        if (
            not stat.S_ISDIR(current_parent.st_mode)
            or (parent_stat.st_dev, parent_stat.st_ino)
            != (current_parent.st_dev, current_parent.st_ino)
            or not stat.S_ISDIR(current_temp.st_mode)
            or (temp_stat.st_dev, temp_stat.st_ino)
            != (current_temp.st_dev, current_temp.st_ino)
        ):
            raise ReleaseError(f"staging path changed during creation: {temp_path}")

        archive_fd = os.open(archive_path, os.O_RDONLY | os.O_NOFOLLOW)
        archive_stat = os.fstat(archive_fd)
        if not stat.S_ISREG(archive_stat.st_mode):
            raise ReleaseError(f"release archive is not a regular file: {archive_path}")
        if (layout_stat.st_dev, layout_stat.st_ino) != (archive_stat.st_dev, archive_stat.st_ino):
            raise ReleaseError(f"release archive changed during layout validation: {archive_path}")
        with os.fdopen(os.dup(archive_fd), "rb") as archive_file:
            actual = _hash_open_file(archive_file, archive_path)
        expected = trusted[archive_name]
        if actual != expected:
            raise ReleaseError(
                f"{archive_name} does not match the producer job's digest: "
                f"expected {expected}, got {actual}"
            )

        staged: dict[str, str] = {}
        with os.fdopen(os.dup(archive_fd), "rb") as archive_file:
            with tarfile.open(fileobj=archive_file, mode="r:gz") as archive:
                seen: set[str] = set()
                for member in archive:
                    _validate_tar_member_headers(archive_path, member)
                    name = member.name.removeprefix("./").rstrip("/")
                    if not member.isfile() or name not in required:
                        continue
                    if name in seen:
                        raise ReleaseError(
                            f"archive {archive_path} repeats required member {name!r}"
                        )
                    seen.add(name)
                    source = archive.extractfile(member)
                    if source is None:
                        raise ReleaseError(f"cannot read archive member {name!r} from {archive_path}")
                    output_fd = os.open(
                        name,
                        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                        0o600,
                        dir_fd=temp_fd,
                    )
                    digest = hashlib.sha256()
                    copied = 0
                    try:
                        with os.fdopen(output_fd, "wb") as destination:
                            while block := source.read(1024 * 1024):
                                copied += len(block)
                                if copied > member.size or copied > MAX_MEMBER_BYTES:
                                    raise ReleaseError(
                                        f"archive member {name!r} exceeds its admitted size"
                                    )
                                destination.write(block)
                                digest.update(block)
                            if copied != member.size:
                                raise ReleaseError(
                                    f"archive member {name!r} ended before its admitted size"
                                )
                            os.fsync(destination.fileno())
                            os.fchmod(
                                destination.fileno(),
                                0o755 if name == required[0] else 0o644,
                            )
                    finally:
                        source.close()
                    staged[name] = digest.hexdigest()
        missing = [name for name in required if name not in staged]
        if missing:
            raise ReleaseError(
                f"archive {archive_path} is missing root file(s): {', '.join(missing)}"
            )
        os.fsync(temp_fd)
        if os.path.lexists(output_dir):
            raise ReleaseError(f"staging output already exists: {output_dir}")
        os.rename(temp_name, output_dir.name, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
        temp_name = None
        output_stat = output_dir.lstat()
        if (temp_stat.st_dev, temp_stat.st_ino) != (output_stat.st_dev, output_stat.st_ino):
            raise ReleaseError(f"staging output changed during handoff: {output_dir}")
        os.fsync(parent_fd)
        return staged
    except ReleaseError:
        raise
    except (OSError, EOFError, tarfile.TarError, zlib.error) as error:
        raise ReleaseError(f"cannot stage release archive {archive_path}: {error}") from error
    finally:
        if archive_fd is not None:
            os.close(archive_fd)
        if temp_fd is not None:
            os.close(temp_fd)
        if temp_name is not None and parent_fd is not None:
            shutil.rmtree(parent / temp_name, ignore_errors=True)
        if parent_fd is not None:
            os.close(parent_fd)


def verify_package_payload(staged: Mapping[str, str], payload_root: Path, label: str) -> None:
    expected = {
        "usr/bin/thegn": ("thegn", "file"),
        "usr/bin/tg": (None, "alias"),
        "usr/share/licenses/thegn/LICENSE-MIT": ("LICENSE-MIT", "file"),
        "usr/share/licenses/thegn/LICENSE-APACHE": ("LICENSE-APACHE", "file"),
    }
    for digest_name in ("thegn", "LICENSE-MIT", "LICENSE-APACHE"):
        digest = staged.get(digest_name)
        if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise ReleaseError(
                f"staged digest for {digest_name} must be lowercase 64-hex SHA-256"
            )
    found: set[str] = set()

    def walk_error(error: OSError) -> None:
        raise ReleaseError(f"cannot inspect {label} payload {payload_root}: {error}") from error

    try:
        for root, directories, files in os.walk(
            payload_root, followlinks=False, onerror=walk_error
        ):
            for name in [*directories, *files]:
                path = Path(root) / name
                relative = path.relative_to(payload_root).as_posix()
                metadata = path.lstat()
                if stat.S_ISDIR(metadata.st_mode):
                    continue
                if relative not in expected:
                    raise ReleaseError(f"{label} payload contains unexpected entry {relative}")
                wanted_digest, kind = expected[relative]
                if kind == "alias":
                    if (
                        not stat.S_ISLNK(metadata.st_mode)
                        or os.readlink(path) != "/usr/bin/thegn"
                    ):
                        raise ReleaseError(f"{label} payload has invalid entry {relative}")
                else:
                    if not stat.S_ISREG(metadata.st_mode):
                        raise ReleaseError(f"{label} payload has invalid entry {relative}")
                    if wanted_digest is None:
                        raise ReleaseError(f"invalid payload verifier entry {relative}")
                    actual = _hash_payload_file(path)
                    if actual != staged[wanted_digest]:
                        raise ReleaseError(f"{label} payload bytes differ at {relative}")
                found.add(relative)
    except OSError as error:
        raise ReleaseError(f"cannot inspect {label} payload {payload_root}: {error}") from error
    missing = sorted(set(expected) - found)
    if missing:
        raise ReleaseError(f"{label} payload is missing entry {missing[0]}")


def load_staged_digests(path: Path) -> dict[str, str]:
    descriptor: int | None = None
    try:
        path_metadata = path.lstat()
        if not stat.S_ISREG(path_metadata.st_mode):
            raise ReleaseError(f"staged digests are not a regular file: {path}")
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        metadata = os.fstat(descriptor)
    except OSError as error:
        if descriptor is not None:
            os.close(descriptor)
        raise ReleaseError(f"cannot inspect staged digests {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        raise ReleaseError(f"staged digests are not a regular file: {path}")
    if metadata.st_size > MAX_TRUSTED_DIGESTS_BYTES:
        os.close(descriptor)
        raise ReleaseError(f"staged digests {path} exceed {MAX_TRUSTED_DIGESTS_BYTES} bytes")
    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for name, digest in pairs:
            if name in value:
                raise ReleaseError(f"staged digest for {name} given twice")
            value[name] = digest
        return value

    try:
        with os.fdopen(descriptor, "rb") as handle:
            descriptor = None
            raw = handle.read(MAX_TRUSTED_DIGESTS_BYTES + 1)
            if len(raw) > MAX_TRUSTED_DIGESTS_BYTES:
                raise ReleaseError(f"staged digests {path} exceed {MAX_TRUSTED_DIGESTS_BYTES} bytes")
            value = json.loads(
                raw.decode("utf-8"), object_pairs_hook=unique_object
            )
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReleaseError(f"cannot read staged digests {path}: {error}") from error
    finally:
        if descriptor is not None:
            os.close(descriptor)
    value = _object(value, f"staged digests in {path}")
    required = {"thegn", "LICENSE-MIT", "LICENSE-APACHE", "README.md"}
    if set(value) != required:
        raise ReleaseError("staged digests must contain exactly the four archive root files")
    for name, digest in value.items():
        if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise ReleaseError(f"staged digest for {name} must be lowercase 64-hex SHA-256")
    return value


def load_release_manifest_record(path: Path, tag: str) -> dict[str, Any]:
    descriptor: int | None = None
    try:
        path_metadata = path.lstat()
        if not stat.S_ISREG(path_metadata.st_mode):
            raise ReleaseError(f"release manifest is not a regular file: {path}")
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        metadata = os.fstat(descriptor)
    except OSError as error:
        if descriptor is not None:
            os.close(descriptor)
        raise ReleaseError(f"cannot inspect release manifest {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        raise ReleaseError(f"release manifest is not a regular file: {path}")
    if metadata.st_size > MAX_TRUSTED_DIGESTS_BYTES:
        os.close(descriptor)
        raise ReleaseError(f"release manifest {path} exceeds {MAX_TRUSTED_DIGESTS_BYTES} bytes")

    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for name, item in pairs:
            if name in value:
                raise ReleaseError(f"release manifest {path} has duplicate JSON key {name}")
            value[name] = item
        return value

    try:
        with os.fdopen(descriptor, "rb") as handle:
            descriptor = None
            raw = handle.read(MAX_TRUSTED_DIGESTS_BYTES + 1)
            if len(raw) > MAX_TRUSTED_DIGESTS_BYTES:
                raise ReleaseError(f"release manifest {path} exceeds {MAX_TRUSTED_DIGESTS_BYTES} bytes")
            value = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise ReleaseError(f"cannot read release manifest {path}: {error}") from error
    finally:
        if descriptor is not None:
            os.close(descriptor)

    value = _object(value, f"release manifest {path}")
    required = {"schema_version", "tag", "commit", "tag_object", "workflow_sha", "run_id", "assets"}
    if set(value) != required:
        raise ReleaseError(f"release manifest {path} must contain exactly the producer fields")
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise ReleaseError(f"release manifest {path} schema_version must be integer 1")
    if value["tag"] != tag:
        raise ReleaseError(f"release manifest {path} tag {value['tag']!r} does not match requested tag {tag!r}")
    for field_name in ("commit", "tag_object", "workflow_sha", "run_id"):
        _string(value[field_name], f"release manifest {path} {field_name}")
    assets = value["assets"]
    if not isinstance(assets, list) or not assets:
        raise ReleaseError(f"release manifest {path} assets must be a non-empty list")
    entries: dict[str, tuple[str, int]] = {}
    for item in assets:
        asset = _object(item, f"release manifest asset in {path}")
        if set(asset) != {"name", "sha256", "size"}:
            raise ReleaseError(f"release manifest asset in {path} must contain exactly name, sha256, and size")
        name = _string(asset["name"], f"release manifest asset name in {path}")
        if name in (".", "..") or TRUSTED_NAME_RE.fullmatch(name) is None:
            raise ReleaseError(f"release manifest asset name in {path} is invalid: {name!r}")
        digest = asset["sha256"]
        if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise ReleaseError(f"release manifest asset {name} sha256 in {path} must be lowercase 64-hex SHA-256")
        size = asset["size"]
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise ReleaseError(f"release manifest asset {name} size in {path} must be a non-negative integer")
        if name in entries:
            raise ReleaseError(f"release manifest {path} has duplicate asset name {name}")
        entries[name] = (digest, size)
    return value


def load_release_manifest(path: Path, tag: str) -> dict[str, tuple[str, int]]:
    record = load_release_manifest_record(path, tag)
    return {
        item["name"]: (item["sha256"], item["size"])
        for item in record["assets"]
    }


def verify_release_asset_files(
    entries: Mapping[str, tuple[str, int]], paths: Sequence[Path], *, exact: bool = False
) -> dict[str, str]:
    verified: dict[str, str] = {}
    for path in paths:
        name = path.name
        if name not in entries:
            raise ReleaseError(f"{name} is not in the release manifest")
        if name in verified:
            raise ReleaseError(f"{name} was given more than once")
        expected_digest, expected_size = entries[name]
        descriptor: int | None = None
        try:
            path_metadata = path.lstat()
            if not stat.S_ISREG(path_metadata.st_mode):
                raise ReleaseError(f"{name} is not a regular file")
            descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
            metadata = os.fstat(descriptor)
        except OSError as error:
            if descriptor is not None:
                os.close(descriptor)
            raise ReleaseError(f"cannot inspect release asset {path}: {error}") from error
        if not stat.S_ISREG(metadata.st_mode):
            os.close(descriptor)
            raise ReleaseError(f"{name} is not a regular file")
        if metadata.st_size != expected_size:
            os.close(descriptor)
            raise ReleaseError(f"{name} size does not match the release manifest: expected {expected_size}, got {metadata.st_size}")
        try:
            with os.fdopen(descriptor, "rb") as handle:
                descriptor = None
                actual_digest = _hash_open_file(handle, path, "release asset")
        finally:
            if descriptor is not None:
                os.close(descriptor)
        if actual_digest != expected_digest:
            raise ReleaseError(
                f"{name} does not match the release manifest: expected {expected_digest}, got {actual_digest}"
            )
        verified[name] = actual_digest
    if exact and set(verified) != set(entries):
        missing = sorted(set(entries) - set(verified))
        raise ReleaseError(
            "release asset files do not exactly match the manifest; missing: "
            f"{', '.join(missing) or '(none)'}"
        )
    return verified


def verify_downloaded_assets(
    manifest: Mapping[str, Any],
    tag: str,
    assets_dir: Path,
    trusted: Mapping[str, str],
) -> list[str]:
    required: list[str] = []
    for target, config in manifest["targets"].items():
        if config["enabled"]:
            required.extend(_asset_names(manifest, tag, target))
    completions_name = f"thegn-{tag}-completions.tar.gz"
    required.extend((completions_name, f"{completions_name}.sha256"))
    required = sorted(required)

    found: set[str] = set()
    try:
        entries = list(os.scandir(assets_dir))
    except OSError as error:
        raise ReleaseError(f"cannot list downloaded assets {assets_dir}: {error}") from error
    for entry in entries:
        try:
            metadata = entry.stat(follow_symlinks=False)
        except OSError as error:
            raise ReleaseError(f"cannot inspect downloaded asset {entry.name}: {error}") from error
        if not stat.S_ISREG(metadata.st_mode):
            raise ReleaseError(f"downloaded asset is not a regular file: {entry.name}")
        found.add(entry.name)
    missing = sorted(set(required) - found)
    unexpected = sorted(found - set(required))
    if missing or unexpected:
        raise ReleaseError(
            f"release asset set mismatch; missing: {', '.join(missing) or '(none)'}; "
            f"unexpected: {', '.join(unexpected) or '(none)'}"
        )
    require_trusted_digests(assets_dir, required, trusted)
    return required


def _safe_archive_names(names: Iterable[str], path: Path) -> set[str]:
    normalized: set[str] = set()
    for name in names:
        pure = PurePosixPath(name)
        if pure.is_absolute() or ".." in pure.parts:
            raise ReleaseError(f"archive {path} contains unsafe path {name!r}")
        clean = "/".join(part for part in pure.parts if part not in ("", "."))
        if clean:
            clean = clean.rstrip("/")
            if clean in normalized:
                raise ReleaseError(f"archive {path} contains duplicate path {clean!r}")
            normalized.add(clean)
    return normalized


def _validate_tar_member_headers(path: Path, member: tarfile.TarInfo) -> None:
    if member.issparse() or any(
        key.startswith("GNU.sparse.") for key in member.pax_headers
    ):
        raise ReleaseError(f"archive {path} member {member.name!r} is a sparse file")
    unsupported = sorted(set(member.pax_headers) - POSIX_PAX_HEADERS)
    if unsupported:
        headers = ", ".join(repr(key) for key in unsupported)
        raise ReleaseError(
            f"archive {path} member {member.name!r} has unsupported PAX header(s): {headers}"
        )


def _validate_archive_layout(
    path: Path, platform: str, required_files: Sequence[str]
) -> None:
    required = set(required_files)
    expanded = 0
    count = 0
    try:
        compressed = path.stat().st_size
        if platform == "unix":
            with tarfile.open(path, "r:gz") as archive:
                names: set[str] = set()
                regular: set[str] = set()
                unsafe: list[str] = []
                thegn_executable = True
                for member in archive:
                    _validate_tar_member_headers(path, member)
                    count += 1
                    if count > MAX_MEMBERS:
                        raise ReleaseError(
                            f"archive {path} exceeds {MAX_MEMBERS} members"
                        )
                    if len(member.name) > MAX_PATH_CHARS:
                        raise ReleaseError(f"archive {path} has an overlong path")
                    clean_names = _safe_archive_names((member.name,), path)
                    duplicates = names & clean_names
                    if duplicates:
                        raise ReleaseError(
                            f"archive {path} contains duplicate path {min(duplicates)!r}"
                        )
                    names.update(clean_names)
                    if member.size > MAX_MEMBER_BYTES:
                        raise ReleaseError(
                            f"archive {path} member exceeds {MAX_MEMBER_BYTES} bytes"
                        )
                    if member.isdir() and member.name.rstrip("/") in ("", "."):
                        continue
                    if member.isdir():
                        continue
                    if not member.isfile():
                        unsafe.append(member.name)
                        continue
                    mode = member.mode
                    if mode & (stat.S_ISUID | stat.S_ISGID | stat.S_ISVTX | 0o002):
                        raise ReleaseError(
                            f"archive {path} member {member.name!r} has unsafe mode {oct(mode)}"
                        )
                    expanded += member.size
                    if expanded > MAX_EXPANDED_BYTES:
                        raise ReleaseError(
                            f"archive {path} exceeds {MAX_EXPANDED_BYTES} expanded bytes"
                        )
                    if expanded > MAX_COMPRESSION_RATIO * max(compressed, 1):
                        raise ReleaseError(
                            f"archive {path} exceeds compression ratio {MAX_COMPRESSION_RATIO}"
                        )
                    regular.add(member.name.rstrip("/"))
                    if member.name.rstrip("/") == "thegn" and not member.mode & 0o111:
                        thegn_executable = False
        else:
            with zipfile.ZipFile(path) as archive:
                names = set()
                unsafe = []
                regular = set()
                for member in archive.infolist():
                    count += 1
                    if count > MAX_MEMBERS:
                        raise ReleaseError(
                            f"archive {path} exceeds {MAX_MEMBERS} members"
                        )
                    if len(member.filename) > MAX_PATH_CHARS:
                        raise ReleaseError(f"archive {path} has an overlong path")
                    clean_names = _safe_archive_names((member.filename,), path)
                    duplicates = names & clean_names
                    if duplicates:
                        raise ReleaseError(
                            f"archive {path} contains duplicate path {min(duplicates)!r}"
                        )
                    names.update(clean_names)
                    if member.file_size > MAX_MEMBER_BYTES:
                        raise ReleaseError(
                            f"archive {path} member exceeds {MAX_MEMBER_BYTES} bytes"
                        )
                    if member.is_dir() and member.filename.rstrip("/") in ("", "."):
                        continue
                    mode = member.external_attr >> 16
                    file_type = stat.S_IFMT(mode)
                    if file_type not in (0, stat.S_IFREG, stat.S_IFDIR):
                        unsafe.append(member.filename)
                    elif not member.is_dir():
                        if mode and mode & (
                            stat.S_ISUID | stat.S_ISGID | stat.S_ISVTX | 0o002
                        ):
                            raise ReleaseError(
                                f"archive {path} member {member.filename!r} has unsafe mode {oct(mode)}"
                            )
                        expanded += member.file_size
                        if expanded > MAX_EXPANDED_BYTES:
                            raise ReleaseError(
                                f"archive {path} exceeds {MAX_EXPANDED_BYTES} expanded bytes"
                            )
                        if expanded > MAX_COMPRESSION_RATIO * max(compressed, 1):
                            raise ReleaseError(
                                f"archive {path} exceeds compression ratio {MAX_COMPRESSION_RATIO}"
                            )
                        regular.add(member.filename.rstrip("/"))
    except (OSError, EOFError, zlib.error, tarfile.TarError, zipfile.BadZipFile) as error:
        raise ReleaseError(f"cannot inspect release archive {path}: {error}") from error
    if unsafe:
        raise ReleaseError(
            f"archive {path} contains non-regular member(s): {', '.join(unsafe)}"
        )
    extra = names - required
    if extra:
        raise ReleaseError(
            f"archive {path} contains unexpected member(s): {', '.join(sorted(extra))}"
        )
    missing = [name for name in required_files if name not in names]
    if missing:
        raise ReleaseError(f"archive {path} is missing root file(s): {', '.join(missing)}")
    non_regular = [name for name in required_files if name not in regular]
    if non_regular:
        raise ReleaseError(
            f"archive {path} has non-regular required file(s): {', '.join(non_regular)}"
        )
    if platform == "unix" and not thegn_executable:
        raise ReleaseError(f"archive {path} binary thegn is not executable")


def validate_assets(
    manifest: Mapping[str, Any],
    tag: str,
    assets_dir: Path,
    managers: Sequence[str],
) -> dict[str, Asset]:
    project = manifest["project"]
    repository = project["repository"].rstrip("/")
    targets = manifest["targets"]
    target_names = list(dict.fromkeys(manifest["managers"][name]["target"] for name in managers))
    assets: dict[str, Asset] = {}
    if not assets_dir.is_dir():
        raise ReleaseError(f"release asset directory does not exist: {assets_dir}")
    for target in target_names:
        archive_name, checksum_name = _asset_names(manifest, tag, target)
        archive_path = assets_dir / archive_name
        checksum_path = assets_dir / checksum_name
        if not archive_path.is_file():
            raise ReleaseError(f"missing release archive: {archive_path}")
        if not checksum_path.is_file():
            raise ReleaseError(f"missing release checksum: {checksum_path}")
        checksum = _read_checksum(checksum_path, archive_name)
        actual = _sha256(archive_path)
        if actual != checksum:
            raise ReleaseError(
                f"checksum mismatch for {archive_name}: expected {checksum}, got {actual}"
            )
        target_config = targets[target]
        required = list(manifest["archive"]["root_files"])
        required[0] = target_config.get("binary", project["binary"])
        _validate_archive_layout(archive_path, target_config["platform"], required)
        url = f"{repository}/releases/download/{tag}/{archive_name}"
        assets[target] = Asset(target, archive_path, checksum_path, checksum, url)
    return assets


def expected_release_assets(manifest: Mapping[str, Any], tag: str) -> list[str]:
    names: list[str] = []
    for target, config in manifest["targets"].items():
        if config["enabled"]:
            names.extend(_asset_names(manifest, tag, target))
    names.extend(
        (
            f"thegn-{tag}-completions.tar.gz",
            f"thegn-{tag}-completions.tar.gz.sha256",
            f"thegn-{tag}-release.json",
            f"thegn-{tag}-homebrew.rb",
            f"thegn-{tag}-PKGBUILD",
            f"thegn-{tag}-deb.nfpm.yaml",
            f"thegn-{tag}-rpm.nfpm.yaml",
            "SHA256SUMS",
            f"thegn-{tag}-THIRD_PARTY_NOTICES.md",
        )
    )
    version = tag[1:]
    names.extend((f"thegn_{version}_amd64.deb", f"thegn-{version}-1.x86_64.rpm"))
    return sorted(names)


def build_release_manifest(
    manifest: Mapping[str, Any],
    tag: str,
    commit: str,
    assets_dir: Path,
    record_path: Path,
    trusted: Mapping[str, str] | None = None,
) -> dict[str, Any]:
    record_name = f"thegn-{tag}-release.json"
    if record_path.name != record_name or record_path.parent.resolve() != assets_dir.resolve():
        raise ReleaseError(f"upload release record must be {record_name} inside the assets directory")
    if record_path.is_symlink():
        raise ReleaseError(f"upload release record is not a regular file: {record_name}")
    assets_dir = assets_dir.resolve()
    if not assets_dir.is_dir():
        raise ReleaseError(f"release asset directory does not exist: {assets_dir}")
    found: dict[str, Path] = {}
    try:
        entries = list(os.scandir(assets_dir))
    except OSError as error:
        raise ReleaseError(f"cannot list release asset directory {assets_dir}: {error}") from error
    ignored_manifest = RELEASE_MANIFEST_NAME.format(tag=tag)
    for entry in entries:
        path = Path(entry.path)
        try:
            metadata = entry.stat(follow_symlinks=False)
        except OSError as error:
            raise ReleaseError(f"cannot inspect release asset {path}: {error}") from error
        if not stat.S_ISREG(metadata.st_mode):
            raise ReleaseError(f"release asset is not a regular file: {path.name}")
        if entry.name != ignored_manifest:
            found[entry.name] = path

    expected = expected_release_assets(manifest, tag)
    missing = sorted(set(expected) - set(found))
    unexpected = sorted(set(found) - set(expected))
    if missing or unexpected:
        raise ReleaseError(
            f"release asset set mismatch; missing: {', '.join(missing) or '(none)'}; "
            f"unexpected: {', '.join(unexpected) or '(none)'}"
        )
    if trusted is not None:
        trusted_missing = sorted(set(expected) - set(trusted))
        trusted_unexpected = sorted(set(trusted) - set(expected))
        if trusted_missing or trusted_unexpected:
            raise ReleaseError(
                "trusted producer digests do not cover exactly the release asset set; "
                f"missing: {', '.join(trusted_missing) or '(none)'}; "
                f"unexpected: {', '.join(trusted_unexpected) or '(none)'}"
            )
        require_trusted_digests(assets_dir, expected, trusted)

    archive_names: list[str] = []
    for target, config in manifest["targets"].items():
        if config["enabled"]:
            archive_name, checksum_name = _asset_names(manifest, tag, target)
            archive_names.extend((archive_name, checksum_name))
    completions_name = f"thegn-{tag}-completions.tar.gz"
    completions_checksum = f"{completions_name}.sha256"
    archive_names.extend((completions_name, completions_checksum))
    for archive_name, checksum_name in zip(archive_names[::2], archive_names[1::2]):
        actual = _sha256(found[archive_name])
        recorded = _read_checksum(found[checksum_name], archive_name)
        if recorded != actual:
            raise ReleaseError(
                f"checksum mismatch for {archive_name}: expected {recorded}, got {actual}"
            )

    try:
        record = json.loads(record_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ReleaseError(f"cannot read upload release record {record_path}: {error}") from error
    record = _object(record, "upload release record")
    if record.get("tag") != tag:
        raise ReleaseError("upload release record tag does not match")
    if record.get("commit") != commit:
        raise ReleaseError("upload release record commit does not match admitted commit")
    tag_object = _string(record.get("tag_object"), "record.tag_object")
    workflow_sha = _string(record.get("workflow_sha"), "record.workflow_sha")
    run_id = _string(record.get("run_id"), "record.run_id")
    record_assets = record.get("assets")
    if not isinstance(record_assets, list):
        raise ReleaseError("upload release record assets must be a list")
    expected_archives = sorted(archive_names)
    recorded_assets: dict[str, str] = {}
    for item in record_assets:
        asset = _object(item, "record asset")
        name = _string(asset.get("name"), "record asset name")
        digest = _string(asset.get("sha256"), "record asset sha256")
        if name in recorded_assets:
            raise ReleaseError(f"upload release record has duplicate asset {name}")
        if SHA256_RE.fullmatch(digest) is None:
            raise ReleaseError(f"record asset {name} sha256 must be lowercase 64-hex SHA-256")
        recorded_assets[name] = digest
    if sorted(recorded_assets) != expected_archives:
        raise ReleaseError("upload release record assets do not exactly match archive/checksum names")
    for name in expected_archives:
        if recorded_assets[name] != _sha256(found[name]):
            raise ReleaseError(f"upload release record digest mismatch for {name}")

    managers = selected_managers(manifest, None)
    validate_assets(manifest, tag, assets_dir, managers)
    for target, config in manifest["targets"].items():
        if config["enabled"]:
            archive_name, _ = _asset_names(manifest, tag, target)
            required = list(manifest["archive"]["root_files"])
            required[0] = config.get("binary", manifest["project"]["binary"])
            _validate_archive_layout(found[archive_name], config["platform"], required)

    result_assets = []
    for name in expected:
        path = found[name]
        result_assets.append({"name": name, "sha256": _sha256(path), "size": path.stat().st_size})
    value = {
        "schema_version": 1,
        "tag": tag,
        "commit": commit,
        "tag_object": tag_object,
        "workflow_sha": workflow_sha,
        "run_id": run_id,
        "assets": result_assets,
    }
    return value


def verify_release_generation(
    path: Path, *, tag: str, commit: str, tag_object: str, workflow_sha: str, run_id: str
) -> None:
    """Require the draft's producer record to name this admitted workflow run."""
    try:
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            raise ReleaseError(f"release generation record is not a regular file: {path}")
        if metadata.st_size > MAX_TRUSTED_DIGESTS_BYTES:
            raise ReleaseError(
                f"release generation record {path} exceeds {MAX_TRUSTED_DIGESTS_BYTES} bytes"
            )
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except OSError as error:
        raise ReleaseError(f"cannot inspect release generation record {path}: {error}") from error
    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for field, item in pairs:
            if field in value:
                raise ReleaseError(f"release generation record {path} has duplicate field {field}")
            value[field] = item
        return value

    try:
        with os.fdopen(descriptor, "r", encoding="utf-8") as handle:
            record = json.load(handle, object_pairs_hook=unique_object)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise ReleaseError(f"cannot read release generation record {path}: {error}") from error
    record = _object(record, "release generation record")
    required_fields = {"tag", "commit", "tag_object", "workflow_sha", "run_id", "assets"}
    if set(record) != required_fields or not isinstance(record.get("assets"), list):
        raise ReleaseError("release generation record has an invalid field set")
    expected = {
        "tag": tag,
        "commit": commit,
        "tag_object": tag_object,
        "workflow_sha": workflow_sha,
        "run_id": run_id,
    }
    for field, value in expected.items():
        if record.get(field) != value:
            raise ReleaseError(f"release draft generation {field} does not match this workflow")


def write_release_manifest(output: Path, assets_dir: Path, value: Mapping[str, Any]) -> None:
    try:
        assets_root = assets_dir.resolve()
        if output.is_symlink():
            raise ReleaseError(f"refusing to write release manifest through symlink: {output}")
        output_parent = output.parent.resolve()
        output_resolved = output_parent / output.name
        if output_resolved == assets_root or assets_root in output_resolved.parents:
            raise ReleaseError("release manifest output must be outside the assets directory")
        output.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except OSError as error:
        raise ReleaseError(f"cannot write release manifest {output}: {error}") from error


def _render_template(path: Path, values: Mapping[str, str]) -> str:
    template = path.read_text(encoding="utf-8")
    placeholders = set(PLACEHOLDER_RE.findall(template))
    missing = sorted(placeholders - set(values))
    unknown = sorted(set(values) - placeholders)
    if missing:
        raise ReleaseError(f"template {path} has unknown placeholder(s): {', '.join(missing)}")
    if unknown:
        raise ReleaseError(f"renderer supplied unused placeholder(s) for {path}: {', '.join(unknown)}")
    rendered = template
    for name in sorted(placeholders):
        rendered = rendered.replace(f"@@{name}@@", values[name])
    if PLACEHOLDER_RE.search(rendered):
        raise ReleaseError(f"template {path} contains an unreplaced placeholder")
    if any(forbidden in rendered for forbidden in FORBIDDEN_OUTPUT):
        raise ReleaseError(f"rendered output from {path} contains forbidden release state")
    return rendered if rendered.endswith("\n") else rendered + "\n"


def arch_pkgver(version: str) -> str:
    """Normalize SemVer prerelease syntax for Arch without changing the tag."""
    normalized = version.replace("-", "_")
    if re.fullmatch(r"[A-Za-z0-9+._]+", normalized) is None:
        raise ReleaseError(f"workspace version cannot be represented as an Arch pkgver: {version!r}")
    return normalized


def _manager_values(
    name: str,
    config: Mapping[str, Any],
    manifest: Mapping[str, Any],
    version: str,
    asset: Asset,
) -> dict[str, str]:
    project = manifest["project"]
    common = {
        "VERSION": version,
        "DESCRIPTION": project["description"],
        "HOMEPAGE": project["homepage"],
        "ARCHIVE_URL": asset.url,
        "SHA256": asset.sha256,
    }
    dependencies = manifest["runtime_dependencies"]
    if name == "homebrew":
        common["OPTIONAL_DEPENDENCIES"] = "\n".join(
            f'  depends_on "{dependency}" => :optional'
            for dependency in dependencies["homebrew_optional"]
        )
    elif name == "aur":
        del common["VERSION"]
        common["PKGVER"] = arch_pkgver(version)
        common["OPTIONAL_DEPENDENCIES"] = "\n".join(
            f"  '{dependency}'" for dependency in dependencies["aur_optional"]
        )
    elif name == "nfpm":
        packager = config["packager"]
        common = {
            "VERSION": version,
            "DESCRIPTION": project["description"],
            "HOMEPAGE": project["homepage"],
            "ARCHITECTURE": config["architecture"],
            "DEPENDENCIES": "\n".join(
                f"  - {dependency}" for dependency in dependencies[packager]
            ),
        }
    elif name == "scoop":
        pass
    elif name == "winget":
        common["SHA256_UPPER"] = asset.sha256.upper()
        del common["SHA256"]
    else:  # Manifest validation makes this unreachable.
        raise ReleaseError(f"no renderer for package manager {name!r}")
    return common


def rendered_files(
    manifest: Mapping[str, Any],
    packaging_dir: Path,
    version: str,
    managers: Sequence[str],
    assets: Mapping[str, Asset],
) -> dict[Path, str]:
    files: dict[Path, str] = {}
    for name in managers:
        config = manifest["managers"][name]
        template = packaging_dir / _safe_relative(config["template"], f"managers.{name}.template")
        asset = assets[config["target"]]
        if name == "nfpm":
            for packager in ("deb", "rpm"):
                variant = dict(config)
                variant["packager"] = packager
                variant["architecture"] = config["architectures"][packager]
                output = _safe_relative(
                    config["outputs"][packager], f"managers.nfpm.outputs.{packager}"
                )
                values = _manager_values(name, variant, manifest, version, asset)
                files[output] = _render_template(template, values)
        else:
            output = _safe_relative(config["output"], f"managers.{name}.output")
            values = _manager_values(name, config, manifest, version, asset)
            files[output] = _render_template(template, values)
    return files


def _validate_rendered_tree(root: Path, expected: Iterable[Path]) -> None:
    actual = sorted(path.relative_to(root) for path in root.rglob("*") if path.is_file())
    wanted = sorted(expected)
    if actual != wanted:
        raise ReleaseError(f"rendered output set differs from manifest: {actual!r} != {wanted!r}")
    for path in actual:
        text = (root / path).read_text(encoding="utf-8")
        if PLACEHOLDER_RE.search(text) or any(item in text for item in FORBIDDEN_OUTPUT):
            raise ReleaseError(f"rendered output failed final validation: {path}")


def atomic_write_output(output_dir: Path, files: Mapping[Path, str]) -> None:
    if output_dir.is_symlink():
        raise ReleaseError(f"output path must not be a symlink: {output_dir}")
    output_dir = output_dir.resolve()
    if output_dir in {Path("/").resolve(), REPOSITORY_ROOT.resolve(), PACKAGING_DIR.resolve()}:
        raise ReleaseError(f"refusing unsafe output directory: {output_dir}")
    output_dir.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{output_dir.name}.tmp-", dir=output_dir.parent))
    backup: Path | None = None
    try:
        for relative, content in sorted(files.items(), key=lambda item: str(item[0])):
            destination = staging / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(content, encoding="utf-8", newline="\n")
        _validate_rendered_tree(staging, files)

        if output_dir.exists() or output_dir.is_symlink():
            if not output_dir.is_dir() or output_dir.is_symlink():
                raise ReleaseError(f"output path is not a directory: {output_dir}")
            backup = output_dir.parent / f".{output_dir.name}.old-{os.getpid()}"
            if backup.exists():
                raise ReleaseError(f"atomic output backup already exists: {backup}")
            os.replace(output_dir, backup)
        try:
            os.replace(staging, output_dir)
        except BaseException:
            if backup is not None and not output_dir.exists():
                os.replace(backup, output_dir)
                backup = None
            raise
        if backup is not None:
            shutil.rmtree(backup)
            backup = None
    finally:
        if staging.exists():
            shutil.rmtree(staging)
        if backup is not None and backup.exists() and not output_dir.exists():
            os.replace(backup, output_dir)


def _attestation_value(value: Any, key: str) -> Any:
    if not isinstance(value, dict) or key not in value:
        raise ReleaseError(f"malformed attestation verification: missing {key}")
    return value[key]


def _attestation_string(value: Any, key: str) -> str:
    result = _attestation_value(value, key)
    if not isinstance(result, str):
        raise ReleaseError(f"malformed attestation verification: missing {key}")
    return result


def _attestation_excerpt(value: str) -> str:
    return repr(value[:200])


def check_attestation_verification(
    verification: Any,
    *,
    archive_sha256: str,
    repository: str,
    server_url: str,
    source_digest: str,
    run_id: str,
    run_attempt: int | None = None,
) -> int:
    arguments = {
        "archive_sha256": (archive_sha256, SHA256_RE),
        "source_digest": (source_digest, re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")),
    }
    for name, (value, pattern) in arguments.items():
        if not isinstance(value, str) or pattern.fullmatch(value) is None:
            shown = repr(value)[:200]
            raise ReleaseError(f"invalid attestation check argument {name}: {shown}")
    if not isinstance(run_id, str) or re.fullmatch(r"[0-9]+", run_id) is None:
        raise ReleaseError(f"invalid attestation check argument run_id: {repr(run_id)[:200]}")
    if run_attempt is not None and (
        isinstance(run_attempt, bool) or not isinstance(run_attempt, int) or run_attempt < 1
    ):
        raise ReleaseError(f"invalid attestation check argument run_attempt: {run_attempt!r}")
    if (
        not isinstance(repository, str)
        or repository.count("/") != 1
        or any(not part for part in repository.split("/"))
    ):
        raise ReleaseError(
            f"invalid attestation check argument repository: {repr(repository)[:200]}"
        )
    if (
        not isinstance(server_url, str)
        or not server_url.startswith("https://")
        or server_url.endswith("/")
    ):
        raise ReleaseError(
            f"invalid attestation check argument server_url: {repr(server_url)[:200]}"
        )
    if not isinstance(verification, list) or not verification:
        raise ReleaseError("malformed attestation verification: expected a non-empty array")

    expected_repository = f"{server_url}/{repository}"
    expected_workflow = f"{expected_repository}/.github/workflows/release.yml@"
    expected_run = f"{expected_repository}/actions/runs/{run_id}/"
    for element in verification:
        if not isinstance(element, dict):
            raise ReleaseError("malformed attestation verification: expected an object")
        result = _attestation_value(element, "verificationResult")
        if not isinstance(result, dict):
            raise ReleaseError("malformed attestation verification: missing verificationResult")
        signature = _attestation_value(result, "signature")
        if not isinstance(signature, dict):
            raise ReleaseError("malformed attestation verification: missing signature")
        certificate = _attestation_value(signature, "certificate")
        if not isinstance(certificate, dict):
            raise ReleaseError("malformed attestation verification: missing certificate")
        statement = _attestation_value(result, "statement")
        if not isinstance(statement, dict):
            raise ReleaseError("malformed attestation verification: missing statement")

        source_repository = _attestation_string(certificate, "sourceRepositoryURI")
        if source_repository != expected_repository:
            raise ReleaseError(
                "attestation names the wrong repository: "
                f"{_attestation_excerpt(source_repository)}"
            )
        signer = _attestation_string(certificate, "buildSignerURI")
        if not signer.startswith(expected_workflow):
            raise ReleaseError(
                "attestation names the wrong signer workflow: "
                f"{_attestation_excerpt(signer)}"
            )
        source_commit = _attestation_string(certificate, "sourceRepositoryDigest")
        if source_commit != source_digest:
            raise ReleaseError(
                "attestation names the wrong source commit: "
                f"{_attestation_excerpt(source_commit)}"
            )
        runner = _attestation_string(certificate, "runnerEnvironment")
        if runner != "github-hosted":
            raise ReleaseError(
                f"attestation was built by an untrusted runner: {_attestation_excerpt(runner)}"
            )
        invocation = _attestation_string(certificate, "runInvocationURI")
        invocation_matches = (
            invocation == f"{expected_run}attempts/{run_attempt}"
            if run_attempt is not None
            else invocation.startswith(expected_run)
        )
        if not invocation_matches:
            raise ReleaseError(
                "attestation is not bound to this workflow run: "
                f"{_attestation_excerpt(invocation)}"
            )

        subjects = _attestation_value(statement, "subject")
        if not isinstance(subjects, list) or not subjects:
            raise ReleaseError("malformed attestation verification: missing subject")
        matching_subject = False
        for subject in subjects:
            if not isinstance(subject, dict):
                raise ReleaseError("malformed attestation verification: expected subject object")
            subject_digest = _attestation_value(subject, "digest")
            if not isinstance(subject_digest, dict):
                raise ReleaseError("malformed attestation verification: missing subject digest")
            digest = _attestation_string(subject_digest, "sha256")
            if digest == archive_sha256:
                matching_subject = True
        if not matching_subject:
            raise ReleaseError("attestation subject digest does not match the archive")
    return len(verification)


def load_attestation_verification(path: Path) -> Any:
    descriptor: int | None = None
    try:
        path_metadata = path.lstat()
        if not stat.S_ISREG(path_metadata.st_mode):
            raise ReleaseError(f"attestation verification is not a regular file: {path}")
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        metadata = os.fstat(descriptor)
    except OSError as error:
        if descriptor is not None:
            os.close(descriptor)
        raise ReleaseError(f"cannot inspect attestation verification {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        raise ReleaseError(f"attestation verification is not a regular file: {path}")
    if metadata.st_size > MAX_ARCHIVE_BYTES:
        os.close(descriptor)
        raise ReleaseError(f"attestation verification {path} exceeds {MAX_ARCHIVE_BYTES} bytes")

    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for name, item in pairs:
            if name in value:
                raise ReleaseError(
                    f"attestation verification {path} has duplicate JSON key {name}"
                )
            value[name] = item
        return value

    try:
        with os.fdopen(descriptor, "rb") as handle:
            descriptor = None
            raw = handle.read(MAX_ARCHIVE_BYTES + 1)
            if len(raw) > MAX_ARCHIVE_BYTES:
                raise ReleaseError(
                    f"attestation verification {path} exceeds {MAX_ARCHIVE_BYTES} bytes"
                )
            return json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise ReleaseError(f"cannot read attestation verification {path}: {error}") from error
    finally:
        if descriptor is not None:
            os.close(descriptor)


def check_publication_identity(
    *,
    manifest_path: Path,
    verification: Any,
    workflow_run: Any,
    tag: str,
    tag_object: str,
    commit: str,
    repository: str,
    server_url: str,
) -> str:
    """Bind a published manifest to its successful release run and current tag."""
    record = load_release_manifest_record(manifest_path, tag)
    oid_pattern = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
    for name, value in (("tag object", tag_object), ("tag commit", commit)):
        if not isinstance(value, str) or oid_pattern.fullmatch(value) is None:
            raise ReleaseError(f"invalid publication {name}")
    if record["tag_object"] != tag_object or record["commit"] != commit:
        raise ReleaseError("published tag identity does not match the signed release manifest")
    if not isinstance(record["workflow_sha"], str) or oid_pattern.fullmatch(record["workflow_sha"]) is None:
        raise ReleaseError("release manifest workflow_sha is invalid")

    run_id = record["run_id"]
    if not isinstance(run_id, str) or re.fullmatch(r"[0-9]{1,20}", run_id) is None:
        raise ReleaseError("release manifest run_id is invalid")
    if not isinstance(workflow_run, dict):
        raise ReleaseError("malformed workflow run metadata")
    run_record_id = workflow_run.get("id")
    if isinstance(run_record_id, bool) or str(run_record_id) != run_id:
        raise ReleaseError("workflow run id does not match the signed release manifest")
    if (
        workflow_run.get("status") != "completed"
        or workflow_run.get("conclusion") != "success"
        or workflow_run.get("path") != ".github/workflows/release.yml"
    ):
        raise ReleaseError("manifest run is not a successful release workflow run")
    head_repository = workflow_run.get("head_repository")
    if not isinstance(head_repository, dict) or head_repository.get("full_name") != repository:
        raise ReleaseError("manifest run belongs to the wrong repository")
    source_digest = workflow_run.get("head_sha")
    if not isinstance(source_digest, str) or oid_pattern.fullmatch(source_digest) is None:
        raise ReleaseError("manifest run has an invalid source commit")
    run_attempt = workflow_run.get("run_attempt")
    check_attestation_verification(
        verification,
        archive_sha256=_sha256(manifest_path),
        repository=repository,
        server_url=server_url,
        source_digest=source_digest,
        run_id=run_id,
        run_attempt=run_attempt,
    )
    return run_id


def _aur_metadata_digests(files_dir: Path) -> dict[str, str]:
    return {
        name: _hash_payload_file(files_dir / name)
        for name in ("PKGBUILD", ".SRCINFO")
    }


def write_aur_metadata_digest_sidecar(
    *,
    output: Path,
    files_dir: Path,
    tag: str,
    repository: str,
    run_id: str,
    run_attempt: int,
    source_sha: str,
) -> dict[str, Any]:
    if RELEASE_TAG_RE.fullmatch(tag) is None:
        raise ReleaseError(f"invalid release tag {tag!r}")
    if not isinstance(repository, str) or re.fullmatch(
        r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository
    ) is None:
        raise ReleaseError("invalid repository for AUR metadata sidecar")
    if not isinstance(run_id, str) or re.fullmatch(r"[0-9]{1,20}", run_id) is None:
        raise ReleaseError("invalid workflow run id for AUR metadata sidecar")
    if isinstance(run_attempt, bool) or not isinstance(run_attempt, int) or run_attempt < 1:
        raise ReleaseError("invalid workflow run attempt for AUR metadata sidecar")
    if not isinstance(source_sha, str) or re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", source_sha) is None:
        raise ReleaseError("invalid workflow source SHA for AUR metadata sidecar")
    value = {
        "schema_version": 1,
        "tag": tag,
        "workflow_run": {
            "repository": repository,
            "id": run_id,
            "attempt": run_attempt,
            "source_sha": source_sha,
        },
        "files": _aur_metadata_digests(files_dir),
    }
    try:
        if output.is_symlink():
            raise ReleaseError(f"refusing to write AUR metadata sidecar through symlink: {output}")
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except OSError as error:
        raise ReleaseError(f"cannot write AUR metadata sidecar {output}: {error}") from error
    return value


def _load_aur_metadata_digest_sidecar(
    path: Path,
    *,
    tag: str,
    repository: str,
    run_id: str,
    run_attempt: int,
    source_sha: str,
) -> dict[str, Any]:
    if path.is_symlink():
        raise ReleaseError(f"AUR metadata digest sidecar must not be a symlink: {path}")
    try:
        if path.stat().st_size > 65536:
            raise ReleaseError(f"AUR metadata digest sidecar {path} exceeds 64 KiB")

        def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
            result: dict[str, Any] = {}
            for name, value in pairs:
                if name in result:
                    raise ReleaseError(f"AUR metadata digest sidecar {path} has duplicate keys")
                result[name] = value
            return result

        value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise ReleaseError(f"cannot read AUR metadata digest sidecar {path}: {error}") from error
    if not isinstance(value, dict) or set(value) != {"schema_version", "tag", "workflow_run", "files"}:
        raise ReleaseError("AUR metadata digest sidecar has an invalid shape")
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise ReleaseError("AUR metadata digest sidecar schema_version must be integer 1")
    if value["tag"] != tag:
        raise ReleaseError("AUR metadata digest sidecar names the wrong release tag")
    workflow_run = value["workflow_run"]
    if not isinstance(workflow_run, dict) or workflow_run != {
        "repository": repository,
        "id": run_id,
        "attempt": run_attempt,
        "source_sha": source_sha,
    }:
        raise ReleaseError("AUR metadata digest sidecar belongs to a different workflow run")
    files = value["files"]
    if not isinstance(files, dict) or set(files) != {"PKGBUILD", ".SRCINFO"}:
        raise ReleaseError("AUR metadata digest sidecar must cover PKGBUILD and .SRCINFO exactly")
    if any(not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None for digest in files.values()):
        raise ReleaseError("AUR metadata digest sidecar contains an invalid SHA-256")
    return value


def verify_aur_metadata_digest_sidecar(
    *,
    files_dir: Path,
    sidecar_path: Path,
    tag: str,
    repository: str,
    run_id: str,
    run_attempt: int,
    source_sha: str,
) -> dict[str, str]:
    sidecar = _load_aur_metadata_digest_sidecar(
        sidecar_path,
        tag=tag,
        repository=repository,
        run_id=run_id,
        run_attempt=run_attempt,
        source_sha=source_sha,
    )
    actual = _aur_metadata_digests(files_dir)
    if actual != sidecar["files"]:
        raise ReleaseError("AUR metadata does not match its workflow digest sidecar")
    return actual


def build_publication_receipt(
    *,
    manifest_path: Path,
    verification: Any,
    workflow_run: Any,
    tag: str,
    tag_object: str,
    commit: str,
    target: str,
    target_repository: str,
    branch: str,
    outcome: str,
    old_oid: str | None,
    new_oid: str,
    observed_oid: str,
    published_blobs: Mapping[str, str],
    aur_metadata_digests: Mapping[str, str] | None,
    repository: str,
    server_url: str,
    publication_run_id: str,
    publication_run_attempt: int,
    publication_source_sha: str,
    aur_metadata_sidecar_sha256: str | None,
    aur_metadata_artifact_id: str | None,
    aur_metadata_producer_attempt: int | None,
    aur_metadata_producer_source_sha: str | None,
) -> dict[str, Any]:
    """Build a credential-free receipt for a verified external publication."""
    release_run_id = check_publication_identity(
        manifest_path=manifest_path,
        verification=verification,
        workflow_run=workflow_run,
        tag=tag,
        tag_object=tag_object,
        commit=commit,
        repository=repository,
        server_url=server_url,
    )
    record = load_release_manifest_record(manifest_path, tag)
    oid_pattern = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
    if target not in {"homebrew", "aur"}:
        raise ReleaseError(f"invalid publication target: {target!r}")
    target_repository_is_valid = (
        isinstance(target_repository, str)
        and len(target_repository) <= 200
        and (
            re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", target_repository) is not None
            if target == "homebrew"
            else target_repository == "thegn-bin"
        )
    )
    if not target_repository_is_valid:
        raise ReleaseError("invalid publication target repository")
    expected_branch = "main" if target == "homebrew" else "master"
    if branch != expected_branch:
        raise ReleaseError(f"invalid {target} publication branch: {branch!r}")
    if outcome not in {"committed", "unchanged"}:
        raise ReleaseError(f"invalid publication outcome: {outcome!r}")
    for name, value in (("new", new_oid), ("observed", observed_oid)):
        if not isinstance(value, str) or oid_pattern.fullmatch(value) is None:
            raise ReleaseError(f"invalid publication {name} OID")
    if old_oid is not None and (
        not isinstance(old_oid, str) or oid_pattern.fullmatch(old_oid) is None
    ):
        raise ReleaseError("invalid publication old OID")
    if observed_oid != new_oid:
        raise ReleaseError("observed publication OID does not match new OID")
    if outcome == "unchanged" and (old_oid is None or new_oid != old_oid):
        raise ReleaseError("unchanged publication must retain its old OID")
    if outcome == "committed" and old_oid == new_oid:
        raise ReleaseError("committed publication must change its OID")
    expected_paths = (
        {"Formula/thegn.rb"}
        if target == "homebrew"
        else {"PKGBUILD", ".SRCINFO"}
    )
    if set(published_blobs) != expected_paths:
        raise ReleaseError("publication receipt blob paths do not match the target contract")
    if any(
        not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None
        for digest in published_blobs.values()
    ):
        raise ReleaseError("publication receipt contains an invalid blob SHA-256")
    signed_asset = f"thegn-{tag}-homebrew.rb" if target == "homebrew" else f"thegn-{tag}-PKGBUILD"
    signed_assets = {asset["name"]: asset["sha256"] for asset in record["assets"]}
    signed_blob_path = "Formula/thegn.rb" if target == "homebrew" else "PKGBUILD"
    if signed_assets.get(signed_asset) != published_blobs[signed_blob_path]:
        raise ReleaseError("published metadata blob does not match the signed release manifest")
    if target == "aur":
        if (
            not isinstance(aur_metadata_digests, Mapping)
            or set(aur_metadata_digests) != {"PKGBUILD", ".SRCINFO"}
            or aur_metadata_digests["PKGBUILD"] != published_blobs["PKGBUILD"]
            or aur_metadata_digests[".SRCINFO"] != published_blobs[".SRCINFO"]
            or not isinstance(aur_metadata_sidecar_sha256, str)
            or SHA256_RE.fullmatch(aur_metadata_sidecar_sha256) is None
            or not isinstance(aur_metadata_artifact_id, str)
            or re.fullmatch(r"[0-9]{1,20}", aur_metadata_artifact_id) is None
            or isinstance(aur_metadata_producer_attempt, bool)
            or not isinstance(aur_metadata_producer_attempt, int)
            or aur_metadata_producer_attempt < 1
            or aur_metadata_producer_attempt > publication_run_attempt
            or not isinstance(aur_metadata_producer_source_sha, str)
            or oid_pattern.fullmatch(aur_metadata_producer_source_sha) is None
            or aur_metadata_producer_source_sha != publication_source_sha
        ):
            raise ReleaseError("published AUR blobs do not match the verified metadata sidecar")
    elif any(
        value is not None
        for value in (
            aur_metadata_digests,
            aur_metadata_sidecar_sha256,
            aur_metadata_artifact_id,
            aur_metadata_producer_attempt,
            aur_metadata_producer_source_sha,
        )
    ):
        raise ReleaseError("Homebrew receipt cannot include AUR metadata digests")
    run_attempt = workflow_run["run_attempt"]
    source_sha = workflow_run["head_sha"]
    if (
        not isinstance(publication_run_id, str)
        or re.fullmatch(r"[0-9]{1,20}", publication_run_id) is None
    ):
        raise ReleaseError("invalid publication workflow run id")
    if (
        isinstance(publication_run_attempt, bool)
        or not isinstance(publication_run_attempt, int)
        or publication_run_attempt < 1
    ):
        raise ReleaseError("invalid publication workflow run attempt")
    if not isinstance(publication_source_sha, str) or oid_pattern.fullmatch(publication_source_sha) is None:
        raise ReleaseError("invalid publication workflow source SHA")
    return {
        "schema_version": 1,
        "target": target,
        "target_repository": target_repository,
        "branch": branch,
        "outcome": outcome,
        "old_oid": old_oid,
        "new_oid": new_oid,
        "observed_oid": observed_oid,
        "published_blobs": [
            {"path": path, "sha256": published_blobs[path]}
            for path in sorted(published_blobs)
        ],
        "metadata_sidecar_sha256": aur_metadata_sidecar_sha256,
        "metadata_artifact_id": aur_metadata_artifact_id,
        "metadata_producer": (
            {
                "run_id": publication_run_id,
                "attempt": aur_metadata_producer_attempt,
                "source_sha": aur_metadata_producer_source_sha,
            }
            if target == "aur"
            else None
        ),
        "release": {
            "tag": tag,
            "tag_object": record["tag_object"],
            "source_commit": record["commit"],
            "workflow_sha": record["workflow_sha"],
            "run_id": release_run_id,
            "run_attempt": run_attempt,
            "run_source_sha": source_sha,
            "repository": repository,
            "manifest_sha256": _sha256(manifest_path),
        },
        "publication_run": {
            "id": publication_run_id,
            "attempt": publication_run_attempt,
            "source_sha": publication_source_sha,
        },
    }


def write_publication_receipt(output: Path, value: Mapping[str, Any]) -> None:
    try:
        if output.is_symlink():
            raise ReleaseError(f"refusing to write publication receipt through symlink: {output}")
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except OSError as error:
        raise ReleaseError(f"cannot write publication receipt {output}: {error}") from error


def _read_publication_saga_receipt(path: Path | None) -> tuple[dict[str, Any] | None, str | None]:
    if path is None:
        return None, None
    descriptor: int | None = None

    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for name, item in pairs:
            if name in value:
                raise ValueError(f"duplicate JSON key {name}")
            value[name] = item
        return value

    try:
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > MAX_TRUSTED_DIGESTS_BYTES:
            return None, None
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(descriptor, "rb") as handle:
            descriptor = None
            raw = handle.read(MAX_TRUSTED_DIGESTS_BYTES + 1)
        if len(raw) > MAX_TRUSTED_DIGESTS_BYTES:
            return None, None
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object)
        if not isinstance(value, dict):
            return None, hashlib.sha256(raw).hexdigest()
        return value, hashlib.sha256(raw).hexdigest()
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError, ValueError):
        return None, None
    finally:
        if descriptor is not None:
            os.close(descriptor)


def _publication_saga_target(
    *,
    target: str,
    job_result: str,
    ready: str,
    outcome_hint: str,
    receipt_artifact_id: str,
    receipt: dict[str, Any] | None,
    receipt_sha256: str | None,
    tag: str,
    repository: str,
    publication_run_id: str,
    publication_run_attempt: int,
    publication_source_sha: str,
) -> dict[str, Any]:
    result: dict[str, Any] = {
        "job_result": job_result,
        "ready": ready,
        "outcome_hint": outcome_hint or None,
        "receipt_artifact_id": receipt_artifact_id or None,
        "receipt_sha256": receipt_sha256,
    }
    if job_result not in {"success", "failure", "cancelled", "skipped"}:
        result.update(state="unknown", reason="invalid publisher job result")
        return result
    if receipt is None:
        if ready == "false" and not receipt_artifact_id:
            result.update(state="pending", reason="publisher was not configured; push steps were skipped")
        elif job_result == "skipped" and not receipt_artifact_id:
            result.update(state="skipped", reason="publisher job was skipped; no push step ran")
        else:
            result.update(
                state="unknown",
                reason="receipt is missing; remote target state must be reconciled",
            )
        return result

    target_repository = receipt.get("target_repository")
    branch = "main" if target == "homebrew" else "master"
    release = receipt.get("release")
    publication = receipt.get("publication_run")
    new_oid = receipt.get("new_oid")
    old_oid = receipt.get("old_oid")
    observed_oid = receipt.get("observed_oid")
    outcome = receipt.get("outcome")
    oid_pattern = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
    publication_attempt = publication.get("attempt") if isinstance(publication, dict) else None
    expected_blob_paths = (
        {"Formula/thegn.rb"} if target == "homebrew" else {"PKGBUILD", ".SRCINFO"}
    )
    published_blobs = receipt.get("published_blobs")
    blob_map: dict[str, str] = {}
    if isinstance(published_blobs, list):
        for item in published_blobs:
            if not isinstance(item, dict):
                continue
            path = item.get("path")
            digest = item.get("sha256")
            if isinstance(path, str) and isinstance(digest, str) and path not in blob_map:
                blob_map[path] = digest
    target_repository_valid = (
        re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", target_repository) is not None
        if target == "homebrew" and isinstance(target_repository, str)
        else target_repository == "thegn-bin"
    )
    aur_metadata = receipt.get("metadata_producer")
    aur_metadata_valid = (
        isinstance(receipt.get("metadata_sidecar_sha256"), str)
        and SHA256_RE.fullmatch(receipt["metadata_sidecar_sha256"]) is not None
        and isinstance(receipt.get("metadata_artifact_id"), str)
        and re.fullmatch(r"[0-9]{1,20}", receipt["metadata_artifact_id"]) is not None
        and isinstance(aur_metadata, dict)
        and aur_metadata.get("run_id") == publication_run_id
        and aur_metadata.get("source_sha") == publication_source_sha
        and isinstance(aur_metadata.get("attempt"), int)
        and not isinstance(aur_metadata.get("attempt"), bool)
        and 1 <= aur_metadata["attempt"] <= publication_run_attempt
    )
    valid = (
        receipt.get("schema_version") == 1
        and receipt.get("target") == target
        and isinstance(target_repository, str)
        and bool(target_repository)
        and target_repository_valid
        and receipt.get("branch") == branch
        and outcome in {"committed", "unchanged"}
        and isinstance(new_oid, str)
        and oid_pattern.fullmatch(new_oid) is not None
        and isinstance(observed_oid, str)
        and observed_oid == new_oid
        and (old_oid is None or (isinstance(old_oid, str) and oid_pattern.fullmatch(old_oid) is not None))
        and (
            (outcome == "unchanged" and old_oid is not None and old_oid == new_oid)
            or (outcome == "committed" and old_oid != new_oid)
        )
        and isinstance(release, dict)
        and release.get("tag") == tag
        and isinstance(release.get("tag_object"), str)
        and oid_pattern.fullmatch(release["tag_object"]) is not None
        and isinstance(release.get("source_commit"), str)
        and oid_pattern.fullmatch(release["source_commit"]) is not None
        and isinstance(release.get("workflow_sha"), str)
        and oid_pattern.fullmatch(release["workflow_sha"]) is not None
        and isinstance(release.get("run_source_sha"), str)
        and oid_pattern.fullmatch(release["run_source_sha"]) is not None
        and release["run_source_sha"] == release["source_commit"]
        and release.get("repository") == repository
        and isinstance(release.get("manifest_sha256"), str)
        and SHA256_RE.fullmatch(release["manifest_sha256"]) is not None
        and isinstance(release.get("run_id"), str)
        and re.fullmatch(r"[0-9]{1,20}", release["run_id"]) is not None
        and isinstance(release.get("run_attempt"), int)
        and not isinstance(release["run_attempt"], bool)
        and release["run_attempt"] > 0
        and isinstance(publication, dict)
        and publication.get("id") == publication_run_id
        and isinstance(publication_attempt, int)
        and not isinstance(publication_attempt, bool)
        and 1 <= publication_attempt <= publication_run_attempt
        and publication.get("source_sha") == publication_source_sha
        and re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", publication_source_sha) is not None
        and isinstance(receipt_artifact_id, str)
        and re.fullmatch(r"[0-9]{1,20}", receipt_artifact_id) is not None
        and isinstance(receipt_sha256, str)
        and SHA256_RE.fullmatch(receipt_sha256) is not None
        and set(blob_map) == expected_blob_paths
        and all(SHA256_RE.fullmatch(digest) is not None for digest in blob_map.values())
        and (aur_metadata_valid if target == "aur" else all(
            receipt.get(field) is None
            for field in ("metadata_sidecar_sha256", "metadata_artifact_id", "metadata_producer")
        ))
    )
    if not valid:
        result.update(state="unknown", reason="receipt does not match this target and workflow run")
        return result

    result.update(
        state="verified",
        outcome=receipt["outcome"],
        target_repository=target_repository,
        branch=branch,
        old_oid=old_oid,
        new_oid=new_oid,
        observed_oid=observed_oid,
        receipt_publication_attempt=publication_attempt,
        release=release,
    )
    return result


def build_publication_saga_ledger(
    *,
    tag: str,
    repository: str,
    publication_run_id: str,
    publication_run_attempt: int,
    publication_source_sha: str,
    homebrew_job_result: str,
    homebrew_ready: str,
    homebrew_outcome_hint: str,
    homebrew_receipt_artifact_id: str,
    homebrew_receipt: dict[str, Any] | None,
    homebrew_receipt_sha256: str | None,
    aur_job_result: str,
    aur_ready: str,
    aur_outcome_hint: str,
    aur_receipt_artifact_id: str,
    aur_receipt: dict[str, Any] | None,
    aur_receipt_sha256: str | None,
) -> dict[str, Any]:
    """Summarize per-target receipts without inferring state from a missing receipt."""
    if RELEASE_TAG_RE.fullmatch(tag) is None:
        raise ReleaseError(f"invalid release tag {tag!r}")
    if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is None:
        raise ReleaseError("invalid publication saga repository")
    if re.fullmatch(r"[0-9]{1,20}", publication_run_id) is None:
        raise ReleaseError("invalid publication saga run id")
    if (
        isinstance(publication_run_attempt, bool)
        or not isinstance(publication_run_attempt, int)
        or publication_run_attempt < 1
    ):
        raise ReleaseError("invalid publication saga run attempt")
    if not isinstance(publication_source_sha, str) or re.fullmatch(
        r"[0-9a-f]{40}|[0-9a-f]{64}", publication_source_sha
    ) is None:
        raise ReleaseError("invalid publication saga source SHA")
    target_inputs = {
        "homebrew": (
            homebrew_job_result,
            homebrew_ready,
            homebrew_outcome_hint,
            homebrew_receipt_artifact_id,
            homebrew_receipt,
            homebrew_receipt_sha256,
        ),
        "aur": (
            aur_job_result,
            aur_ready,
            aur_outcome_hint,
            aur_receipt_artifact_id,
            aur_receipt,
            aur_receipt_sha256,
        ),
    }
    targets: dict[str, dict[str, Any]] = {}
    for target, values in target_inputs.items():
        job_result, ready, outcome_hint, artifact_id, receipt, receipt_sha256 = values
        targets[target] = _publication_saga_target(
            target=target,
            job_result=job_result,
            ready=ready,
            outcome_hint=outcome_hint,
            receipt_artifact_id=artifact_id,
            receipt=receipt,
            receipt_sha256=receipt_sha256,
            tag=tag,
            repository=repository,
            publication_run_id=publication_run_id,
            publication_run_attempt=publication_run_attempt,
            publication_source_sha=publication_source_sha,
        )

    homebrew = targets["homebrew"]
    aur = targets["aur"]
    if homebrew["state"] == "verified" and aur["state"] == "verified":
        homebrew_identity = homebrew["release"]
        aur_identity = aur["release"]
        identity_fields = (
            "tag",
            "tag_object",
            "source_commit",
            "workflow_sha",
            "run_id",
            "run_attempt",
            "run_source_sha",
            "repository",
            "manifest_sha256",
        )
        if all(homebrew_identity.get(field) == aur_identity.get(field) for field in identity_fields):
            state = "complete"
        else:
            state = "inconsistent"
    elif homebrew["state"] in {"pending", "skipped"} and aur["state"] in {"pending", "skipped"}:
        state = "pending"
    elif homebrew["state"] == "verified" or aur["state"] == "verified":
        state = "partial"
    else:
        state = "unknown"

    ledger = {
        "schema_version": 1,
        "state": state,
        "reconcile_required": state in {"partial", "unknown", "inconsistent"},
        "tag": tag,
        "repository": repository,
        "publication_run": {
            "id": publication_run_id,
            "attempt": publication_run_attempt,
            "source_sha": publication_source_sha,
        },
        "targets": targets,
        "retention_days": 90,
    }
    manifest_digests = {
        target["release"]["manifest_sha256"]
        for target in targets.values()
        if target["state"] == "verified"
    }
    if len(manifest_digests) == 1:
        ledger["release_manifest_sha256"] = next(iter(manifest_digests))
    return ledger


def publication_saga_summary(ledger: Mapping[str, Any]) -> str:
    state = ledger["state"]
    lines = [
        f"## External publication saga: {ledger['tag']}",
        "",
        f"State: **{state}**",
    ]
    for target_name in ("homebrew", "aur"):
        target = ledger["targets"][target_name]
        if target["state"] == "verified":
            lines.append(
                f"- {target_name}: {target['outcome']} at {target['observed_oid']} "
                f"(receipt publication attempt {target['receipt_publication_attempt']})"
            )
        else:
            hint = f"; publisher outcome hint: {target['outcome_hint']}" if target["outcome_hint"] else ""
            lines.append(
                f"- {target_name}: {target['state']} ({target['job_result']}, ready={target['ready']}) "
                f"— {target['reason']}{hint}"
            )
    if ledger["reconcile_required"]:
        lines.extend(
            [
                "",
                "A missing receipt is not evidence that a push did not happen. Inspect both remote tips and "
                "reconcile explicitly; this workflow does not infer unchanged or retry automatically.",
            ]
        )
    elif state == "pending":
        lines.extend(["", "No target has a verified publication; pending/skipped publisher jobs did not run their push steps."])
    else:
        lines.extend(["", f"Ledger artifact retention: {ledger['retention_days']} days."])
    return "\n".join(lines) + "\n"


def write_publication_saga_ledger(
    *,
    output: Path,
    summary_path: Path,
    ledger: Mapping[str, Any],
) -> None:
    try:
        if output.is_symlink() or summary_path.is_symlink():
            raise ReleaseError("refusing to write publication saga through a symlink")
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(ledger, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        with summary_path.open("a", encoding="utf-8") as summary:
            summary.write(publication_saga_summary(ledger))
    except OSError as error:
        raise ReleaseError(f"cannot write publication saga ledger: {error}") from error


def check_release_asset_sizes(document: Any, patterns: Sequence[str]) -> list[str]:
    if not isinstance(document, dict) or not isinstance(document.get("assets"), list):
        raise ReleaseError("malformed release asset listing: expected an assets list")
    sizes: dict[str, int] = {}
    for asset in document["assets"]:
        if not isinstance(asset, dict):
            raise ReleaseError("malformed release asset listing: asset is not an object")
        name = asset.get("name")
        size = asset.get("size")
        if not isinstance(name, str) or not name:
            raise ReleaseError("malformed release asset listing: asset name is missing")
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise ReleaseError(f"malformed release asset listing: bad size for {name}")
        if name in sizes:
            raise ReleaseError(f"malformed release asset listing: duplicate asset {name}")
        sizes[name] = size
    selected = sorted(
        name
        for name in sizes
        if not patterns or any(fnmatch.fnmatchcase(name, pattern) for pattern in patterns)
    )
    if not selected:
        raise ReleaseError("no release assets match the download patterns")
    total = 0
    for name in selected:
        limit = (
            MAX_TRUSTED_DIGESTS_BYTES
            if name.endswith(TEXT_ASSET_SUFFIXES)
            else MAX_ARCHIVE_BYTES
        )
        if sizes[name] > limit:
            raise ReleaseError(f"release asset {name} exceeds the {limit} byte download limit")
        size = sizes[name]
        if size > MAX_SELECTED_DOWNLOAD_BYTES - total:
            raise ReleaseError(
                f"selected release assets exceed the {MAX_SELECTED_DOWNLOAD_BYTES} byte aggregate download limit"
            )
        total += size
    return selected


def load_release_asset_listing(path: Path) -> Any:
    descriptor: int | None = None
    try:
        path_metadata = path.lstat()
        if not stat.S_ISREG(path_metadata.st_mode):
            raise ReleaseError(f"release asset listing is not a regular file: {path}")
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        metadata = os.fstat(descriptor)
    except OSError as error:
        if descriptor is not None:
            os.close(descriptor)
        raise ReleaseError(f"cannot inspect release asset listing {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        raise ReleaseError(f"release asset listing is not a regular file: {path}")
    if metadata.st_size > MAX_ARCHIVE_BYTES:
        os.close(descriptor)
        raise ReleaseError(f"release asset listing {path} exceeds {MAX_ARCHIVE_BYTES} bytes")

    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for name, item in pairs:
            if name in value:
                raise ReleaseError(f"release asset listing {path} has duplicate JSON key {name}")
            value[name] = item
        return value

    try:
        with os.fdopen(descriptor, "rb") as handle:
            descriptor = None
            raw = handle.read(MAX_ARCHIVE_BYTES + 1)
            if len(raw) > MAX_ARCHIVE_BYTES:
                raise ReleaseError(f"release asset listing {path} exceeds {MAX_ARCHIVE_BYTES} bytes")
            return json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise ReleaseError(f"cannot read release asset listing {path}: {error}") from error
    finally:
        if descriptor is not None:
            os.close(descriptor)


def plan_release_asset_uploads(document: Any, assets: Sequence[Path]) -> list[Path]:
    """Select absent draft assets; accept existing names only at the same SHA-256."""
    requested_names = {path.name for path in assets}
    pages: list[Any]
    if isinstance(document, list) and document and all(isinstance(page, list) for page in document):
        pages = document
    elif isinstance(document, list):
        pages = [document]
    else:
        raise ReleaseError("GitHub release asset inventory must be a JSON array")

    existing: dict[str, str | None] = {}
    for page in pages:
        for entry in page:
            item = _object(entry, "GitHub release asset")
            name = item.get("name")
            if not isinstance(name, str) or TRUSTED_NAME_RE.fullmatch(name) is None:
                raise ReleaseError("GitHub release asset inventory has an invalid name")
            if name in existing:
                raise ReleaseError(f"GitHub release asset inventory repeats {name}")
            digest = item.get("digest")
            if name in requested_names and digest is not None and (
                not isinstance(digest, str)
                or not digest.startswith("sha256:")
                or SHA256_RE.fullmatch(digest.removeprefix("sha256:")) is None
            ):
                raise ReleaseError(f"GitHub release asset {name} has unusable digest metadata")
            existing[name] = digest

    seen: set[str] = set()
    upload: list[Path] = []
    for path in assets:
        name = path.name
        if name in seen:
            raise ReleaseError(f"local release upload repeats asset {name}")
        seen.add(name)
        if name in (".", "..") or TRUSTED_NAME_RE.fullmatch(name) is None:
            raise ReleaseError(f"local release asset name is invalid: {name!r}")
        try:
            metadata = path.lstat()
        except OSError as error:
            raise ReleaseError(f"cannot inspect local release asset {path}: {error}") from error
        if not stat.S_ISREG(metadata.st_mode):
            raise ReleaseError(f"local release asset is not a regular file: {path}")
        remote_digest = existing.get(name)
        if name not in existing:
            upload.append(path)
            continue
        if remote_digest is None:
            raise ReleaseError(f"existing release asset {name} has no trusted SHA-256 metadata")
        local_digest = _sha256(path)
        if remote_digest != f"sha256:{local_digest}":
            raise ReleaseError(f"existing release asset {name} conflicts with this generation")
    return upload


def _run(args: argparse.Namespace) -> None:
    if args.command == "require-draft":
        require_draft(args.is_draft)
        return
    if args.command == "verify-generation":
        verify_release_generation(
            args.record,
            tag=args.tag,
            commit=args.commit,
            tag_object=args.tag_object,
            workflow_sha=args.workflow_sha,
            run_id=args.run_id,
        )
        print(f"{args.tag}: draft is bound to workflow run {args.run_id}")
        return
    if args.command == "verify-assets":
        if RELEASE_TAG_RE.fullmatch(args.tag) is None:
            raise ReleaseError(f"invalid release tag {args.tag!r}")
        entries = load_release_manifest(args.release_manifest, args.tag)
        verified = verify_release_asset_files(entries, args.asset, exact=args.exact)
        for name in verified:
            print(f"{name}: matches the release manifest")
        return
    if args.command == "verify-attestation":
        count = check_attestation_verification(
            load_attestation_verification(args.verification),
            archive_sha256=_sha256(args.archive),
            repository=args.repository,
            server_url=args.server_url,
            source_digest=args.source_digest,
            run_id=args.run_id,
        )
        print(f"{args.archive.name}: {count} attestation(s) bound to this run")
        return
    if args.command == "verify-publication-identity":
        try:
            workflow_run = json.loads(args.workflow_run.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
            raise ReleaseError(f"cannot read workflow run metadata {args.workflow_run}: {error}") from error
        run_id = check_publication_identity(
            manifest_path=args.release_manifest,
            verification=load_attestation_verification(args.verification),
            workflow_run=workflow_run,
            tag=args.tag,
            tag_object=args.tag_object,
            commit=args.commit,
            repository=args.repository,
            server_url=args.server_url,
        )
        print(f"{args.tag}: signed release manifest is bound to run {run_id} and current tag")
        return
    if args.command == "write-aur-metadata-digests":
        write_aur_metadata_digest_sidecar(
            output=args.output,
            files_dir=args.files_dir,
            tag=args.tag,
            repository=args.repository,
            run_id=args.run_id,
            run_attempt=args.run_attempt,
            source_sha=args.source_sha,
        )
        print(f"{args.tag}: wrote AUR metadata digest sidecar")
        return
    if args.command == "verify-aur-metadata-digests":
        verified = verify_aur_metadata_digest_sidecar(
            files_dir=args.files_dir,
            sidecar_path=args.sidecar,
            tag=args.tag,
            repository=args.repository,
            run_id=args.run_id,
            run_attempt=args.run_attempt,
            source_sha=args.source_sha,
        )
        for name in sorted(verified):
            print(f"{name}: {verified[name]}")
        return
    if args.command == "write-publication-receipt":
        try:
            workflow_run = json.loads(args.workflow_run.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
            raise ReleaseError(f"cannot read workflow run metadata {args.workflow_run}: {error}") from error
        try:
            tag_identity = json.loads(args.tag_identity.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
            raise ReleaseError(f"cannot read live tag identity {args.tag_identity}: {error}") from error
        if not isinstance(tag_identity, dict):
            raise ReleaseError("malformed live tag identity")
        aur_metadata_digests: Mapping[str, str] | None = None
        aur_metadata_sidecar_sha256: str | None = None
        if args.metadata_sidecar is not None:
            sidecar_record = _load_aur_metadata_digest_sidecar(
                args.metadata_sidecar,
                tag=args.tag,
                repository=args.repository,
                run_id=args.publication_run_id,
                run_attempt=args.metadata_producer_attempt,
                source_sha=args.metadata_producer_source_sha,
            )
            aur_metadata_digests = sidecar_record["files"]
            aur_metadata_sidecar_sha256 = _hash_payload_file(args.metadata_sidecar)
        published_blobs: dict[str, str] = {}
        for item in args.published_blob:
            if "=" not in item:
                raise ReleaseError("published blob must use PATH=SHA256 syntax")
            path, digest = item.split("=", 1)
            if path in published_blobs:
                raise ReleaseError(f"duplicate published blob path {path!r}")
            published_blobs[path] = digest
        receipt = build_publication_receipt(
            manifest_path=args.release_manifest,
            verification=load_attestation_verification(args.verification),
            workflow_run=workflow_run,
            tag=args.tag,
            tag_object=tag_identity.get("tag_object"),
            commit=tag_identity.get("commit"),
            target=args.target,
            target_repository=args.target_repository,
            branch=args.branch,
            outcome=args.outcome,
            old_oid=args.old_oid or None,
            new_oid=args.new_oid,
            observed_oid=args.observed_oid,
            repository=args.repository,
            server_url=args.server_url,
            publication_run_id=args.publication_run_id,
            publication_run_attempt=args.publication_run_attempt,
            publication_source_sha=args.publication_source_sha,
            aur_metadata_sidecar_sha256=aur_metadata_sidecar_sha256,
            aur_metadata_artifact_id=args.metadata_artifact_id,
            aur_metadata_producer_attempt=args.metadata_producer_attempt,
            aur_metadata_producer_source_sha=args.metadata_producer_source_sha,
            published_blobs=published_blobs,
            aur_metadata_digests=aur_metadata_digests,
        )
        write_publication_receipt(args.output, receipt)
        print(f"{args.target}: wrote publication receipt for {args.tag}")
        return
    if args.command == "publication-saga":
        homebrew_receipt, homebrew_sha256 = _read_publication_saga_receipt(
            args.homebrew_receipt
        )
        aur_receipt, aur_sha256 = _read_publication_saga_receipt(args.aur_receipt)
        ledger = build_publication_saga_ledger(
            tag=args.tag,
            repository=args.repository,
            publication_run_id=args.publication_run_id,
            publication_run_attempt=args.publication_run_attempt,
            publication_source_sha=args.publication_source_sha,
            homebrew_job_result=args.homebrew_result,
            homebrew_ready=args.homebrew_ready,
            homebrew_outcome_hint=args.homebrew_outcome,
            homebrew_receipt_artifact_id=args.homebrew_artifact_id,
            homebrew_receipt=homebrew_receipt,
            homebrew_receipt_sha256=homebrew_sha256,
            aur_job_result=args.aur_result,
            aur_ready=args.aur_ready,
            aur_outcome_hint=args.aur_outcome,
            aur_receipt_artifact_id=args.aur_artifact_id,
            aur_receipt=aur_receipt,
            aur_receipt_sha256=aur_sha256,
        )
        write_publication_saga_ledger(
            output=args.output,
            summary_path=args.summary,
            ledger=ledger,
        )
        print(f"{args.tag}: publication saga state {ledger['state']}")
        return
    if args.command == "check-asset-sizes":
        selected = check_release_asset_sizes(
            load_release_asset_listing(args.assets_json), args.pattern or []
        )
        print(f"{len(selected)} release asset(s) within download limits")
        return
    if args.command == "plan-upload":
        plan = plan_release_asset_uploads(
            load_release_asset_listing(args.assets_json), args.asset
        )
        print(json.dumps([str(path) for path in plan]))
        return
    manifest_path = args.manifest.resolve()
    root = args.repo_root.resolve()
    manifest = load_manifest(manifest_path)
    version = workspace_version(root)
    require_matching_tag(args.tag, version)
    if args.command == "verify-digests":
        trusted = load_trusted_digests(args.trusted_digests)
        names = verify_downloaded_assets(manifest, args.tag, args.assets_dir, trusted)
        for name in names:
            print(f"{name}: {_sha256(args.assets_dir / name)}")
        return
    if args.command == "stage":
        digest_path = args.digests_out
        if os.path.lexists(digest_path):
            raise ReleaseError(f"staged digest output already exists: {digest_path}")
        output_absolute = Path(os.path.abspath(args.output_dir))
        digest_absolute = Path(os.path.abspath(digest_path))
        try:
            inside = os.path.commonpath((output_absolute, digest_absolute)) == str(output_absolute)
        except ValueError:
            inside = False
        if inside:
            raise ReleaseError(f"staged digest output must be outside the staging directory: {digest_path}")
        trusted = load_trusted_digests(args.trusted_digests)
        staged = stage_archive(
            manifest, args.tag, args.assets_dir, args.target, args.output_dir, trusted
        )
        digest_created = False
        try:
            descriptor = os.open(
                digest_path,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                0o600,
            )
            digest_created = True
            with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
                json.dump(staged, handle, sort_keys=True, indent=2)
                handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
        except (OSError, ValueError) as error:
            shutil.rmtree(args.output_dir, ignore_errors=True)
            cleanup_error: OSError | None = None
            if digest_created:
                try:
                    digest_path.unlink()
                except OSError as cleanup:
                    cleanup_error = cleanup
            message = f"cannot write staged digests {digest_path}: {error}"
            if cleanup_error is not None:
                message += f"; cleanup failed: {cleanup_error}"
            raise ReleaseError(message) from error
        for name in sorted(staged):
            print(f"{name}: {staged[name]}")
        return
    if args.command == "verify-payload":
        staged = load_staged_digests(args.staged_digests)
        verify_package_payload(staged, args.payload_root, args.label)
        print(f"{args.label} payload matches staged bytes")
        return
    if args.command == "manifest":
        value = build_release_manifest(
            manifest,
            args.tag,
            args.commit,
            args.assets_dir,
            args.record,
            trusted=(
                load_trusted_digests(args.trusted_digests)
                if args.trusted_digests is not None
                else None
            ),
        )
        write_release_manifest(args.output, args.assets_dir, value)
        print(args.output.as_posix())
        return
    managers = selected_managers(manifest, args.manager)
    assets = validate_assets(manifest, args.tag, args.assets_dir.resolve(), managers)
    if args.command == "render":
        files = rendered_files(manifest, manifest_path.parent, version, managers, assets)
        atomic_write_output(args.output_dir, files)
        for output in sorted(files, key=str):
            print(output.as_posix())
    else:
        for target in assets:
            print(f"{target}: {assets[target].sha256}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    draft_parser = subparsers.add_parser(
        "require-draft", help="refuse release asset uploads unless the release is a draft"
    )
    draft_parser.add_argument("--is-draft", required=True)
    generation_parser = subparsers.add_parser(
        "verify-generation", help="bind a draft to the current admitted workflow run"
    )
    generation_parser.add_argument("--record", required=True, type=Path)
    generation_parser.add_argument("--tag", required=True)
    generation_parser.add_argument("--commit", required=True)
    generation_parser.add_argument("--tag-object", required=True)
    generation_parser.add_argument("--workflow-sha", required=True)
    generation_parser.add_argument("--run-id", required=True)
    for command in ("validate", "render"):
        subparser = subparsers.add_parser(command)
        subparser.add_argument("--tag", required=True, help="release tag, exactly v<workspace version>")
        subparser.add_argument("--assets-dir", required=True, type=Path)
        subparser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
        subparser.add_argument("--repo-root", type=Path, default=REPOSITORY_ROOT)
        subparser.add_argument(
            "--manager",
            action="append",
            help="manager to validate/render; repeat as needed (default: every active manager)",
        )
        if command == "render":
            subparser.add_argument("--output-dir", required=True, type=Path)
    manifest_parser = subparsers.add_parser("manifest")
    manifest_parser.add_argument("--tag", required=True, help="release tag, exactly v<workspace version>")
    manifest_parser.add_argument("--assets-dir", required=True, type=Path)
    manifest_parser.add_argument("--record", required=True, type=Path)
    manifest_parser.add_argument("--commit", required=True)
    manifest_parser.add_argument("--output", required=True, type=Path)
    manifest_parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    manifest_parser.add_argument("--repo-root", type=Path, default=REPOSITORY_ROOT)
    manifest_parser.add_argument("--trusted-digests", type=Path, action="append")
    verify_parser = subparsers.add_parser("verify-digests")
    verify_parser.add_argument("--tag", required=True, help="release tag, exactly v<workspace version>")
    verify_parser.add_argument("--assets-dir", required=True, type=Path)
    verify_parser.add_argument("--trusted-digests", required=True, type=Path, action="append")
    verify_parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    verify_parser.add_argument("--repo-root", type=Path, default=REPOSITORY_ROOT)
    stage_parser = subparsers.add_parser("stage")
    stage_parser.add_argument("--tag", required=True, help="release tag, exactly v<workspace version>")
    stage_parser.add_argument("--assets-dir", required=True, type=Path)
    stage_parser.add_argument("--target", required=True)
    stage_parser.add_argument("--trusted-digests", required=True, type=Path, action="append")
    stage_parser.add_argument("--output-dir", required=True, type=Path)
    stage_parser.add_argument("--digests-out", required=True, type=Path)
    stage_parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    stage_parser.add_argument("--repo-root", type=Path, default=REPOSITORY_ROOT)
    payload_parser = subparsers.add_parser("verify-payload")
    payload_parser.add_argument("--tag", required=True, help="release tag, exactly v<workspace version>")
    payload_parser.add_argument("--staged-digests", required=True, type=Path)
    payload_parser.add_argument("--payload-root", required=True, type=Path)
    payload_parser.add_argument("--label", required=True, choices=("deb", "rpm"))
    payload_parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    payload_parser.add_argument("--repo-root", type=Path, default=REPOSITORY_ROOT)
    verify_assets_parser = subparsers.add_parser("verify-assets")
    verify_assets_parser.add_argument(
        "--tag",
        required=True,
        help="release tag (v<major>.<minor>.<patch>[-pre]); must match the release manifest's tag",
    )
    verify_assets_parser.add_argument("--release-manifest", required=True, type=Path)
    verify_assets_parser.add_argument("--asset", required=True, type=Path, action="append")
    verify_assets_parser.add_argument("--exact", action="store_true")
    verify_assets_parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    verify_assets_parser.add_argument("--repo-root", type=Path, default=REPOSITORY_ROOT)
    verify_attestation_parser = subparsers.add_parser("verify-attestation")
    verify_attestation_parser.add_argument("--archive", required=True, type=Path)
    verify_attestation_parser.add_argument("--verification", required=True, type=Path)
    verify_attestation_parser.add_argument("--repository", required=True)
    verify_attestation_parser.add_argument("--server-url", required=True)
    verify_attestation_parser.add_argument("--source-digest", required=True)
    verify_attestation_parser.add_argument("--run-id", required=True)
    publication_identity_parser = subparsers.add_parser("verify-publication-identity")
    publication_identity_parser.add_argument("--release-manifest", required=True, type=Path)
    publication_identity_parser.add_argument("--verification", required=True, type=Path)
    publication_identity_parser.add_argument("--workflow-run", required=True, type=Path)
    publication_identity_parser.add_argument("--tag", required=True)
    publication_identity_parser.add_argument("--tag-object", required=True)
    publication_identity_parser.add_argument("--commit", required=True)
    publication_identity_parser.add_argument("--repository", required=True)
    publication_identity_parser.add_argument("--server-url", required=True)
    aur_metadata_write_parser = subparsers.add_parser(
        "write-aur-metadata-digests", help="write a run-bound sidecar for generated AUR metadata"
    )
    aur_metadata_write_parser.add_argument("--files-dir", required=True, type=Path)
    aur_metadata_write_parser.add_argument("--output", required=True, type=Path)
    aur_metadata_write_parser.add_argument("--tag", required=True)
    aur_metadata_write_parser.add_argument("--repository", required=True)
    aur_metadata_write_parser.add_argument("--run-id", required=True)
    aur_metadata_write_parser.add_argument("--run-attempt", required=True, type=int)
    aur_metadata_write_parser.add_argument("--source-sha", required=True)
    aur_metadata_verify_parser = subparsers.add_parser(
        "verify-aur-metadata-digests", help="verify generated AUR metadata against its run-bound sidecar"
    )
    aur_metadata_verify_parser.add_argument("--files-dir", required=True, type=Path)
    aur_metadata_verify_parser.add_argument("--sidecar", required=True, type=Path)
    aur_metadata_verify_parser.add_argument("--tag", required=True)
    aur_metadata_verify_parser.add_argument("--repository", required=True)
    aur_metadata_verify_parser.add_argument("--run-id", required=True)
    aur_metadata_verify_parser.add_argument("--run-attempt", required=True, type=int)
    aur_metadata_verify_parser.add_argument("--source-sha", required=True)
    publication_receipt_parser = subparsers.add_parser(
        "write-publication-receipt", help="write a machine-readable external publication receipt"
    )
    publication_receipt_parser.add_argument("--release-manifest", required=True, type=Path)
    publication_receipt_parser.add_argument("--verification", required=True, type=Path)
    publication_receipt_parser.add_argument("--workflow-run", required=True, type=Path)
    publication_receipt_parser.add_argument("--tag-identity", required=True, type=Path)
    publication_receipt_parser.add_argument("--tag", required=True)
    publication_receipt_parser.add_argument("--target", required=True, choices=("homebrew", "aur"))
    publication_receipt_parser.add_argument("--target-repository", required=True)
    publication_receipt_parser.add_argument("--branch", required=True)
    publication_receipt_parser.add_argument(
        "--outcome", required=True, choices=("committed", "unchanged")
    )
    publication_receipt_parser.add_argument("--old-oid", default="")
    publication_receipt_parser.add_argument("--new-oid", required=True)
    publication_receipt_parser.add_argument("--observed-oid", required=True)
    publication_receipt_parser.add_argument("--repository", required=True)
    publication_receipt_parser.add_argument("--server-url", required=True)
    publication_receipt_parser.add_argument("--publication-run-id", required=True)
    publication_receipt_parser.add_argument("--publication-run-attempt", required=True, type=int)
    publication_receipt_parser.add_argument("--publication-source-sha", required=True)
    publication_receipt_parser.add_argument("--metadata-sidecar", type=Path)
    publication_receipt_parser.add_argument("--metadata-artifact-id")
    publication_receipt_parser.add_argument("--metadata-producer-attempt", type=int)
    publication_receipt_parser.add_argument("--metadata-producer-source-sha")
    publication_receipt_parser.add_argument("--published-blob", action="append", default=[])
    publication_receipt_parser.add_argument("--output", required=True, type=Path)
    publication_saga_parser = subparsers.add_parser(
        "publication-saga", help="write a read-only ledger for external publication outcomes"
    )
    publication_saga_parser.add_argument("--tag", required=True)
    publication_saga_parser.add_argument("--repository", required=True)
    publication_saga_parser.add_argument("--publication-run-id", required=True)
    publication_saga_parser.add_argument("--publication-run-attempt", required=True, type=int)
    publication_saga_parser.add_argument("--publication-source-sha", required=True)
    for target in ("homebrew", "aur"):
        publication_saga_parser.add_argument(
            f"--{target}-result", required=True, choices=("success", "failure", "cancelled", "skipped")
        )
        publication_saga_parser.add_argument(
            f"--{target}-ready", required=True, choices=("true", "false", "unknown")
        )
        publication_saga_parser.add_argument(f"--{target}-outcome", default="")
        publication_saga_parser.add_argument(f"--{target}-artifact-id", default="")
        publication_saga_parser.add_argument(f"--{target}-receipt", required=True, type=Path)
    publication_saga_parser.add_argument("--output", required=True, type=Path)
    publication_saga_parser.add_argument("--summary", required=True, type=Path)
    check_sizes_parser = subparsers.add_parser("check-asset-sizes")
    check_sizes_parser.add_argument("--assets-json", required=True, type=Path)
    check_sizes_parser.add_argument("--pattern", action="append")
    upload_plan_parser = subparsers.add_parser(
        "plan-upload", help="select absent draft assets without replacing existing names"
    )
    upload_plan_parser.add_argument("--assets-json", required=True, type=Path)
    upload_plan_parser.add_argument("--asset", required=True, action="append", type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        _run(args)
    except ReleaseError as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    sys.exit(main())
