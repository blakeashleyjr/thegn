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
TARGET_RE = re.compile(r"[a-z0-9_]+(?:-[a-z0-9_]+){2,}")
PLACEHOLDER_RE = re.compile(r"@@([A-Z][A-Z0-9_]*)@@")
FORBIDDEN_OUTPUT = ("REPLACE_WITH_", "thegn-dev", ".SRCINFO")
WINDOWS_ERROR = "Windows release lane is not enabled"
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
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
    except OSError as error:
        raise ReleaseError(f"cannot read release archive {path}: {error}") from error
    return digest.hexdigest()


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
    try:
        if platform == "unix":
            with tarfile.open(path, "r:gz") as archive:
                members = archive.getmembers()
                names = _safe_archive_names((member.name for member in members), path)
                unsafe = [
                    member.name
                    for member in members
                    if not (member.isfile() or member.isdir())
                ]
                regular = {
                    member.name.rstrip("/") for member in members if member.isfile()
                }
        else:
            with zipfile.ZipFile(path) as archive:
                members = archive.infolist()
                names = _safe_archive_names((member.filename for member in members), path)
                unsafe = []
                regular = set()
                for member in members:
                    mode = member.external_attr >> 16
                    file_type = stat.S_IFMT(mode)
                    if file_type not in (0, stat.S_IFREG, stat.S_IFDIR):
                        unsafe.append(member.filename)
                    elif not member.is_dir():
                        regular.add(member.filename.rstrip("/"))
    except (OSError, tarfile.TarError, zipfile.BadZipFile) as error:
        raise ReleaseError(f"cannot inspect release archive {path}: {error}") from error
    if unsafe:
        raise ReleaseError(
            f"archive {path} contains non-regular member(s): {', '.join(unsafe)}"
        )
    missing = [name for name in required_files if name not in names]
    if missing:
        raise ReleaseError(f"archive {path} is missing root file(s): {', '.join(missing)}")
    non_regular = [name for name in required_files if name not in regular]
    if non_regular:
        raise ReleaseError(
            f"archive {path} has non-regular required file(s): {', '.join(non_regular)}"
        )


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
