"""Licence ratchet for vendored and bundled third-party material.

Fails when a vendored directory has no licence file, when a vendored or bundled
component is missing from THIRD_PARTY_NOTICES.md, or when the committed notices
no longer list every registry crate in Cargo.lock.
"""

from __future__ import annotations

import re
import tomllib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
NOTICES = ROOT / "THIRD_PARTY_NOTICES.md"
LICENSE_NAME = re.compile(r"^(LICEN[CS]E|COPYING|UNLICENSE)", re.IGNORECASE)
# Markers that say "this file was copied from elsewhere".
BUNDLED_MARKER = re.compile(r"Vendored from|Adapted from|Ported from", re.IGNORECASE)
BUNDLED_ROOTS = ("config", "extensions", "examples", "nix")


def vendored_dirs():
    vendor = ROOT / "vendor"
    return sorted(path for path in vendor.iterdir() if path.is_dir()) if vendor.is_dir() else []


class ThirdPartyVendorRatchet(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.notices = NOTICES.read_text(encoding="utf-8")

    def test_every_vendored_directory_has_a_licence_file(self):
        for directory in vendored_dirs():
            names = [path.name for path in directory.iterdir() if path.is_file()]
            self.assertTrue(
                any(LICENSE_NAME.match(name) for name in names),
                f"{directory.relative_to(ROOT)} has no LICENSE/COPYING file",
            )

    def test_every_vendored_component_is_in_the_notices(self):
        for directory in vendored_dirs():
            relative = directory.relative_to(ROOT).as_posix()
            self.assertIn(
                f"`{relative}`",
                self.notices,
                f"{relative} is vendored but THIRD_PARTY_NOTICES.md does not mention it "
                "(add it to scripts/third_party_extra.md and run `just third-party-notices`)",
            )
            manifest = directory / "Cargo.toml"
            if manifest.is_file():
                package = tomllib.loads(manifest.read_text(encoding="utf-8")).get("package", {})
                self.assertIn(f"`{package['name']}` `{package['version']}`", self.notices)

    def test_files_marked_as_copied_are_in_the_notices(self):
        for root in BUNDLED_ROOTS:
            base = ROOT / root
            if not base.is_dir():
                continue
            for path in sorted(base.rglob("*")):
                if not path.is_file() or path.stat().st_size > 1_000_000:
                    continue
                try:
                    head = path.read_text(encoding="utf-8")[:600]
                except (UnicodeDecodeError, OSError):
                    continue
                if BUNDLED_MARKER.search(head):
                    relative = path.relative_to(ROOT).as_posix()
                    self.assertIn(
                        relative,
                        self.notices,
                        f"{relative} says it was copied from upstream but is not in "
                        "THIRD_PARTY_NOTICES.md",
                    )

    def test_notices_list_every_registry_crate_in_the_lock(self):
        lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
        missing = [
            f"{package['name']} {package['version']}"
            for package in lock["package"]
            if package.get("source") and f"`{package['name']}` `{package['version']}`" not in self.notices
        ]
        self.assertEqual(
            missing[:10],
            [],
            f"{len(missing)} crate(s) in Cargo.lock are missing from THIRD_PARTY_NOTICES.md; "
            "run `just third-party-notices` on the builder",
        )

    def test_no_crate_has_an_unknown_licence(self):
        self.assertNotIn("### `UNKNOWN`", self.notices)


if __name__ == "__main__":
    unittest.main()
