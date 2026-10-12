#!/usr/bin/env python3
"""Keep Nix and release package identity and license metadata in sync with Cargo."""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPDX_TO_NIX = {"MIT": "mit", "Apache-2.0": "asl20"}
LICENSE_FILES = {"LICENSE-MIT", "LICENSE-APACHE"}
RELEASE_TEMPLATES = {
    "nfpm.yaml.tmpl": ("packaging/nfpm.yaml.tmpl", "@@VERSION@@", "MIT OR Apache-2.0"),
    "scoop/thegn.json.tmpl": ("packaging/scoop/thegn.json.tmpl", "@@VERSION@@", "MIT OR Apache-2.0"),
    "winget/thegn.yaml.tmpl": ("packaging/winget/thegn.yaml.tmpl", "@@VERSION@@", "MIT OR Apache-2.0"),
    "homebrew/thegn.rb.tmpl": ("packaging/homebrew/thegn.rb.tmpl", "@@VERSION@@", 'any_of: ["MIT", "Apache-2.0"]'),
    "aur/PKGBUILD.tmpl": ("packaging/aur/PKGBUILD.tmpl", "@@PKGVER@@", "('MIT' 'Apache-2.0')"),
}


def block_after(source: str, marker: str) -> str | None:
    """Return the brace-delimited expression immediately following marker."""
    start = source.find(marker)
    if start < 0:
        return None
    opening = source.find("{", start + len(marker))
    if opening < 0:
        return None
    depth = 0
    for index in range(opening, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[opening + 1 : index]
    return None


def expected_nix_licenses(spdx: str) -> list[str] | None:
    try:
        return [SPDX_TO_NIX[item.strip()] for item in spdx.split(" OR ")]
    except KeyError:
        return None


def validate_texts(
    cargo_text: str,
    flake: str,
    package: str,
    source: str,
    existing_files: set[str],
) -> list[str]:
    problems: list[str] = []
    try:
        identity = tomllib.loads(cargo_text)["workspace"]["package"]
        cargo_version = identity["version"]
        cargo_license = identity["license"]
    except (tomllib.TOMLDecodeError, KeyError, TypeError) as exc:
        return [f"Cargo.toml workspace package identity is unreadable: {exc}"]

    nix_licenses = expected_nix_licenses(cargo_license)
    if not isinstance(cargo_version, str) or not cargo_version:
        problems.append("Cargo workspace package version is missing or invalid")
    if not re.search(
        r"cargoVersion\s*=\s*\(builtins\.fromTOML\s*"
        r"\(builtins\.readFile\s+\./Cargo\.toml\)\)\.workspace\.package\.version\s*;",
        flake,
    ):
        problems.append("flake.nix cargoVersion does not read Cargo.toml workspace.package.version")
    if not re.search(
        r"cargoLicense\s*=\s*\(builtins\.fromTOML\s*"
        r"\(builtins\.readFile\s+\./Cargo\.toml\)\)\.workspace\.package\.license\s*;",
        flake,
    ):
        problems.append("flake.nix cargoLicense does not read Cargo.toml workspace.package.license")
    if nix_licenses is None:
        problems.append(f"unknown Cargo SPDX license expression: {cargo_license!r}")
    else:
        nix_list = "[" + " ".join(nix_licenses) + "]"
        for derivation in ("thegn", "thegnDev"):
            if not re.search(
                rf"assert\s+{derivation}\.meta\.license\s*==\s*"
                rf"\(with\s+pkgs\.lib\.licenses;\s*{re.escape(nix_list)}\)",
                flake,
            ):
                problems.append(f"{derivation} Nix license assertion does not match {nix_list}")
        if not re.search(
            rf'assert\s+\(system\s*!=\s*"x86_64-linux"\s*\|\|\s*'
            rf'thegnMusl\.meta\.license\s*==\s*'
            rf'\(with\s+pkgs\.lib\.licenses;\s*{re.escape(nix_list)}\)\)',
            flake,
        ):
            problems.append(f"thegnMusl Nix license assertion does not match {nix_list}")

    expected_bindings = {"cargoCommonArgs", "thegn", "thegnDev", "thegnMusl"}
    derivation_bindings = re.findall(
        r"^\s*(cargoCommonArgs|thegn\w*)\s*=\s*"
        r"(?=(?:\{|pkgs\.callPackage|muslRustPlatform\.buildRustPackage))",
        flake,
        re.MULTILINE,
    )
    if not expected_bindings.issubset(derivation_bindings):
        problems.append("flake.nix is missing a known Thegn Nix derivation binding")
    for label in derivation_bindings:
        marker = f"{label} ="
        block = block_after(flake, marker)
        if block is None:
            problems.append(f"missing {label} version derivation")
            continue
        if not re.search(r"\bversion\s*=\s*cargoVersion\s*;", block):
            problems.append(f"{label} does not use version = cargoVersion")
        if re.search(r'\bversion\s*=\s*"', block):
            problems.append(f"{label} has a quoted version literal")

    if not re.search(r"cargoArtifacts\s*=\s*craneLib\.buildDepsOnly\s+cargoCommonArgs\s*;", flake):
        problems.append("cargoArtifacts no longer derives from cargoCommonArgs")
    for version in ("9.9.9", "9.9.9-rc.1"):
        if not re.search(
            rf'assert\s+\(pkgs\.callPackage\s+\./nix/package\.nix\s*\{{'
            rf'(?:(?!\}};).)*version\s*=\s*"{re.escape(version)}";'
            rf'(?:(?!\}};).)*\)\.version\s*==\s*"{re.escape(version)}";',
            flake,
            re.DOTALL,
        ):
            problems.append(f"version-metadata lacks the {version} eval fixture")
    if not re.search(
        r'assert\s+\(pkgs\.callPackage\s+\./nix/package\.nix\s*\{'
        r'(?:(?!\};).)*version\s*=\s*"9\.9\.9-rc\.1";'
        r'(?:(?!\};).)*channel\s*=\s*"dev";'
        r'(?:(?!\};).)*\)\.pname\s*==\s*"thegn-dev";',
        flake,
        re.DOTALL,
    ):
        problems.append("version-metadata lacks the dev-name eval fixture")
    if not re.search(
        r'assert\s+\(pkgs\.callPackage\s+\./nix/package\.nix\s*\{'
        r'(?:(?!\};).)*version\s*=\s*"9\.9\.9";'
        r'(?:(?!\};).)*\)\.pname\s*==\s*"thegn";',
        flake,
        re.DOTALL,
    ):
        problems.append("version-metadata lacks the stable package-name eval fixture")
    if not re.search(
        r'version\s*\?\s*\(builtins\.fromTOML\s*'
        r'\(builtins\.readFile\s+\.\./Cargo\.toml\)\)\.workspace\.package\.version',
        package,
    ):
        problems.append("nix/package.nix version default does not read ../Cargo.toml")

    roots = re.search(r"roots\s*=\s*\[(.*?)\];", source, re.DOTALL)
    allowed = set(re.findall(r'"([^"]+)"', roots.group(1))) if roots else set()
    for name in LICENSE_FILES:
        if name not in allowed:
            problems.append(f"nix/source.nix does not allowlist {name}")
        if name not in existing_files:
            problems.append(f"{name} is missing from the source tree")
        if not re.search(rf"install\s+-Dm644\s+[^\n]*{name}[^\n]*", package):
            problems.append(f"nix/package.nix does not install {name}")

    musl = block_after(flake, "thegnMusl = muslRustPlatform.buildRustPackage")
    # The musl installPhase is a quoted shell string after the derivation attrs.
    if musl is None:
        problems.append("missing thegnMusl derivation")
    else:
        musl_phase = flake[flake.find("thegnMusl = muslRustPlatform.buildRustPackage") :]
        musl_phase = musl_phase[: musl_phase.find("\n      };\n")]
        for name in LICENSE_FILES:
            if not re.search(rf"install\s+-Dm644\s+[^\n]*{name}[^\n]*", musl_phase):
                problems.append(f"thegnMusl installPhase does not install {name}")

    check_start = flake.find("version-metadata =")
    check_end = flake.find("dev-docker-shim =", check_start)
    check = flake[check_start:check_end] if check_start >= 0 and check_end >= 0 else ""
    if not check:
        problems.append("missing version-metadata check")
    else:
        for command in (
            '${thegn}/bin/thegn --version',
            '${thegnDev}/bin/thegn-dev --version',
            '${thegnMusl}/bin/thegn --version',
        ):
            exact = rf'test\s+"\$\({re.escape(command)}\)"\s*=\s*"thegn \$\{{cargoVersion\}}"'
            if not re.search(exact, check):
                problems.append(f"{command} lacks an exact binary-version assertion")
        if "grep -F" in check:
            problems.append("version-metadata uses substring grep -F instead of exact equality")

    return problems


def validate_release_texts(
    cargo_text: str,
    release_yml: str,
    release_json: str,
    templates: dict[str, str],
) -> list[str]:
    problems: list[str] = []
    try:
        license_expr = tomllib.loads(cargo_text)["workspace"]["package"]["license"]
        targets = json.loads(release_json)["targets"]
        enabled_targets = {
            target for target, metadata in targets.items() if metadata.get("enabled") is True
        }
        archive_files = set(json.loads(release_json)["archive"]["root_files"])
    except (tomllib.TOMLDecodeError, json.JSONDecodeError, KeyError, TypeError, AttributeError) as exc:
        return [f"release metadata is unreadable: {exc}"]

    license_parts = [part.strip() for part in license_expr.split(" OR ")]
    if not license_parts or any(part not in SPDX_TO_NIX for part in license_parts):
        problems.append(f"unknown Cargo SPDX license expression for release templates: {license_expr!r}")
    else:
        spdx_text = " OR ".join(license_parts)
        homebrew = "any_of: [" + ", ".join(f'"{part}"' for part in license_parts) + "]"
        aur = "(" + " ".join(f"'{part}'" for part in license_parts) + ")"
        expected_licenses = {
            "nfpm.yaml.tmpl": spdx_text,
            "scoop/thegn.json.tmpl": spdx_text,
            "winget/thegn.yaml.tmpl": spdx_text,
            "homebrew/thegn.rb.tmpl": homebrew,
            "aur/PKGBUILD.tmpl": aur,
        }
        for name, expected in expected_licenses.items():
            text = templates.get(name, "")
            if expected not in text:
                problems.append(f"{name} license spelling does not match Cargo license {license_expr!r}")

    for name, (_, placeholder, _) in RELEASE_TEMPLATES.items():
        text = templates.get(name, "")
        if placeholder not in text:
            problems.append(f"{name} is missing version placeholder {placeholder}")
        if re.search(r"[\"']v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?[\"']", text):
            problems.append(f"{name} contains a quoted semver literal")

    matrix_targets = set(re.findall(r"^\s*-\s+target:\s*([A-Za-z0-9_-]+)\s*$", release_yml, re.MULTILINE))
    if matrix_targets != enabled_targets:
        problems.append(
            "release.yml upload matrix targets differ from packaging/release.json enabled targets: "
            f"matrix-only={sorted(matrix_targets - enabled_targets)}, "
            f"manifest-only={sorted(enabled_targets - matrix_targets)}"
        )

    upload_step = re.search(r"^[ \t]*- id: upload[ \t]*$", release_yml, re.MULTILINE)
    upload_config = release_yml[upload_step.end() :] if upload_step else ""
    next_step = re.search(r"\n[ \t]*- (?:name:|id:|uses:)", upload_config)
    if next_step:
        upload_config = upload_config[: next_step.start()]
    include_match = re.search(r"^\s*include:\s*([^\n#]+)", upload_config, re.MULTILINE)
    include_files = (
        {part.strip() for part in include_match.group(1).split(",") if part.strip()}
        if include_match
        else set()
    )
    missing_licenses = sorted(LICENSE_FILES - include_files)
    if missing_licenses:
        problems.append(f"release.yml upload include is missing license files: {missing_licenses}")
    missing_archive = sorted(LICENSE_FILES - archive_files)
    if missing_archive:
        problems.append(f"packaging/release.json archive root_files is missing license files: {missing_archive}")
    return problems


def validate(root: Path = ROOT) -> list[str]:
    cargo_text = (root / "Cargo.toml").read_text()
    nix_problems = validate_texts(
        cargo_text,
        (root / "flake.nix").read_text(),
        (root / "nix/package.nix").read_text(),
        (root / "nix/source.nix").read_text(),
        {name for name in LICENSE_FILES if (root / name).is_file()},
    )
    templates = {
        name: (root / path).read_text()
        for name, (path, _, _) in RELEASE_TEMPLATES.items()
    }
    return nix_problems + validate_release_texts(
        cargo_text,
        (root / ".github/workflows/release.yml").read_text(),
        (root / "packaging/release.json").read_text(),
        templates,
    )


def main(root: Path = ROOT) -> int:
    problems = validate(root)
    if problems:
        print("FAIL: Nix package metadata drift:")
        for problem in problems:
            print(f"  {problem}")
        return 1
    print(
        "OK: Nix package metadata, release templates, upload matrix, and archive files "
        "follow Cargo and packaging metadata"
    )
    return 0


def fixture(version: str) -> tuple[str, str, str, str, set[str]]:
    cargo = (
        "[workspace.package]\n"
        f'version = "{version}"\n'
        'license = "MIT OR Apache-2.0"\n'
    )
    flake = '''
      cargoVersion = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
      cargoLicense = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.license;
      cargoCommonArgs = { pname = "thegn"; version = cargoVersion; };
      cargoArtifacts = craneLib.buildDepsOnly cargoCommonArgs;
      thegn = pkgs.callPackage ./nix/package.nix { version = cargoVersion; };
      thegnDev = pkgs.callPackage ./nix/package.nix { version = cargoVersion; channel = "dev"; };
      thegnMusl = muslRustPlatform.buildRustPackage {
        pname = "thegn-musl"; version = cargoVersion;
        installPhase = ''
          install -Dm644 ${src}/LICENSE-MIT $out/share/licenses/thegn-musl/LICENSE-MIT
          install -Dm644 ${src}/LICENSE-APACHE $out/share/licenses/thegn-musl/LICENSE-APACHE
        '';
      };
      checks = {
        version-metadata = assert cargoLicense == "MIT OR Apache-2.0";
        assert thegn.meta.license == (with pkgs.lib.licenses; [mit asl20]);
        assert thegnDev.meta.license == (with pkgs.lib.licenses; [mit asl20]);
        assert (system != "x86_64-linux" || thegnMusl.meta.license == (with pkgs.lib.licenses; [mit asl20]));
        assert (pkgs.callPackage ./nix/package.nix {
          src = thegnSrc;
          yazi = yaziPinned;
          inherit yaziDeps craneLib cargoArtifacts;
          version = "9.9.9";
        }).version == "9.9.9";
        assert (pkgs.callPackage ./nix/package.nix {
          src = thegnSrc;
          yazi = yaziPinned;
          inherit yaziDeps craneLib cargoArtifacts;
          version = "9.9.9";
        }).pname == "thegn";
        assert (pkgs.callPackage ./nix/package.nix {
          src = thegnSrc;
          yazi = yaziPinned;
          inherit yaziDeps craneLib cargoArtifacts;
          version = "9.9.9-rc.1";
          channel = "dev";
        }).version == "9.9.9-rc.1";
        assert (pkgs.callPackage ./nix/package.nix {
          src = thegnSrc;
          yazi = yaziPinned;
          inherit yaziDeps craneLib cargoArtifacts;
          version = "9.9.9-rc.1";
          channel = "dev";
        }).pname == "thegn-dev";
        pkgs.runCommand "metadata" {} ''
          test "$(${thegn}/bin/thegn --version)" = "thegn ${cargoVersion}"
          test "$(${thegnDev}/bin/thegn-dev --version)" = "thegn ${cargoVersion}"
          test "$(${thegnMusl}/bin/thegn --version)" = "thegn ${cargoVersion}"
        '';
      };
      dev-docker-shim = null;
    '''
    package = '''
      version ? (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version,
      install -Dm644 ${src}/LICENSE-MIT $out/share/licenses/thegn/LICENSE-MIT
      install -Dm644 ${src}/LICENSE-APACHE $out/share/licenses/thegn/LICENSE-APACHE
    '''
    source = '''
      roots = [ "Cargo.toml" "LICENSE-MIT" "LICENSE-APACHE" ];
    '''
    return cargo, flake, package, source, set(LICENSE_FILES)


def release_fixture() -> tuple[str, str, str, dict[str, str]]:
    cargo = '[workspace.package]\nversion = "1.0.0"\nlicense = "MIT OR Apache-2.0"\n'
    release_yml = '''
        - target: x86_64-unknown-linux-gnu
        - target: aarch64-apple-darwin
        - id: upload
        uses: taiki-e/upload-rust-binary-action
        include: LICENSE-MIT,LICENSE-APACHE,README.md
    '''
    release_json = json.dumps(
        {
            "archive": {"root_files": ["thegn", "LICENSE-MIT", "LICENSE-APACHE", "README.md"]},
            "targets": {
                "x86_64-unknown-linux-gnu": {"enabled": True},
                "aarch64-apple-darwin": {"enabled": True},
                "x86_64-pc-windows-msvc": {"enabled": False},
            },
        }
    )
    templates = {
        "nfpm.yaml.tmpl": 'version: "@@VERSION@@"\nlicense: MIT OR Apache-2.0\n',
        "scoop/thegn.json.tmpl": '"version": "@@VERSION@@",\n"license": "MIT OR Apache-2.0"\n',
        "winget/thegn.yaml.tmpl": 'PackageVersion: "@@VERSION@@"\nLicense: MIT OR Apache-2.0\n',
        "homebrew/thegn.rb.tmpl": 'version "@@VERSION@@"\nlicense any_of: ["MIT", "Apache-2.0"]\n',
        "aur/PKGBUILD.tmpl": 'pkgver=@@PKGVER@@\nlicense=(\'MIT\' \'Apache-2.0\')\n',
    }
    return cargo, release_yml, release_json, templates


def self_test() -> int:
    cases = [fixture("1.0.0"), fixture("1.0.0-alpha.2")]
    for cargo, flake, package, source, files in cases:
        problems = validate_texts(cargo, flake, package, source, files)
        if problems:
            print(f"SELF-TEST FAIL: good version fixture rejected: {problems}")
            return 1

    good = cases[0]
    bad_fixtures = (
        (good[1].replace('version = cargoVersion;', 'version = "1.0.0";', 1), good[2], "quoted version"),
        (good[1].replace("[mit asl20]", "[mit]"), good[2], "MIT-only license"),
        (good[1], good[2].replace("install -Dm644 ${src}/LICENSE-APACHE", "# omitted"), "missing license install"),
        (
            good[1].replace(
                'test "$(${thegn}/bin/thegn --version)" = "thegn ${cargoVersion}"',
                '${thegn}/bin/thegn --version | grep -F ${cargoVersion}',
            ),
            good[2],
            "substring version check",
        ),
    )
    for flake, package, label in bad_fixtures:
        problems = validate_texts(good[0], flake, package, good[3], good[4])
        if not problems:
            print(f"SELF-TEST FAIL: {label} drift was accepted")
            return 1
        print(f"SELF-TEST OK: rejected {label} drift")

    release_good = release_fixture()
    problems = validate_release_texts(*release_good)
    if problems:
        print(f"SELF-TEST FAIL: good release fixture rejected: {problems}")
        return 1
    spaced_include = release_good[1].replace(
        "LICENSE-MIT,LICENSE-APACHE", "LICENSE-MIT, LICENSE-APACHE"
    )
    problems = validate_release_texts(
        release_good[0], spaced_include, release_good[2], release_good[3]
    )
    if problems:
        print(f"SELF-TEST FAIL: spaced include list rejected: {problems}")
        return 1
    blank_before_upload = release_good[1].replace(
        "        - id: upload", "\n        - id: upload"
    )
    problems = validate_release_texts(
        release_good[0], blank_before_upload, release_good[2], release_good[3]
    )
    if problems:
        print(f"SELF-TEST FAIL: blank line before upload step rejected: {problems}")
        return 1
    malformed_targets = json.dumps(
        {
            "archive": {"root_files": ["LICENSE-MIT", "LICENSE-APACHE"]},
            "targets": {"x86_64-unknown-linux-gnu": None},
        }
    )
    problems = validate_release_texts(
        release_good[0], release_good[1], malformed_targets, release_good[3]
    )
    if not any(problem.startswith("release metadata is unreadable:") for problem in problems):
        print(f"SELF-TEST FAIL: malformed target metadata was not reported: {problems}")
        return 1
    bad_release_fixtures = (
        (
            release_good[3]
            | {"scoop/thegn.json.tmpl": release_good[3]["scoop/thegn.json.tmpl"].replace(
                "MIT OR Apache-2.0", "MIT"
            )},
            release_good[1],
            release_good[2],
            "MIT-only template license",
        ),
        (
            release_good[3],
            release_good[1].replace("aarch64-apple-darwin", "x86_64-unknown-linux-musl"),
            release_good[2],
            "matrix target missing from manifest",
        ),
        (
            release_good[3],
            release_good[1].replace(",LICENSE-APACHE", ""),
            release_good[2],
            "upload include missing LICENSE-APACHE",
        ),
    )
    for templates, release_yml, release_json, label in bad_release_fixtures:
        problems = validate_release_texts(release_good[0], release_yml, release_json, templates)
        if not problems:
            print(f"SELF-TEST FAIL: {label} drift was accepted")
            return 1
        print(f"SELF-TEST OK: rejected {label}")

    print("SELF-TEST OK: stable and prerelease identity fixtures")
    return 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    sys.exit(self_test() if args.self_test else main(args.root.resolve()))
