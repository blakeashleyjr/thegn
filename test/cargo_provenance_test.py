#!/usr/bin/env python3
"""Hermetic fixture tests for the opt-in Cargo provenance report."""

import hashlib
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "test" / "cargo-provenance.sh"


def run(args, bin_dir=None, env_extra=None, cwd=None, input_text=""):
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
            cwd=cwd,
            input=input_text,
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
        core_src = checkout / "crates" / "thegn-core" / "src"
        core_src.mkdir(parents=True)
        (core_src / "lib.rs").write_text("// fixture\n", encoding="utf-8")
        (core_src / "a.rs").write_text("// fixture\n", encoding="utf-8")
        deps = root / "target" / "debug" / "deps"
        deps.mkdir(parents=True)
        artifact = deps / "libthegn_core-aaaa.rlib"
        artifact.write_bytes(b"fixture-rlib")
        if foreign:
            lib_path = "/elsewhere/wt-b/crates/thegn-core/src/lib.rs"
            other_path = "/elsewhere/wt-b/crates/thegn-core/src/a.rs"
        else:
            lib_path = str(checkout / "crates" / "thegn-core" / "src" / "lib.rs")
            other_path = str(checkout / "crates" / "thegn-core" / "src" / "a.rs")
        depinfo = deps / "thegn_core-aaaa.d"
        depinfo.write_text(
            f"{lib_path}: {lib_path} {other_path} /x/other.rs\n", encoding="utf-8"
        )
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

    def test_matching_depinfo_via_temporary_checkout_alias_is_ok(self):
        fixture = self.fixture()
        alias = fixture["checkout"].parent / "checkout-alias"
        alias.symlink_to(fixture["checkout"], target_is_directory=True)
        depinfo = fixture["deps"] / "thegn_core-aaaa.d"
        lib_path = alias / "crates" / "thegn-core" / "src" / "lib.rs"
        other_path = alias / "crates" / "thegn-core" / "src" / "a.rs"
        depinfo.write_text(
            f"{lib_path}: {lib_path} {other_path} /x/other.rs\n", encoding="utf-8"
        )

        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "yes")
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "0")
        self.assertEqual(facts["provenance.verdict"], "ok")

    def test_missing_component_before_parent_traversal_is_refused(self):
        fixture = self.fixture()
        depinfo = fixture["deps"] / "thegn_core-aaaa.d"
        lib = fixture["checkout"] / "crates/thegn-core/src/lib.rs"
        source = fixture["checkout"] / "crates/thegn-core/src/a.rs"
        injected = fixture["checkout"] / "missing-prefix/../crates/thegn-core/src/lib.rs"
        depinfo.write_text(f"{lib}: {injected} {source}\n", encoding="utf-8")

        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "unknown")

    def test_untraversable_component_before_parent_traversal_is_refused(self):
        fixture = self.fixture()
        blocker = fixture["checkout"].parent / "blocked"
        blocker.mkdir()
        blocker.chmod(0)
        try:
            injected = blocker / "../checkout/crates/thegn-core/src/lib.rs"
            try:
                os.stat(injected)
            except PermissionError:
                # This branch exercises the DAC failure that real users see.
                pass
            else:
                # Root and processes with DAC-bypass capabilities can traverse
                # mode-000 directories. A regular file gives the kernel a
                # deterministic ENOTDIR check on the same `component/../` path.
                blocker.chmod(0o700)
                blocker.rmdir()
                blocker.write_text("not a directory", encoding="utf-8")
                injected = blocker / "../checkout/crates/thegn-core/src/lib.rs"
                with self.assertRaises(NotADirectoryError):
                    os.stat(injected)

            depinfo = fixture["deps"] / "thegn_core-aaaa.d"
            source = fixture["checkout"] / "crates/thegn-core/src/a.rs"
            valid_lib = fixture["checkout"] / "crates/thegn-core/src/lib.rs"
            depinfo.write_text(
                f"{valid_lib}: {injected} {source}\n", encoding="utf-8"
            )
            result, facts = self.facts_for(
                fixture, "--target-dir", str(fixture["target"]), "--strict"
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")
            self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "unknown")
        finally:
            blocker.chmod(0o700)

    def test_unresolved_checkout_cannot_be_counted_as_coverage(self):
        fixture = self.fixture()
        missing_checkout = fixture["checkout"].parent / "missing-checkout"
        result = run(
            ["--checkout", str(missing_checkout), "--target-dir", str(fixture["target"]), "--strict"],
            fixture["bin_dir"],
        )
        facts = parse_output(result.stdout)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")
        self.assertNotEqual(facts["provenance.core_depinfo_missing_sources"], "0")
        self.assertEqual(facts["provenance.verdict"], "foreign-depinfo")

    def test_escaped_depinfo_path_form_is_refused(self):
        fixture = self.fixture()
        depinfo = fixture["deps"] / "thegn_core-aaaa.d"
        lib_path = fixture["checkout"] / "crates/thegn-core/src/lib.rs"
        depinfo.write_text(
            f"{lib_path}: {lib_path} crates/thegn-core/src/a\\ file.rs\n",
            encoding="utf-8",
        )
        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")
        self.assertEqual(facts["provenance.verdict"], "foreign-depinfo")

    def test_source_enumeration_failure_cannot_be_zero_missing(self):
        fixture = self.fixture()
        result, facts = self.run_parser_with_injected_error(fixture, "walk")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "unknown")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")

    def test_comparison_failure_cannot_be_accepted_as_success(self):
        fixture = self.fixture()
        result, facts = self.run_parser_with_injected_error(fixture, "compare")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "unknown")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")

    def test_non_numeric_comparison_output_cannot_be_accepted(self):
        fixture = self.fixture()
        python = fixture["bin_dir"] / "python3"
        python.write_text("#!/bin/sh\nprintf 'not-a-count\\t0\\tlib.rs\\n'\n", encoding="utf-8")
        python.chmod(0o700)
        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertNotEqual(facts["provenance.core_depinfo_missing_sources"], "0")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")

    def run_parser_with_injected_error(self, fixture, boundary):
        driver = (
            "import importlib.util, os, sys\n"
            "sys.dont_write_bytecode = True\n"
            "from unittest import mock\n"
            "spec = importlib.util.spec_from_file_location('parser', sys.argv[1])\n"
            "module = importlib.util.module_from_spec(spec)\n"
            "spec.loader.exec_module(module)\n"
            "sys.argv = [sys.argv[1], *sys.argv[2:]]\n"
            "if os.environ['CARGO_PROVENANCE_INJECT'] == 'walk':\n"
            "    patch = mock.patch.object(module.os, 'walk', side_effect=OSError('controlled walk failure'))\n"
            "else:\n"
            "    patch = mock.patch.object(module, 'source_dependencies', side_effect=OSError('controlled comparison failure'))\n"
            "with patch:\n"
            "    raise SystemExit(module.main())\n"
        )
        python = fixture["bin_dir"] / "python3"
        python.write_text(
            f"#!/bin/sh\nexec {shlex.quote(sys.executable)} -c {shlex.quote(driver)} \"$@\"\n",
            encoding="utf-8",
        )
        python.chmod(0o700)
        result, facts = self.facts_for(
            fixture,
            "--target-dir",
            str(fixture["target"]),
            "--strict",
            env_extra={"CARGO_PROVENANCE_INJECT": boundary},
        )
        return result, facts

    def test_relative_cargo_depinfo_with_all_checkout_sources_is_ok(self):
        fixture = self.fixture()
        (fixture["deps"] / "thegn_core-aaaa.d").write_text(
            f"{fixture['deps'] / 'thegn_core-aaaa.d'}: "
            "crates/thegn-core/src/lib.rs crates/thegn-core/src/a.rs\n",
            encoding="utf-8",
        )
        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "yes")
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "0")
        self.assertEqual(facts["provenance.verdict"], "ok")

    def test_relative_depinfo_omitting_checkout_source_is_flagged(self):
        fixture = self.fixture()
        depinfo = fixture["deps"] / "thegn_core-aaaa.d"
        depinfo.write_text(
            f"{depinfo}: crates/thegn-core/src/lib.rs\n", encoding="utf-8"
        )
        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "1")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")
        self.assertEqual(facts["provenance.verdict"], "foreign-depinfo")

    def test_rustc_rules_count_prerequisites_and_ignore_dummy_targets(self):
        fixture = self.fixture()
        lib = fixture["checkout"] / "crates/thegn-core/src/lib.rs"
        source = fixture["checkout"] / "crates/thegn-core/src/a.rs"
        depinfo = fixture["deps"] / "thegn_core-aaaa.d"
        depinfo.write_text(
            f"{fixture['target']}/debug/deps/libthegn_core-aaaa.rlib: "
            f"{lib} {source} /rustc/library/core/src/lib.rs\n"
            f"{fixture['target']}/debug/deps/thegn_core-aaaa.d: "
            f"{lib} {source} /rustc/library/core/src/lib.rs\n"
            f"{lib}:\n{source}:\n",
            encoding="utf-8",
        )
        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "0")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "yes")

    def test_dummy_targets_without_prerequisite_rule_are_refused(self):
        fixture = self.fixture()
        lib = fixture["checkout"] / "crates/thegn-core/src/lib.rs"
        source = fixture["checkout"] / "crates/thegn-core/src/a.rs"
        (fixture["deps"] / "thegn_core-aaaa.d").write_text(
            f"{lib}:\n{source}:\n", encoding="utf-8"
        )
        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "unknown")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")

    def test_nul_depinfo_is_refused_before_text_parsing(self):
        fixture = self.fixture()
        lib = fixture["checkout"] / "crates/thegn-core/src/lib.rs"
        source = fixture["checkout"] / "crates/thegn-core/src/a.rs"
        (fixture["deps"] / "thegn_core-aaaa.d").write_bytes(
            f"{lib}: {lib} {str(source)[:-1]}\0.rs\n".encode()
        )
        result, facts = self.facts_for(
            fixture, "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "unknown")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")

    def test_relative_tmpdir_with_equals_and_empty_stdin_keeps_omission(self):
        fixture = self.fixture()
        depinfo = fixture["deps"] / "thegn_core-aaaa.d"
        depinfo.write_text(
            f"{depinfo}: crates/thegn-core/src/lib.rs\n", encoding="utf-8"
        )
        relative_tmpdir = fixture["checkout"].parent / "cache=tmp"
        relative_tmpdir.mkdir()
        result = run(
            ["--checkout", "checkout", "--target-dir", str(fixture["target"]), "--strict"],
            fixture["bin_dir"],
            {"TMPDIR": "cache=tmp"},
            cwd=fixture["checkout"].parent,
            input_text="",
        )
        facts = parse_output(result.stdout)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_missing_sources"], "1")
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")

    def test_cdpath_cannot_resolve_missing_relative_checkout(self):
        fixture = self.fixture()
        matching = run(
            ["--checkout", "checkout", "--target-dir", str(fixture["target"]), "--strict"],
            fixture["bin_dir"],
            {"CDPATH": "."},
            cwd=fixture["checkout"].parent,
        )
        matching_facts = parse_output(matching.stdout)
        self.assertEqual(matching.returncode, 0, matching.stderr)
        self.assertEqual(matching_facts["provenance.core_depinfo_matches_checkout"], "yes")

        search = fixture["checkout"].parent / "search"
        search.mkdir()
        searched_checkout = search / "checkout"
        shutil.copytree(fixture["checkout"], searched_checkout)
        lib = searched_checkout / "crates/thegn-core/src/lib.rs"
        source = searched_checkout / "crates/thegn-core/src/a.rs"
        (fixture["deps"] / "thegn_core-aaaa.d").write_text(
            f"{lib}: {lib} {source}\n", encoding="utf-8"
        )
        invocation = fixture["checkout"].parent / "invocation"
        invocation.mkdir()
        result = run(
            ["--checkout", "checkout", "--target-dir", str(fixture["target"]), "--strict"],
            fixture["bin_dir"],
            {"CDPATH": str(search)},
            cwd=invocation,
        )
        facts = parse_output(result.stdout)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")

    def test_newline_ending_checkout_is_not_confused_with_sibling(self):
        fixture = self.fixture()
        plain = fixture["checkout"]
        newline_checkout = plain.with_name(plain.name + "\n")
        shutil.copytree(plain, newline_checkout)
        depinfo = fixture["deps"] / "thegn_core-aaaa.d"
        lib = newline_checkout / "crates/thegn-core/src/lib.rs"
        source = newline_checkout / "crates/thegn-core/src/a.rs"
        depinfo.write_text(f"{lib}: {lib} {source}\n", encoding="utf-8")
        result, facts = self.facts_for(
            {**fixture, "checkout": newline_checkout},
            "--target-dir", str(fixture["target"]), "--strict"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(facts["provenance.core_depinfo_matches_checkout"], "no")

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

    def test_help_exits_zero(self):
        result = run(["--help"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Usage: test/cargo-provenance.sh", result.stdout)

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
