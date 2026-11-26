#!/usr/bin/env python3
"""Hermetic regression tests for the third-party notices generator."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import random
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "third_party_notices.py"
SPEC = importlib.util.spec_from_file_location("third_party_notices", SCRIPT)
notices = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(notices)


def fixture_metadata():
    workspace_id = "path+file:///w/crates/thegn-core#0.1.0"
    return {
        "workspace_members": [workspace_id],
        "packages": [
            {"id": workspace_id, "name": "thegn-core", "version": "0.1.0", "license": "MIT", "repository": None},
            {"id": "registry+serde#1.0.200", "name": "serde", "version": "1.0.200", "license": "MIT OR Apache-2.0", "repository": "https://github.com/serde-rs/serde"},
            {"id": "registry+serde#1.0.9", "name": "serde", "version": "1.0.9", "license": "MIT OR Apache-2.0", "repository": "https://github.com/serde-rs/serde"},
            {"id": "registry+zlib-rs#0.4.0", "name": "zlib-rs", "version": "0.4.0", "license": None, "license_file": "LICENSE", "repository": None},
            {"id": "registry+mystery#2.0.0", "name": "mystery", "version": "2.0.0", "license": None, "license_file": None, "repository": "https://x.example/a b"},
            {"id": "path+file:///w/vendor/termwiz#0.22.0", "name": "termwiz", "version": "0.22.0", "source": None, "license": "MIT", "repository": "https://github.com/wezterm/wezterm"},
            {"id": "registry+zeta#1.0.0", "name": "Zeta", "version": "1.0.0", "license": "ISC", "repository": None},
        ],
    }


class ThirdPartyNoticesTests(unittest.TestCase):
    def test_workspace_members_are_excluded(self):
        packages = notices.third_party_packages(fixture_metadata())

        self.assertNotIn("thegn-core", [item["name"] for item in packages])

    def test_vendored_path_dependency_is_listed(self):
        packages = notices.third_party_packages(fixture_metadata())

        self.assertIn("termwiz", [item["name"] for item in packages])

    def test_license_fallbacks(self):
        packages = notices.third_party_packages(fixture_metadata())
        by_name = {item["name"]: item for item in packages}

        self.assertEqual(by_name["zlib-rs"]["license"], "see LICENSE")
        self.assertEqual(by_name["mystery"]["license"], "UNKNOWN")

    def test_order_is_case_insensitive_and_version_aware(self):
        packages = notices.third_party_packages(fixture_metadata())

        self.assertEqual(
            [(item["name"], item["version"]) for item in packages],
            [
                ("mystery", "2.0.0"),
                ("serde", "1.0.9"),
                ("serde", "1.0.200"),
                ("termwiz", "0.22.0"),
                ("Zeta", "1.0.0"),
                ("zlib-rs", "0.4.0"),
            ],
        )

    def test_render_format_is_exact(self):
        packages = notices.third_party_packages(fixture_metadata())
        expected_crates = """## Crates (6)

### `ISC` (1)

- `Zeta` `1.0.0`, no repository listed

### `MIT` (1)

- `termwiz` `0.22.0`, <https://github.com/wezterm/wezterm>

### `MIT OR Apache-2.0` (2)

- `serde` `1.0.9`, <https://github.com/serde-rs/serde>
- `serde` `1.0.200`, <https://github.com/serde-rs/serde>

### `see LICENSE` (1)

- `zlib-rs` `0.4.0`, no repository listed

### `UNKNOWN` (1)

