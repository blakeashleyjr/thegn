#!/usr/bin/env python3
"""Hermetic fixture tests for the opt-in Cargo provenance report."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "test" / "cargo-provenance.sh"


def run(args, bin_dir=None, env_extra=None):
    env = os.environ.copy()
    for key in (
        "RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_TARGET_DIR",
        "RUSTFLAGS",
    ):
        env.pop(key, None)
    path = env.get("PATH", "")
    if bin_dir is not None:
        path = f"{bin_dir}{os.pathsep}{path}"
    env["PATH"] = path
    with tempfile.TemporaryDirectory() as home:
        env["HOME"] = home
        if env_extra:
            env.update(env_extra)
        return subprocess.run(
            ["bash", str(SCRIPT), *args],
            env=env,
            capture_output=True,
            text=True,
            timeout=10,
        )


def parse_output(stdout):
    facts = {}
    for line in stdout.splitlines():
        key, separator, value = line.partition(": ")
        if separator:
            facts[key] = value
    return facts


class CargoProvenanceTests(unittest.TestCase):
    def fixture(self, foreign=False, with_sccache=False):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        rustc = bin_dir / "rustc"
        rustc.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' 'rustc 1.99.0 (fixture)' 'binary: rustc' 'host: x86_64-unknown-linux-gnu'\n",
            encoding="utf-8",
        )
        rustc.chmod(0o700)
        if with_sccache:
            sccache = bin_dir / "sccache"
            sccache.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' 'Compile requests 3' 'Cache hits 1'\n",
                encoding="utf-8",
            )
            sccache.chmod(0o700)

        checkout = root / "checkout"
        checkout.mkdir()
        deps = root / "target" / "debug" / "deps"
        deps.mkdir(parents=True)
        artifact = deps / "libthegn_core-aaaa.rlib"
        artifact.write_bytes(b"fixture-rlib")
        if foreign:
            lib_path = "/elsewhere/wt-b/crates/thegn-core/src/lib.rs"
        else:
            lib_path = str(checkout / "crates" / "thegn-core" / "src" / "lib.rs")
        depinfo = deps / "thegn_core-aaaa.d"
        depinfo.write_text(f"{lib_path}: {lib_path} /x/other.rs\n", encoding="utf-8")
        return {
            "bin_dir": bin_dir,
            "checkout": checkout,
            "target": root / "target",
            "artifact": artifact,
            "deps": deps,
        }

    def facts_for(self, fixture, *args, env_extra=None):
        result = run(
            ["--checkout", str(fixture["checkout"]), *args],
            fixture["bin_dir"],
            env_extra,
        )
        return result, parse_output(result.stdout)

    def test_reports_toolchain_wrapper_and_target(self):
        fixture = self.fixture()
        target = fixture["target"] / "chosen"
        result, facts = self.facts_for(
            fixture,
            "--target-dir",
            str(target),
            env_extra={"RUSTFLAGS": "-C debuginfo=0"},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(facts["provenance.rustc_vv.1"], "rustc 1.99.0 (fixture)")
        self.assertEqual(facts["provenance.rustc_wrapper"], "none")
        self.assertEqual(facts["provenance.rustflags"], "-C debuginfo=0")
        self.assertEqual(facts["provenance.target_dir"], str(target))
        self.assertEqual(facts["provenance.cargo_target_dir_env"], "unset")
        self.assertTrue(
            facts.get("provenance.sccache") == "absent"
            or any(key.startswith("provenance.sccache.") for key in facts)
        )

        wrapped, wrapped_facts = self.facts_for(
            fixture,
            "--target-dir",
            str(target),
            env_extra={"RUSTC_WRAPPER": "sccache"},
        )
        self.assertEqual(wrapped.returncode, 0, wrapped.stderr)
        self.assertEqual(wrapped_facts["provenance.rustc_wrapper"], "sccache")

        build_wrapper, build_wrapper_facts = self.facts_for(
            fixture,
            "--target-dir",
            str(target),
            env_extra={"CARGO_BUILD_RUSTC_WRAPPER": "fixture-wrapper"},
        )
        self.assertEqual(build_wrapper.returncode, 0, build_wrapper.stderr)
        self.assertEqual(build_wrapper_facts["provenance.rustc_wrapper"], "fixture-wrapper")

    def test_sccache_stats_are_listed(self):
        fixture = self.fixture(with_sccache=True)
        result, facts = self.facts_for(fixture, "--target-dir", str(fixture["target"]))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(facts["provenance.sccache.1"], "Compile requests 3")
        self.assertEqual(facts["provenance.sccache.2"], "Cache hits 1")

    def test_matching_depinfo_is_ok(self):
        fixture = self.fixture()
        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "yes")
        self.assertEqual(facts["provenance.verdict"], "ok")

    def test_foreign_depinfo_is_flagged_and_strict_fails(self):
        fixture = self.fixture(foreign=True)
        normal, normal_facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"])
        )
        self.assertEqual(normal.returncode, 0, normal.stderr)
        self.assertEqual(normal_facts["provenance.core_depinfo_matches_checkout"], "no")
        self.assertEqual(normal_facts["provenance.verdict"], "foreign-depinfo")

        strict, strict_facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(strict.returncode, 1, strict.stderr)
        self.assertEqual(strict_facts["provenance.verdict"], "foreign-depinfo")

    def test_artifact_hash_is_the_file_sha256(self):
        if shutil.which("sha256sum") is None and shutil.which("shasum") is None:
            self.skipTest("neither sha256sum nor shasum is available")
        fixture = self.fixture()
        result, facts = self.facts_for(fixture, "--target-dir", str(fixture["target"]))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            facts["provenance.core_artifact_sha256"],
            hashlib.sha256(b"fixture-rlib").hexdigest(),
        )

    def test_missing_target_dir_reports_without_failing(self):
        fixture = self.fixture()
        missing = fixture["target"] / "missing"
        result, facts = self.facts_for(fixture, "--target-dir", str(missing))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(facts["provenance.core_artifact"], "none")
        self.assertEqual(facts["provenance.core_depinfo"], "none")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "unknown")
        self.assertEqual(facts["provenance.verdict"], "unknown")

    def test_usage_errors_exit_2(self):
        fixture = self.fixture()
        for args in (("--bogus",), ("--target-dir",), ("--target-dir", "--strict")):
            with self.subTest(args=args):
                result = run(list(args), fixture["bin_dir"])
                self.assertEqual(result.returncode, 2)
                self.assertEqual(len(result.stderr.splitlines()), 1)

    def test_script_is_read_only(self):
        fixture = self.fixture()

        def target_snapshot():
            files = sorted(path for path in fixture["target"].rglob("*") if path.is_file())
            return [
                (str(path.relative_to(fixture["target"])), path.read_bytes())
                for path in files
            ]

        before = target_snapshot()
        result, _facts = self.facts_for(fixture, "--target-dir", str(fixture["target"]))
        after = target_snapshot()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(after, before)


if __name__ == "__main__":
    unittest.main()
