#!/usr/bin/env python3
"""Exercise standalone deb and rpm packages in disposable containers."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import secrets
import shlex
import shutil
import stat
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable


DEB_IMAGES = ("docker.io/library/debian:stable-slim", "docker.io/library/ubuntu:latest")
RPM_IMAGES = ("registry.fedoraproject.org/fedora:latest",)
DEFAULT_STEP_TIMEOUT_S = 1800  # primary sets this from the per-step durations the first real rehearsal records
OUTPUT_TAIL_BYTES = 4096
DEB_NAME = re.compile(r"^thegn_(?P<version>[0-9A-Za-z.+~-]+)_amd64\.deb$")
RPM_NAME = re.compile(r"^thegn-(?P<version>[0-9A-Za-z.+~-]+)-1\.x86_64\.rpm$")

_RELEASE_PATH = Path(__file__).resolve().with_name("release.py")
_RELEASE_SPEC = importlib.util.spec_from_file_location("thegn_release_for_rehearsal", _RELEASE_PATH)
if _RELEASE_SPEC is None or _RELEASE_SPEC.loader is None:
    raise RuntimeError(f"cannot load release verifier library at {_RELEASE_PATH}")
release = importlib.util.module_from_spec(_RELEASE_SPEC)
sys.modules[_RELEASE_SPEC.name] = release
_RELEASE_SPEC.loader.exec_module(release)


class RehearsalError(ValueError):
    """Invalid CLI input or unsafe package path."""


@dataclass(frozen=True)
class Package:
    path: Path
    basename: str
    version: str
    sha256: str


@dataclass(frozen=True)
class Step:
    name: str
    argv: list[str]


@dataclass
class Leg:
    image: str
    container: str
    pull_argv: list[str]
    digest_argv: list[str]
    start_argv: list[str]
    remove_argv: list[str]
    steps: list[Step]
    packages: list[Package]
    results: list[dict[str, object]] = field(default_factory=list)
    image_digests: list[str] | None = None
    passed: bool = False


@dataclass(frozen=True)
class StepResult:
    rc: int | None
    stdout: str | bytes
    stderr: str | bytes
    duration_ms: int
    timed_out: bool = False


@dataclass
class Evidence:
    runtime: str
    runtime_version: str
    packages: list[Package]
    legs: list[Leg]
    passed: bool
    json_path: Path | None = None
    markdown_path: Path | None = None
    interrupted: bool = False
    release_manifest_sha256: str | None = None


def _validate_package(value: str | Path | None, kind: str) -> Package | None:
    if value is None:
        return None
    path = Path(value).expanduser().absolute()
    if ":" in str(path) or "," in str(path):
        raise RehearsalError(f"package path {path} contains a ':' or ',' unsupported by container volume syntax")
    try:
        mode = path.lstat().st_mode
    except OSError as error:
        raise RehearsalError(f"package path {path}: {error.strerror}") from error
    if not stat.S_ISREG(mode):
        raise RehearsalError(f"package path {path} must be a regular file and not a symlink")
    match = (DEB_NAME if kind == "deb" else RPM_NAME).fullmatch(path.name)
    if match is None:
        raise RehearsalError(f"package path {path} has an invalid {kind} release filename")
    digest = hashlib.sha256()
    with path.open("rb") as package_file:
        for chunk in iter(lambda: package_file.read(1024 * 1024), b""):
            digest.update(chunk)
    return Package(path, path.name, match.group("version"), digest.hexdigest())


def _validated_packages(args: argparse.Namespace) -> tuple[list[Package], str]:
    new_deb = _validate_package(args.deb, "deb")
    previous_deb = _validate_package(args.previous_deb, "deb")
    new_rpm = _validate_package(args.rpm, "rpm")
    previous_rpm = _validate_package(args.previous_rpm, "rpm")
    if new_deb is None and new_rpm is None:
        raise RehearsalError("at least one of --deb or --rpm is required")
    if previous_deb is not None and new_deb is None:
        raise RehearsalError("--previous-deb requires --deb")
    if previous_rpm is not None and new_rpm is None:
        raise RehearsalError("--previous-rpm requires --rpm")
    for current, previous, flag in (
        (new_deb, previous_deb, "--previous-deb"),
        (new_rpm, previous_rpm, "--previous-rpm"),
    ):
        if current is not None and previous is not None and current.version == previous.version:
            raise RehearsalError(f"{flag} package must have a different version from the new package")
    versions = [package.version for package in (new_deb, new_rpm) if package is not None]
    if len(set(versions)) != 1:
        raise RehearsalError("deb and rpm packages must have the same version")
    return [p for p in (new_deb, previous_deb, new_rpm, previous_rpm) if p is not None], versions[0]


def _verify_against_manifests(args: argparse.Namespace, packages: list[Package], version: str) -> str | None:
    release_manifest = args.release_manifest
    previous_manifest = args.previous_release_manifest
    previous_packages = [package for package in packages if package.version != version]
    if previous_manifest is not None and not previous_packages:
        raise RehearsalError("--previous-release-manifest requires previous packages")
    previous_versions = {package.version for package in previous_packages}
    if len(previous_versions) > 1:
        raise RehearsalError("previous deb and rpm packages must have the same version")
    if release_manifest is None and not args.dry_run:
        raise RehearsalError(
            f"--release-manifest is required: download thegn-v{version}-manifest.json with the packages "
            "and verify its attestation first"
        )
    try:
        manifest_digest = None
        if release_manifest is not None:
            entries = release.load_release_manifest(release_manifest, f"v{version}")
            current_packages = [package for package in packages if package.version == version]
            release.verify_release_asset_files(entries, [package.path for package in current_packages])
            manifest_digest = release._hash_payload_file(release_manifest)
        if previous_packages:
            if previous_manifest is None and not args.dry_run:
                raise RehearsalError("--previous-release-manifest is required when rehearsing an upgrade leg")
            if previous_manifest is not None:
                previous_version = next(iter(previous_versions))
                entries = release.load_release_manifest(previous_manifest, f"v{previous_version}")
                release.verify_release_asset_files(entries, [package.path for package in previous_packages])
        return manifest_digest
    except release.ReleaseError as error:
        raise RehearsalError(str(error)) from error


def _exec_step(runtime: str, container: str, name: str, script: str) -> Step:
    return Step(name, [runtime, "exec", container, "sh", "-c", script])


def _steps(runtime: str, container: str, kind: str, current: Package, previous: Package | None) -> list[Step]:
    steps: list[Step] = []
    if kind == "deb":
        steps.append(_exec_step(runtime, container, "update", "apt-get update"))
        if previous is not None:
            steps.append(_exec_step(runtime, container, "install-previous", f"DEBIAN_FRONTEND=noninteractive apt-get install -y /rehearsal/{previous.basename}"))
            steps.append(_exec_step(runtime, container, "previous-version", f"test \"$(thegn --version)\" = {shlex.quote('thegn ' + previous.version)}"))
        if previous is None:
            steps.append(_exec_step(runtime, container, "install", f"DEBIAN_FRONTEND=noninteractive apt-get install -y /rehearsal/{current.basename}"))
        else:
            steps.append(_exec_step(runtime, container, "upgrade-from-previous", f"DEBIAN_FRONTEND=noninteractive apt-get install -y /rehearsal/{current.basename}"))
    else:
        if previous is not None:
            steps.append(_exec_step(runtime, container, "install-previous", f"dnf install -y /rehearsal/{previous.basename}"))
            steps.append(_exec_step(runtime, container, "previous-version", f"test \"$(thegn --version)\" = {shlex.quote('thegn ' + previous.version)}"))
            steps.append(_exec_step(runtime, container, "upgrade-from-previous", f"dnf upgrade -y /rehearsal/{current.basename}"))
        else:
            steps.append(_exec_step(runtime, container, "install", f"dnf install -y /rehearsal/{current.basename}"))
    steps.extend(
        [
            _exec_step(runtime, container, "version", f"test \"$(thegn --version)\" = {shlex.quote('thegn ' + current.version)}"),
            _exec_step(runtime, container, "tg-version", f"test \"$(tg --version)\" = {shlex.quote('thegn ' + current.version)}"),
            _exec_step(runtime, container, "tg-symlink", "test -L /usr/bin/tg"),
            _exec_step(runtime, container, "licenses", "test -f /usr/share/licenses/thegn/LICENSE-MIT && test -f /usr/share/licenses/thegn/LICENSE-APACHE"),
            _exec_step(runtime, container, "doctor", "HOME=/tmp/rehearsal-home thegn doctor --json >/dev/null"),
            _exec_step(runtime, container, "uninstall", "apt-get remove -y thegn" if kind == "deb" else "dnf remove -y thegn"),
            _exec_step(runtime, container, "absent-after-uninstall", "test ! -e /usr/bin/thegn && test ! -e /usr/bin/tg && test ! -L /usr/bin/tg"),
        ]
    )
    return steps


def plan(args: argparse.Namespace, packages: list[Package]) -> list[Leg]:
    runtime = args.runtime
    legs: list[Leg] = []
    for kind, new_attr, old_attr, images_attr in (
        ("deb", "deb", "previous_deb", "deb_images"),
        ("rpm", "rpm", "previous_rpm", "rpm_images"),
    ):
        current = next((p for p in packages if p.path == Path(getattr(args, new_attr)).expanduser().absolute()), None) if getattr(args, new_attr) else None
        if current is None:
            continue
        previous = next((p for p in packages if args.__dict__.get(old_attr) and p.path == Path(getattr(args, old_attr)).expanduser().absolute()), None)
        for image in getattr(args, images_attr):
            container = "thegn-rehearse-" + secrets.token_hex(6)
            mounts: list[str] = []
            for package in (previous, current):
                if package is not None:
                    mounts.extend(["--volume", f"{package.path}:/rehearsal/{package.basename}:ro"])
            pull = [runtime, "pull", image]
            digest = [runtime, "image", "inspect", "--format", "{{json .RepoDigests}}", image]
            start = [runtime, "run", "-d", "--rm=false", "--name", container, *mounts, image, "sleep", "infinity"]
            remove = [runtime, "rm", "-f", container]
            legs.append(
                Leg(
                    image,
                    container,
                    pull,
                    digest,
                    start,
                    remove,
                    _steps(runtime, container, kind, current, previous),
                    [p for p in (previous, current) if p is not None],
                )
            )
    return legs


def _tail(value: str | bytes) -> str:
    raw = value.encode() if isinstance(value, str) else value
    return raw[-OUTPUT_TAIL_BYTES:].decode(errors="replace")


def _run(argv: list[str], timeout: float) -> StepResult:
    started = time.monotonic()
    try:
        result = subprocess.run(argv, capture_output=True, timeout=timeout, stdin=subprocess.DEVNULL, check=False)
        return StepResult(result.returncode, result.stdout, result.stderr, int((time.monotonic() - started) * 1000))
    except subprocess.TimeoutExpired as error:
        return StepResult(None, error.stdout or b"", error.stderr or b"", int((time.monotonic() - started) * 1000), True)
    except OSError as error:
        return StepResult(127, b"", str(error).encode(), int((time.monotonic() - started) * 1000))


def run_rehearsal(
    args: argparse.Namespace,
    runner: Callable[[list[str], float], StepResult],
    packages: list[Package],
) -> Evidence:
    legs = plan(args, packages)
    runtime = args.runtime
    try:
        runtime_result = runner([runtime, "--version"], args.step_timeout)
    except KeyboardInterrupt:
        return Evidence(runtime, "", packages, legs, False, interrupted=True)
    runtime_version = _tail(runtime_result.stdout)
    overall = runtime_result.rc == 0 and not runtime_result.timed_out
    interrupted = False
    for leg in legs:
        leg_passed = True
        try:
            result = runner(leg.pull_argv, args.step_timeout)
            leg.results.append(_result_record("pull", leg.pull_argv, result))
            if result.rc != 0 or result.timed_out:
                leg_passed = False
            else:
                result = runner(leg.digest_argv, args.step_timeout)
                leg.results.append(_result_record("digest", leg.digest_argv, result))
                if result.rc == 0 and not result.timed_out:
                    try:
                        digests = json.loads(_tail(result.stdout))
                        leg.image_digests = digests if isinstance(digests, list) else None
                    except json.JSONDecodeError:
                        leg.image_digests = None
                    if not leg.image_digests:
                        leg_passed = False
                else:
                    leg_passed = False
            if not any(item["name"] == "digest" for item in leg.results):
                leg.results.append(_skipped_record("digest", leg.digest_argv))
            if not leg_passed:
                leg.results.append(_skipped_record("start", leg.start_argv))
                leg.results.extend(_skipped_record(step.name, step.argv) for step in leg.steps)
            if leg_passed:
                try:
                    start = runner(leg.start_argv, args.step_timeout)
                    leg.results.append(_result_record("start", leg.start_argv, start))
                    if start.rc != 0 or start.timed_out:
                        leg_passed = False
                        leg.results.extend(_skipped_record(step.name, step.argv) for step in leg.steps)
                    if leg_passed:
                        for index, step in enumerate(leg.steps):
                            result = runner(step.argv, args.step_timeout)
                            leg.results.append(_result_record(step.name, step.argv, result))
                            if result.rc != 0 or result.timed_out:
                                leg_passed = False
                                for skipped in leg.steps[index + 1 :]:
                                    leg.results.append(_skipped_record(skipped.name, skipped.argv))
                                break
                except KeyboardInterrupt:
                    interrupted = True
                    leg_passed = False
                finally:
                    try:
                        result = runner(leg.remove_argv, args.step_timeout)
                        leg.results.append(_result_record("remove-container", leg.remove_argv, result))
                        if result.rc != 0 or result.timed_out:
                            leg_passed = False
                    except KeyboardInterrupt:
                        interrupted = True
                        leg.results.append(_skipped_record("remove-container", leg.remove_argv))
        except KeyboardInterrupt:
            interrupted = True
            leg_passed = False
            if not any(item["name"] == "pull" for item in leg.results):
                leg.results.append(_skipped_record("pull", leg.pull_argv))
            if not any(item["name"] == "digest" for item in leg.results):
                leg.results.append(_skipped_record("digest", leg.digest_argv))
            if not any(item["name"] == "start" for item in leg.results):
                leg.results.append(_skipped_record("start", leg.start_argv))
            leg.results.extend(_skipped_record(step.name, step.argv) for step in leg.steps)
        leg.passed = leg_passed
        overall = overall and leg_passed
        if interrupted:
            break
    return Evidence(runtime, runtime_version, packages, legs, overall, interrupted=interrupted)


def _result_record(name: str, argv: list[str], result: StepResult) -> dict[str, object]:
    return {"name": name, "argv": argv, "rc": result.rc, "timed_out": result.timed_out, "duration_ms": result.duration_ms, "stdout_tail": _tail(result.stdout), "stderr_tail": _tail(result.stderr)}


def _skipped_record(name: str, argv: list[str]) -> dict[str, object]:
    return {"name": name, "argv": argv, "rc": None, "timed_out": False, "duration_ms": 0, "stdout_tail": "", "stderr_tail": "", "skipped": True}


def _write_evidence(evidence: Evidence, directory: Path, version: str) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    base = f"linux-packages-{version}-{stamp}"
    json_path = directory / f"{base}.json"
    markdown_path = directory / f"{base}.md"
    payload = {
        "runtime": evidence.runtime,
        "runtime_version": evidence.runtime_version,
        "packages": [{"basename": package.basename, "version": package.version, "sha256": package.sha256} for package in evidence.packages],
        "release_manifest_sha256": evidence.release_manifest_sha256,
        "passed": evidence.passed,
        "interrupted": evidence.interrupted,
        "legs": [{"image": leg.image, "image_digests": leg.image_digests, "passed": leg.passed, "steps": leg.results} for leg in evidence.legs],
    }
    descriptor = os.open(json_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        json.dump(payload, output, indent=2)
        output.write("\n")
    rows = [f"# Package rehearsal: {version}", "", f"Result: **{'passed' if evidence.passed else 'failed'}**", "", "| Leg | Step | Result | Duration |", "| --- | --- | --- | ---: |"]
    for leg in evidence.legs:
        for step in leg.results:
            outcome = "skipped" if step.get("skipped") else ("passed" if step.get("rc") == 0 and not step.get("timed_out") else "failed")
            rows.append(f"| {leg.image} | {step['name']} | {outcome} | {step['duration_ms']} ms |")
    md_descriptor = os.open(markdown_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    with os.fdopen(md_descriptor, "w", encoding="utf-8") as output:
        output.write("\n".join(rows) + "\n")
    evidence.json_path = json_path
    evidence.markdown_path = markdown_path


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--deb")
    parser.add_argument("--rpm")
    parser.add_argument("--previous-deb")
    parser.add_argument("--previous-rpm")
    parser.add_argument("--release-manifest", type=Path)
    parser.add_argument("--previous-release-manifest", type=Path)
    parser.add_argument("--runtime", choices=("podman", "docker"))
    parser.add_argument("--deb-image", action="append", dest="deb_images")
    parser.add_argument("--rpm-image", action="append", dest="rpm_images")
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--step-timeout", type=float, default=DEFAULT_STEP_TIMEOUT_S)
    parser.add_argument("--dry-run", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = _parser()
    args = parser.parse_args(argv)
    args.deb_images = args.deb_images or list(DEB_IMAGES)
    args.rpm_images = args.rpm_images or list(RPM_IMAGES)
    try:
        packages, version = _validated_packages(args)
        manifest_digest = _verify_against_manifests(args, packages, version)
        if not args.dry_run and args.evidence_dir is None:
            raise RehearsalError("--evidence-dir is required unless --dry-run is used")
        if args.step_timeout <= 0:
            raise RehearsalError("--step-timeout must be greater than zero")
        runtime = args.runtime or next((name for name in ("podman", "docker") if shutil.which(name)), None)
        if runtime is None and args.dry_run:
            runtime = "podman"
            print("note: no container runtime found; using podman as the dry-run placeholder", file=sys.stderr)
        elif runtime is None:
            raise RehearsalError("no container runtime (podman or docker) on PATH")
        args.runtime = runtime
        legs = plan(args, packages)
        if args.dry_run:
            for leg in legs:
                commands = [leg.pull_argv, leg.digest_argv, leg.start_argv, *[step.argv for step in leg.steps], leg.remove_argv]
                for command in commands:
                    print(shlex.join(command))
            return 0
        evidence = run_rehearsal(args, _run, packages)
        evidence.release_manifest_sha256 = manifest_digest
        _write_evidence(evidence, args.evidence_dir, version)
        print(f"evidence: {evidence.json_path}")
        print(f"summary: {evidence.markdown_path}")
        return 0 if evidence.passed else 1
    except (RehearsalError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
