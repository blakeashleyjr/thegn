"""Ratchet: docs, release workflow and caveats agree on one Homebrew tap."""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
TAP_REPO = "homebrew-thegn"
INSTALL = "brew install blakeashleyjr/thegn/thegn"
FILES = [
    "README.md",
    "RELEASING.md",
    "KNOWN_ISSUES.md",
    "docs/help/known-limitations.md",
    "docs/help/upgrading-and-uninstalling.md",
    ".github/workflows/release.yml",
    "packaging/homebrew/thegn.rb.tmpl",
    "packaging/release.py",
]


class HomebrewTapNameTest(unittest.TestCase):
    def test_no_alternate_tap_name(self):
        pattern = re.compile(r"homebrew-tap\b|blakeashleyjr/tap\b")
        for rel in FILES:
            text = (ROOT / rel).read_text()
            self.assertIsNone(pattern.search(text), f"{rel}: stray tap name")

    def test_install_command_is_single(self):
        pattern = re.compile(r"brew install (\S*/\S*/)?thegn")
        for rel in FILES:
            for match in pattern.finditer((ROOT / rel).read_text()):
                self.assertEqual(
                    INSTALL,
                    match.group(0) if match.group(1) else None,
                    f"{rel}: non-canonical install command {match.group(0)!r}",
                )

    def test_canonical_names_are_documented(self):
        self.assertIn(TAP_REPO, (ROOT / "RELEASING.md").read_text())
        self.assertIn(INSTALL, (ROOT / "README.md").read_text())
        self.assertIn(f"<owner>/{TAP_REPO}", (ROOT / ".github/workflows/release.yml").read_text())


if __name__ == "__main__":
    unittest.main()
