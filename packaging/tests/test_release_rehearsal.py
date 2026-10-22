from __future__ import annotations

import contextlib
import importlib.util
import io
import struct
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "packaging" / "release_rehearsal.py"
SPEC = importlib.util.spec_from_file_location("thegn_release_rehearsal", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
rehearsal = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = rehearsal
SPEC.loader.exec_module(rehearsal)


class ElfFixture:
    def make_elf(
        self,
        interpreter: bytes | None,
        *,
        elf_class: int = 2,
        byte_order: int = 1,
        program_headers: bool = True,
        truncated_header: bool = False,
    ) -> Path:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        path = Path(temporary.name) / "fixture"
        header = bytearray(64)
        header[:4] = b"\x7fELF"
        header[4] = elf_class
        header[5] = byte_order
        struct.pack_into("<Q", header, 32, 64)
        struct.pack_into("<H", header, 54, 56)
        struct.pack_into("<H", header, 56, 1 if program_headers else 0)
        if not program_headers or truncated_header:
            path.write_bytes(header)
            return path

        segment = interpreter if interpreter is not None else b""
        program_header = bytearray(56)
        struct.pack_into("<I", program_header, 0, 3 if interpreter is not None else 1)
        struct.pack_into("<Q", program_header, 8, 120)
        struct.pack_into("<Q", program_header, 32, len(segment))
        path.write_bytes(header + program_header + segment)
        return path


class ElfInterpreterTests(ElfFixture, unittest.TestCase):
    def test_nix_store_interpreter_is_read(self) -> None:
        path = self.make_elf(b"/nix/store/abc-glibc/lib/ld-linux-x86-64.so.2\0")
        self.assertEqual(
            "/nix/store/abc-glibc/lib/ld-linux-x86-64.so.2",
            rehearsal.elf_interpreter(path),
        )

    def test_system_interpreter_is_read(self) -> None:
        path = self.make_elf(b"/lib64/ld-linux-x86-64.so.2\0")
        self.assertEqual("/lib64/ld-linux-x86-64.so.2", rehearsal.elf_interpreter(path))

    def test_static_binary_has_no_interpreter(self) -> None:
        path = self.make_elf(None, program_headers=False)
        self.assertIsNone(rehearsal.elf_interpreter(path))

    def test_non_elf_returns_none(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        path = Path(temporary.name) / "macho"
        path.write_bytes(b"\xcf\xfa\xed\xfe")
        self.assertIsNone(rehearsal.elf_interpreter(path))

    def test_elf32_is_refused(self) -> None:
        path = self.make_elf(None, elf_class=1)
        with self.assertRaisesRegex(
            rehearsal.RehearsalError, "unsupported ELF class/byte order"
        ):
            rehearsal.elf_interpreter(path)

    def test_big_endian_is_refused(self) -> None:
        path = self.make_elf(None, byte_order=2)
        with self.assertRaisesRegex(
            rehearsal.RehearsalError, "unsupported ELF class/byte order"
        ):
            rehearsal.elf_interpreter(path)

    def test_truncated_program_header_is_refused(self) -> None:
        path = self.make_elf(None, truncated_header=True)
        with self.assertRaisesRegex(rehearsal.RehearsalError, "malformed ELF"):
            rehearsal.elf_interpreter(path)

    def test_interpreter_without_nul_is_refused(self) -> None:
        path = self.make_elf(b"/lib64/ld-linux-x86-64.so.2")
        with self.assertRaisesRegex(rehearsal.RehearsalError, "NUL-terminated"):
            rehearsal.elf_interpreter(path)


class HostOnlyFindingTests(ElfFixture, unittest.TestCase):
    def test_nix_store_interpreter_is_a_finding(self) -> None:
        path = self.make_elf(b"/nix/store/abc-glibc/lib/ld-linux-x86-64.so.2\0")
        findings = rehearsal.host_only_findings(path)
        self.assertEqual(1, len(findings))
        self.assertIn(
            "ELF interpreter /nix/store/abc-glibc/lib/ld-linux-x86-64.so.2",
            findings[0],
        )
        self.assertIn("ubuntu-latest", findings[0])

    def test_system_interpreter_is_clean(self) -> None:
        path = self.make_elf(b"/lib64/ld-linux-x86-64.so.2\0")
        self.assertEqual([], rehearsal.host_only_findings(path))

    def test_check_binary_exits_1_without_host_only(self) -> None:
        path = self.make_elf(b"/nix/store/abc-glibc/lib/ld-linux-x86-64.so.2\0")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = rehearsal.main(["check-binary", "--binary", str(path)])
        self.assertEqual(1, result)
        self.assertIn("release-rehearsal:", stderr.getvalue())
        self.assertNotIn("WARNING (host-only rehearsal)", stderr.getvalue())
        self.assertIn(
            "rerun with 'just release-verify <tag> host_only=1' to rehearse the archive anyway",
            stderr.getvalue(),
        )

    def test_check_binary_warns_and_exits_0_with_host_only(self) -> None:
        path = self.make_elf(b"/nix/store/abc-glibc/lib/ld-linux-x86-64.so.2\0")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = rehearsal.main(["check-binary", "--binary", str(path), "--host-only"])
        self.assertEqual(0, result)
        self.assertIn("WARNING (host-only rehearsal)", stderr.getvalue())

    def test_check_binary_clean_exits_0(self) -> None:
        path = self.make_elf(b"/lib64/ld-linux-x86-64.so.2\0")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = rehearsal.main(["check-binary", "--binary", str(path)])
        self.assertEqual(0, result)
        self.assertEqual("", stderr.getvalue())


class ManagerForTargetTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.manifest = rehearsal.release.load_manifest()

    def test_linux_gnu_maps_to_nfpm(self) -> None:
        self.assertEqual(
            "nfpm",
            rehearsal.manager_for_target(self.manifest, "x86_64-unknown-linux-gnu"),
        )

    def test_linux_musl_maps_to_aur(self) -> None:
        self.assertEqual(
            "aur",
            rehearsal.manager_for_target(self.manifest, "x86_64-unknown-linux-musl"),
        )

    def test_darwin_maps_to_homebrew(self) -> None:
        self.assertEqual(
            "homebrew",
            rehearsal.manager_for_target(self.manifest, "aarch64-apple-darwin"),
        )

    def test_windows_target_has_no_enabled_manager(self) -> None:
        with self.assertRaisesRegex(rehearsal.RehearsalError, "x86_64-pc-windows-msvc"):
            rehearsal.manager_for_target(self.manifest, "x86_64-pc-windows-msvc")

    def test_unknown_target_is_refused(self) -> None:
        with self.assertRaisesRegex(rehearsal.RehearsalError, "unknown-target"):
            rehearsal.manager_for_target(self.manifest, "unknown-target")

    def test_duplicate_target_is_refused(self) -> None:
        manifest = {
            "managers": {
                "first": {"enabled": True, "target": "fixture-target"},
                "second": {"enabled": True, "target": "fixture-target"},
            }
        }
        with self.assertRaisesRegex(rehearsal.RehearsalError, "more than one enabled manager"):
            rehearsal.manager_for_target(manifest, "fixture-target")
