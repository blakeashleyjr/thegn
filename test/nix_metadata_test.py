#!/usr/bin/env python3
"""Keep the Nix package identity, license metadata, and license files in sync with Cargo."""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPDX_TO_NIX = {"MIT": "mit", "Apache-2.0": "asl20"}
LICENSE_FILES = {"LICENSE-MIT", "LICENSE-APACHE"}


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


def validate(root: Path = ROOT) -> list[str]:
    return validate_texts(
        (root / "Cargo.toml").read_text(),
        (root / "flake.nix").read_text(),
        (root / "nix/package.nix").read_text(),
        (root / "nix/source.nix").read_text(),
        {name for name in LICENSE_FILES if (root / name).is_file()},
    )


def main(root: Path = ROOT) -> int:
    problems = validate(root)
    if problems:
        print("FAIL: Nix package metadata drift:")
        for problem in problems:
            print(f"  {problem}")
        return 1
    print("OK: Nix package versions, licenses, and installed license files follow Cargo")
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

    print("SELF-TEST OK: stable and prerelease identity fixtures")
    return 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    sys.exit(self_test() if args.self_test else main(args.root.resolve()))
