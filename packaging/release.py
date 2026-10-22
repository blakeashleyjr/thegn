#!/usr/bin/env python3
"""Validate release assets and render deterministic package-manager metadata.

This module deliberately uses only the Python standard library. It performs no
network access, invokes no package manager or project binary, and reads no
credentials.
"""

from __future__ import annotations

import argparse
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
RELEASE_MANIFEST_NAME = "thegn-{tag}-manifest.json"
MAX_MEMBERS = 64
MAX_MEMBER_BYTES = 256 * 1024 * 1024
MAX_EXPANDED_BYTES = 512 * 1024 * 1024
MAX_PATH_CHARS = 255
# Expanded bytes may be at most this multiple of the archive's on-disk size.
# Release archives compress about 3-4x; gzip's ceiling is about 1032x, so 100
# refuses decompression bombs with wide headroom.
MAX_COMPRESSION_RATIO = 100
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


def load_release_manifest(path: Path, tag: str) -> dict[str, tuple[str, int]]:
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
    return entries


def verify_release_asset_files(
    entries: Mapping[str, tuple[str, int]], paths: Sequence[Path]
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
                    if member.issparse() or any(
                        key.startswith("GNU.sparse.") for key in member.pax_headers
                    ):
                        raise ReleaseError(
                            f"archive {path} member {member.name!r} is a sparse file"
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
    return {
        "schema_version": 1,
        "tag": tag,
        "commit": commit,
        "tag_object": tag_object,
        "workflow_sha": workflow_sha,
        "run_id": run_id,
        "assets": result_assets,
    }


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
        if not invocation.startswith(expected_run):
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


def _run(args: argparse.Namespace) -> None:
    if args.command == "verify-assets":
        if RELEASE_TAG_RE.fullmatch(args.tag) is None:
            raise ReleaseError(f"invalid release tag {args.tag!r}")
        entries = load_release_manifest(args.release_manifest, args.tag)
        verified = verify_release_asset_files(entries, args.asset)
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
    verify_assets_parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    verify_assets_parser.add_argument("--repo-root", type=Path, default=REPOSITORY_ROOT)
    verify_attestation_parser = subparsers.add_parser("verify-attestation")
    verify_attestation_parser.add_argument("--archive", required=True, type=Path)
    verify_attestation_parser.add_argument("--verification", required=True, type=Path)
    verify_attestation_parser.add_argument("--repository", required=True)
    verify_attestation_parser.add_argument("--server-url", required=True)
    verify_attestation_parser.add_argument("--source-digest", required=True)
    verify_attestation_parser.add_argument("--run-id", required=True)
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