- `mystery` `2.0.0`, no repository listed"""

        self.assertEqual("\n".join(notices.render_crates(packages)), expected_crates)
        rendered = notices.render(packages)
        self.assertTrue(rendered.startswith("# Third-party notices\n\n"))
        self.assertIn(expected_crates + "\n\n", rendered)
        self.assertTrue(rendered.endswith("\n"))
        self.assertFalse(rendered.endswith("\n\n"))
        self.assertFalse(any(line.endswith(" ") for line in rendered.splitlines()))
        self.assertNotIn("\t", rendered)

    def test_render_is_deterministic_under_input_shuffle(self):
        packages = notices.third_party_packages(fixture_metadata())
        expected = notices.render(packages)
        rng = random.Random(7)

        for _ in range(3):
            shuffled = list(packages)
            rng.shuffle(shuffled)
            self.assertEqual(notices.render(shuffled), expected)

    def test_check_mode_detects_stale_file_without_writing(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            metadata_path = directory / "metadata.json"
            output_path = directory / "notices.md"
            metadata_path.write_text(json.dumps(fixture_metadata()), encoding="utf-8")
            output_path.write_text("stale contents\n", encoding="utf-8")
            original = output_path.read_bytes()
            stderr = io.StringIO()

            with contextlib.redirect_stderr(stderr):
                self.assertEqual(notices.main(["--metadata", str(metadata_path), "--output", str(output_path), "--check"]), 1)

            self.assertIn("stale", stderr.getvalue())
            self.assertEqual(output_path.read_bytes(), original)
            self.assertEqual(notices.main(["--metadata", str(metadata_path), "--output", str(output_path)]), 0)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                result = notices.main(["--metadata", str(metadata_path), "--output", str(output_path), "--check"])
            self.assertEqual(result, 0)
            self.assertEqual(stdout.getvalue(), "THIRD_PARTY_NOTICES.md is up to date\n")

    def test_missing_metadata_file_is_a_usage_error(self):
        stderr = io.StringIO()

        with contextlib.redirect_stderr(stderr):
            result = notices.main(["--metadata", "/nonexistent/x.json"])

        self.assertEqual(result, 2)
        self.assertIn("cannot read cargo metadata", stderr.getvalue())

    def test_script_never_shells_out_when_metadata_is_given(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            metadata_path = directory / "metadata.json"
            output_path = directory / "notices.md"
            metadata_path.write_text(json.dumps(fixture_metadata()), encoding="utf-8")

            with mock.patch.object(notices.subprocess, "run") as run:
                self.assertEqual(notices.main(["--metadata", str(metadata_path), "--output", str(output_path)]), 0)

            run.assert_not_called()

    def test_committed_file_is_prettier_safe_shape(self):
        output_path = ROOT / "THIRD_PARTY_NOTICES.md"
        if not output_path.exists():
            self.skipTest("generated notices file is not available")
        content = output_path.read_text(encoding="utf-8")
        lines = content.splitlines()

        self.assertTrue(content.endswith("\n"))
        self.assertFalse(content.endswith("\n\n"))
        self.assertFalse(any(line.rstrip() != line for line in lines))
        self.assertNotIn("\t", content)
        self.assertNotIn("\n\n\n", content)
        start = lines.index(next(line for line in lines if line.startswith("## Crates (")))
        end = next(i for i in range(start + 1, len(lines)) if lines[i].startswith("## "))
        crate_lines = lines[start:end]
        expected_count = int(crate_lines[0].removeprefix("## Crates (").removesuffix(")"))
        self.assertEqual(sum(line.startswith("- `") for line in crate_lines), expected_count)

    def test_extra_sections_are_part_of_the_render(self):
        rendered = notices.render([])

        self.assertIn("## Vendored source", rendered)
        self.assertIn("## NOTICE texts", rendered)
        self.assertIn("`vendor/termwiz`", rendered)

    def test_lock_packages_read_licences_from_the_registry_cache(self):
        with tempfile.TemporaryDirectory() as temporary:
            registry = Path(temporary)
            crate = registry / "serde-1.0.200"
            crate.mkdir()
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "serde"\nversion = "1.0.200"\nlicense = "MIT OR Apache-2.0"\n'
                'repository = "https://github.com/serde-rs/serde"\n',
                encoding="utf-8",
            )
            lock = (
                '[[package]]\nname = "serde"\nversion = "1.0.200"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n\n'
                '[[package]]\nname = "thegn-core"\nversion = "0.1.0"\n\n'
                '[[package]]\nname = "ghost"\nversion = "1.0.0"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
            )

            packages = notices.lock_packages(lock, [registry], workspace_vendored=())

        by_name = {item["name"]: item for item in packages}
        self.assertEqual(sorted(by_name), ["ghost", "serde"])
        self.assertEqual(by_name["serde"]["license"], "MIT OR Apache-2.0")
        self.assertEqual(by_name["ghost"]["license"], "UNKNOWN")

if __name__ == "__main__":
    unittest.main()
