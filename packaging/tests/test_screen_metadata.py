from __future__ import annotations

import contextlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "screen_metadata", ROOT / "packaging" / "screen_metadata.py"
)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load packaging/screen_metadata.py")
screen_metadata = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(screen_metadata)


class ScreenMetadataTests(unittest.TestCase):
    def test_accepts_the_shipped_templates(self) -> None:
        for path in (
            ROOT / "packaging" / "homebrew" / "thegn.rb.tmpl",
            ROOT / "packaging" / "aur" / "PKGBUILD.tmpl",
        ):
            with self.subTest(path=path.name):
                self.assertIsNone(screen_metadata.screen(path.read_bytes()))

    def test_canary_command_substitution_is_screened_without_execution(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            canary = root / "executed"
            path = root / "PKGBUILD"
            path.write_text(
                f'pkgver=$(touch "{canary}")\n'
                f'function f {{ touch "{canary}"; }}; f\n',
                encoding="utf-8",
            )
            with contextlib.redirect_stdout(io.StringIO()):
                result = screen_metadata.main([str(path)])
            self.assertEqual(0, result)
            self.assertFalse(canary.exists(), "screening must never execute metadata")

    def test_refuses_oversized_metadata(self) -> None:
        data = b"x" * (screen_metadata.MAX_METADATA_BYTES + 1)
        reason = screen_metadata.screen(data)
        self.assertIsNotNone(reason)
        self.assertIn(str(screen_metadata.MAX_METADATA_BYTES + 1), reason)

    def test_refuses_empty_metadata(self) -> None:
        self.assertIn("empty", screen_metadata.screen(b""))

    def test_refuses_control_and_format_characters(self) -> None:
        for data, codepoint, byte_offset in (
            (b"x\x00y", "U+0000", 1),
            (b"x\ry", "U+000D", 1),
            (b"x\x1by", "U+001B", 1),
            (b"x\x7fy", "U+007F", 1),
            ("x\u0085y".encode(), "U+0085", 1),
            ("x\u202ey".encode(), "U+202E", 1),
            ("x\u200by".encode(), "U+200B", 1),
            ("x\ufeffy".encode(), "U+FEFF", 1),
            ("é\u202ey".encode(), "U+202E", 2),
        ):
            with self.subTest(codepoint=codepoint):
                reason = screen_metadata.screen(data)
                self.assertIsNotNone(reason)
                self.assertIn(codepoint, reason)
                self.assertIn(f"byte offset {byte_offset}", reason)

    def test_refuses_invalid_utf8(self) -> None:
        self.assertIn("UTF-8", screen_metadata.screen(b"valid\xff"))

    def test_refuses_symlinked_input(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            target = root / "metadata"
            target.write_text("valid metadata\n", encoding="utf-8")
            link = root / "linked-metadata"
            link.symlink_to(target)
            error_output = io.StringIO()
            with contextlib.redirect_stderr(error_output):
                result = screen_metadata.main([str(link)])
            self.assertEqual(1, result)
            self.assertIn("regular file", error_output.getvalue())


if __name__ == "__main__":
    unittest.main()
