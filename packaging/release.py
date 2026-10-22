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
FORBIDDEN_OUTPUT = ("REPLACE_WITH_", "thegn-dev", ".SRCINFO")
WINDOWS_ERROR = "Windows release lane is not enabled"
MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_TRUSTED_DIGESTS_BYTES = 64 * 1024
RELEASE_MANIFEST_NAME = "thegn-{tag}-manifest.json"
MAX_MEMBERS = 64
MAX_MEMBER_BYTES = 256 * 1024 * 1024
MAX_EXPANDED_BYTES = 512 * 1024 * 1024
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
                    if len(member.name) > 255:
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
                    if len(member.filename) > 255:
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
    except (OSError, tarfile.TarError, zipfile.BadZipFile) as error:
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


def _run(args: argparse.Namespace) -> None:
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
