from __future__ import annotations

import copy
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import stat
import sys
import tarfile
import tempfile
import tomllib
import unittest
import zipfile
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
RELEASE_PATH = ROOT / "packaging" / "release.py"
SPEC = importlib.util.spec_from_file_location("thegn_release", RELEASE_PATH)
assert SPEC is not None and SPEC.loader is not None
release = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release
SPEC.loader.exec_module(release)


class ReleaseRendererTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.temp = Path(self.temporary.name)
        self.assets = self.temp / "assets"
        self.assets.mkdir()
        self.manifest = release.load_manifest()
        self.version = release.workspace_version()
        self.tag = f"v{self.version}"

    def make_asset(self, target: str, *, missing: str | None = None) -> tuple[Path, str]:
        archive_name, checksum_name = release._asset_names(self.manifest, self.tag, target)
        archive = self.assets / archive_name
        target_config = self.manifest["targets"][target]
        names = list(self.manifest["archive"]["root_files"])
        names[0] = target_config.get("binary", self.manifest["project"]["binary"])
        names = [name for name in names if name != missing]
        if target_config["platform"] == "unix":
            with tarfile.open(archive, "w:gz") as output:
                for name in names:
                    content = f"fixture:{name}\n".encode()
                    info = tarfile.TarInfo(name)
                    info.size = len(content)
                    info.mode = 0o755 if name == "thegn" else 0o644
                    info.mtime = 0
                    output.addfile(info, io.BytesIO(content))
        else:
            with zipfile.ZipFile(archive, "w") as output:
                for name in names:
                    output.writestr(name, f"fixture:{name}\n")
        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.assets / checksum_name).write_text(
            f"{checksum}  {archive_name}\n", encoding="utf-8"
        )
        return archive, checksum

    def make_active_assets(self) -> dict[str, str]:
        checksums: dict[str, str] = {}
        managers = release.selected_managers(self.manifest, None)
        for target in dict.fromkeys(
            self.manifest["managers"][manager]["target"] for manager in managers
        ):
            _, checksums[target] = self.make_asset(target)
        return checksums

    def write_tar_asset(
        self, entries: list[tuple[str, bytes, int]]
    ) -> tuple[Path, str]:
        target = "aarch64-apple-darwin"
        archive_name, checksum_name = release._asset_names(self.manifest, self.tag, target)
        archive_path = self.assets / archive_name
        with tarfile.open(archive_path, "w:gz") as output:
            for name, content, mode in entries:
                info = tarfile.TarInfo(name)
                info.size = len(content)
                info.mode = mode
                if name.endswith("/"):
                    info.type = tarfile.DIRTYPE
                output.addfile(info, io.BytesIO(content))
        checksum = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        (self.assets / checksum_name).write_text(
            f"{checksum}  {archive_name}\n", encoding="utf-8"
        )
        return archive_path, checksum

    def render(self, output: Path | None = None) -> Path:
        output = output or self.temp / "rendered"
        managers = release.selected_managers(self.manifest, None)
        assets = release.validate_assets(self.manifest, self.tag, self.assets, managers)
        files = release.rendered_files(
            self.manifest,
            release.PACKAGING_DIR,
            self.version,
            managers,
            assets,
        )
        release.atomic_write_output(output, files)
        return output

    def test_tag_must_exactly_match_workspace_version(self) -> None:
        release.require_matching_tag(self.tag, self.version)
        for bad in (self.version, "v0.1.0", f"v{self.version}.1", "v0.1.0-beta.1"):
            with self.subTest(tag=bad), self.assertRaisesRegex(
                release.ReleaseError, "does not match workspace version"
            ):
                release.require_matching_tag(bad, self.version)

    def test_renders_all_active_managers_deterministically(self) -> None:
        checksums = self.make_active_assets()
        first = self.render(self.temp / "first")
        second = self.render(self.temp / "second")
        expected = {
            Path("homebrew/thegn.rb"),
            Path("aur/PKGBUILD"),
            Path("nfpm/thegn-deb.yaml"),
            Path("nfpm/thegn-rpm.yaml"),
        }
        actual = {path.relative_to(first) for path in first.rglob("*") if path.is_file()}
        self.assertEqual(expected, actual)
        for relative in expected:
            self.assertEqual((first / relative).read_bytes(), (second / relative).read_bytes())

        formula = (first / "homebrew/thegn.rb").read_text(encoding="utf-8")
        self.assertIn(f'version "{self.version}"', formula)
        self.assertIn(checksums["aarch64-apple-darwin"], formula)
        self.assertIn('bin.install "thegn"', formula)
        self.assertIn('license any_of: ["MIT", "Apache-2.0"]', formula)
        self.assertIn('depends_on "gh" => :optional', formula)

        pkgbuild = (first / "aur/PKGBUILD").read_text(encoding="utf-8")
        self.assertIn(f"pkgver={release.arch_pkgver(self.version)}", pkgbuild)
        self.assertIn(f"/releases/download/{self.tag}/thegn-{self.tag}-", pkgbuild)
        self.assertIn(checksums["x86_64-unknown-linux-musl"], pkgbuild)
        self.assertIn("ln -s thegn", pkgbuild)
        self.assertNotIn("thegn-dev", pkgbuild)

        deb = (first / "nfpm/thegn-deb.yaml").read_text(encoding="utf-8")
        rpm = (first / "nfpm/thegn-rpm.yaml").read_text(encoding="utf-8")
        self.assertIn('arch: "amd64"', deb)
        self.assertIn('arch: "x86_64"', rpm)
        for text in (deb, rpm):
            self.assertIn("dst: /usr/bin/thegn", text)
            self.assertIn("dst: /usr/bin/tg", text)
            self.assertIn("LICENSE-MIT", text)
            self.assertIn("LICENSE-APACHE", text)
            self.assertIn("  - git", text)

    def test_rejects_missing_checksum(self) -> None:
        managers = ["homebrew"]
        archive, _ = self.make_asset("aarch64-apple-darwin")
        _, checksum_name = release._asset_names(
            self.manifest, self.tag, "aarch64-apple-darwin"
        )
        (self.assets / checksum_name).unlink()
        self.assertTrue(archive.exists())
        with self.assertRaisesRegex(release.ReleaseError, "missing release checksum"):
            release.validate_assets(self.manifest, self.tag, self.assets, managers)

    def test_rejects_non_lowercase_and_mismatched_checksums(self) -> None:
        _, checksum = self.make_asset("aarch64-apple-darwin")
        _, checksum_name = release._asset_names(
            self.manifest, self.tag, "aarch64-apple-darwin"
        )
        checksum_path = self.assets / checksum_name
        checksum_path.write_text(checksum.upper() + "\n", encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "lowercase 64-hex"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])
        checksum_path.write_text("0" * 64 + "\n", encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "checksum mismatch"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_archive_missing_required_root_file(self) -> None:
        self.make_asset("aarch64-apple-darwin", missing="LICENSE-APACHE")
        with self.assertRaisesRegex(release.ReleaseError, "missing root file.*LICENSE-APACHE"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_archive_link_used_as_required_file(self) -> None:
        target = "aarch64-apple-darwin"
        archive_name, checksum_name = release._asset_names(self.manifest, self.tag, target)
        archive_path = self.assets / archive_name
        with tarfile.open(archive_path, "w:gz") as output:
            for name in self.manifest["archive"]["root_files"]:
                info = tarfile.TarInfo(name)
                if name == "thegn":
                    info.type = tarfile.SYMTYPE
                    info.linkname = "/etc/passwd"
                    output.addfile(info)
                else:
                    content = f"fixture:{name}\n".encode()
                    info.size = len(content)
                    output.addfile(info, io.BytesIO(content))
        checksum = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        (self.assets / checksum_name).write_text(
            f"{checksum}  {archive_name}\n", encoding="utf-8"
        )

        with self.assertRaisesRegex(release.ReleaseError, "non-regular member.*thegn"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_duplicate_archive_paths(self) -> None:
        target = "aarch64-apple-darwin"
        archive_name, checksum_name = release._asset_names(self.manifest, self.tag, target)
        archive_path = self.assets / archive_name
        with tarfile.open(archive_path, "w:gz") as output:
            for name in [*self.manifest["archive"]["root_files"], "thegn"]:
                content = f"fixture:{name}\n".encode()
                info = tarfile.TarInfo(name)
                info.size = len(content)
                output.addfile(info, io.BytesIO(content))
        checksum = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        (self.assets / checksum_name).write_text(
            f"{checksum}  {archive_name}\n", encoding="utf-8"
        )

        with self.assertRaisesRegex(release.ReleaseError, "duplicate path 'thegn'"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_extra_archive_file_and_directory(self) -> None:
        entries = [
            (name, f"fixture:{name}\n".encode(), 0o755 if name == "thegn" else 0o644)
            for name in self.manifest["archive"]["root_files"]
        ]
        self.write_tar_asset([*entries, ("evil.sh", b"echo bad\n", 0o755)])
        with self.assertRaisesRegex(release.ReleaseError, "unexpected member.*evil.sh"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

        self.write_tar_asset([*entries, ("extra/", b"", 0o755)])
        with self.assertRaisesRegex(release.ReleaseError, "unexpected member.*extra"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_archive_member_count_over_limit(self) -> None:
        entries = [
            (name, f"fixture:{name}\n".encode(), 0o755 if name == "thegn" else 0o644)
            for name in self.manifest["archive"]["root_files"]
        ]
        entries.extend((f"extra-{index}", b"x", 0o644) for index in range(61))
        self.write_tar_asset(entries)
        with self.assertRaisesRegex(release.ReleaseError, "exceeds 64 members"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_oversized_archive_member_header(self) -> None:
        self.make_asset("aarch64-apple-darwin")
        with mock.patch.object(release, "MAX_MEMBER_BYTES", 8):
            with self.assertRaisesRegex(release.ReleaseError, "member exceeds 8 bytes"):
                release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_non_executable_unix_binary(self) -> None:
        entries = [
            (name, f"fixture:{name}\n".encode(), 0o644)
            for name in self.manifest["archive"]["root_files"]
        ]
        self.write_tar_asset(entries)
        with self.assertRaisesRegex(release.ReleaseError, "binary thegn is not executable"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_high_compression_ratio_archive(self) -> None:
        entries = [
            (
                name,
                b"\0" * (1024 * 1024) if name == "README.md" else f"fixture:{name}\n".encode(),
                0o755 if name == "thegn" else 0o644,
            )
            for name in self.manifest["archive"]["root_files"]
        ]
        self.write_tar_asset(entries)
        with self.assertRaisesRegex(release.ReleaseError, "exceeds compression ratio"):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_rejects_unsafe_member_modes(self) -> None:
        for unsafe_name, unsafe_mode in (("thegn", 0o4755), ("README.md", 0o666)):
            with self.subTest(member=unsafe_name, mode=oct(unsafe_mode)):
                entries = [
                    (
                        name,
                        f"fixture:{name}\n".encode(),
                        unsafe_mode
                        if name == unsafe_name
                        else (0o755 if name == "thegn" else 0o644),
                    )
                    for name in self.manifest["archive"]["root_files"]
                ]
                self.write_tar_asset(entries)
                with self.assertRaisesRegex(release.ReleaseError, "unsafe mode"):
                    release.validate_assets(
                        self.manifest, self.tag, self.assets, ["homebrew"]
                    )

    def test_accepts_ordinary_member_modes(self) -> None:
        # Positive control: this fixture also validates on the base branch.
        self.make_asset("aarch64-apple-darwin")
        release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew"])

    def test_inactive_windows_managers_fail_clearly(self) -> None:
        for manager in ("scoop", "winget"):
            with self.subTest(manager=manager), self.assertRaisesRegex(
                release.ReleaseError, release.WINDOWS_ERROR
            ):
                release.selected_managers(self.manifest, [manager])

    def test_enabled_windows_asset_can_render_both_manifests(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["targets"]["x86_64-pc-windows-msvc"]["enabled"] = True
        manifest["managers"]["scoop"]["enabled"] = True
        manifest["managers"]["winget"]["enabled"] = True
        self.manifest = manifest
        _, checksum = self.make_asset("x86_64-pc-windows-msvc")
        managers = release.selected_managers(manifest, ["scoop", "winget"])
        assets = release.validate_assets(manifest, self.tag, self.assets, managers)
        files = release.rendered_files(
            manifest, release.PACKAGING_DIR, self.version, managers, assets
        )
        self.assertEqual({Path("scoop/thegn.json"), Path("winget/thegn.yaml")}, set(files))
        self.assertIn(checksum, files[Path("scoop/thegn.json")])
        self.assertIn(checksum.upper(), files[Path("winget/thegn.yaml")])

    def test_failed_validation_preserves_existing_output(self) -> None:
        output = self.temp / "output"
        output.mkdir()
        marker = output / "owned-by-caller"
        marker.write_text("keep\n", encoding="utf-8")
        self.make_asset("aarch64-apple-darwin")
        with self.assertRaises(release.ReleaseError):
            release.validate_assets(self.manifest, self.tag, self.assets, ["homebrew", "aur"])
        self.assertEqual("keep\n", marker.read_text(encoding="utf-8"))
        self.assertEqual([marker], list(output.iterdir()))

    def test_failed_staged_render_preserves_existing_output(self) -> None:
        output = self.temp / "output"
        output.mkdir()
        marker = output / "owned-by-caller"
        marker.write_text("keep\n", encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "failed final validation"):
            release.atomic_write_output(output, {Path("bad.txt"): "@@UNKNOWN@@\n"})
        self.assertEqual("keep\n", marker.read_text(encoding="utf-8"))
        self.assertEqual([marker], list(output.iterdir()))

    def test_success_replaces_existing_output_only_after_staging(self) -> None:
        self.make_active_assets()
        output = self.temp / "output"
        output.mkdir()
        (output / "stale").write_text("stale\n", encoding="utf-8")
        self.render(output)
        self.assertFalse((output / "stale").exists())
        self.assertTrue((output / "homebrew/thegn.rb").is_file())

    def test_rejects_symlink_output_without_replacing_target(self) -> None:
        target = self.temp / "caller-owned"
        target.mkdir()
        marker = target / "marker"
        marker.write_text("keep\n", encoding="utf-8")
        output = self.temp / "rendered"
        output.symlink_to(target, target_is_directory=True)

        with self.assertRaisesRegex(release.ReleaseError, "must not be a symlink"):
            release.atomic_write_output(output, {Path("ok.txt"): "ok\n"})

        self.assertTrue(output.is_symlink())
        self.assertEqual("keep\n", marker.read_text(encoding="utf-8"))

    def test_template_rejects_unknown_placeholder(self) -> None:
        template = self.temp / "bad.tmpl"
        template.write_text("value=@@NOT_A_RENDER_VALUE@@\n", encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "unknown placeholder"):
            release._render_template(template, {})

    def test_manifest_rejects_output_path_traversal(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["managers"]["aur"]["output"] = "../PKGBUILD"
        with self.assertRaisesRegex(release.ReleaseError, "safe relative path"):
            release._validate_manifest(manifest, release.PACKAGING_DIR)

    def test_binstall_metadata_matches_archive_contract(self) -> None:
        host = tomllib.loads(
            (ROOT / "crates/thegn-host/Cargo.toml").read_text(encoding="utf-8")
        )
        binstall = host["package"]["metadata"]["binstall"]
        self.assertEqual(
            "{ repo }/releases/download/v{ version }/thegn-v{ version }-{ target }.{ archive-format }",
            binstall["pkg-url"],
        )
        self.assertEqual("tgz", binstall["pkg-fmt"])
        self.assertEqual("thegn{ binary-ext }", binstall["bin-dir"])
        windows = binstall["overrides"]["x86_64-pc-windows-msvc"]
        self.assertEqual("zip", windows["pkg-fmt"])
        self.assertEqual("tar.gz", self.manifest["archive"]["formats"]["unix"])
        self.assertEqual("zip", self.manifest["archive"]["formats"]["windows"])

    def test_templates_contain_no_release_specific_state(self) -> None:
        version = release.workspace_version()
        for config in self.manifest["managers"].values():
            path = release.PACKAGING_DIR / config["template"]
            text = path.read_text(encoding="utf-8")
            with self.subTest(path=path):
                self.assertNotIn(version, text)
                self.assertNotIn(f"v{version}", text)
                self.assertNotIn("REPLACE_WITH_", text)
                self.assertNotRegex(text, r"[0-9a-f]{64}")
                self.assertNotIn("secret", text.lower())


class ReleaseManifestTests(unittest.TestCase):
    make_asset = ReleaseRendererTests.make_asset

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.temp = Path(self.temporary.name)
        self.assets = self.temp / "assets"
        self.assets.mkdir()
        self.manifest = release.load_manifest()
        self.version = release.workspace_version()
        self.tag = f"v{self.version}"
        self.commit = "a" * 40
        self.record_path = self.assets / f"thegn-{self.tag}-release.json"
        self.expected = release.expected_release_assets(self.manifest, self.tag)
        archive_names: list[str] = []
        for target, config in self.manifest["targets"].items():
            if config["enabled"]:
                archive_name, checksum_name = release._asset_names(
                    self.manifest, self.tag, target
                )
                self.make_asset(target)
                archive_names.extend((archive_name, checksum_name))
        completions = f"thegn-{self.tag}-completions.tar.gz"
        completions_checksum = f"{completions}.sha256"
        (self.assets / completions).write_bytes(b"completions fixture")
        completions_digest = hashlib.sha256((self.assets / completions).read_bytes()).hexdigest()
        (self.assets / completions_checksum).write_text(
            f"{completions_digest}  {completions}\n", encoding="utf-8"
        )
        archive_names.extend((completions, completions_checksum))
        for name in self.expected:
            if name not in archive_names and name != self.record_path.name:
                (self.assets / name).write_bytes(f"fixture:{name}".encode())
        self.write_record(archive_names)

    def write_record(self, names: list[str] | None = None, *, commit: str | None = None) -> None:
        names = names or [
            name
            for name in self.expected
            if name.endswith((".tar.gz", ".sha256"))
        ]
        record = {
            "tag": self.tag,
            "tag_object": "b" * 40,
            "commit": commit or self.commit,
            "workflow_sha": "c" * 40,
            "run_id": "12345",
            "assets": [
                {"name": name, "sha256": release._sha256(self.assets / name)}
                for name in names
            ],
        }
        self.record_path.write_text(json.dumps(record), encoding="utf-8")

    def build(self) -> dict:
        return release.build_release_manifest(
            self.manifest, self.tag, self.commit, self.assets, self.record_path
        )

    def test_manifest_accepts_the_exact_asset_set(self) -> None:
        result = self.build()
        self.assertEqual(self.expected, [asset["name"] for asset in result["assets"]])
        for asset in result["assets"]:
            path = self.assets / asset["name"]
            self.assertEqual(release._sha256(path), asset["sha256"])
            self.assertEqual(path.stat().st_size, asset["size"])

    def test_manifest_rejects_a_missing_completions_archive(self) -> None:
        (self.assets / f"thegn-{self.tag}-completions.tar.gz").unlink()
        with self.assertRaisesRegex(release.ReleaseError, "missing:.*completions"):
            self.build()

    def test_manifest_rejects_a_missing_matrix_leg(self) -> None:
        for name in release._asset_names(self.manifest, self.tag, "aarch64-apple-darwin"):
            (self.assets / name).unlink()
        with self.assertRaisesRegex(release.ReleaseError, "missing:.*aarch64-apple-darwin"):
            self.build()

    def test_manifest_rejects_an_extra_asset(self) -> None:
        (self.assets / f"thegn-{self.tag}-stale.tar.gz").write_bytes(b"stale")
        with self.assertRaisesRegex(release.ReleaseError, "unexpected:.*stale"):
            self.build()

    def test_manifest_ignores_a_previous_manifest_asset(self) -> None:
        (self.assets / f"thegn-{self.tag}-manifest.json").write_text("{}", encoding="utf-8")
        self.assertEqual(self.expected, [asset["name"] for asset in self.build()["assets"]])

    def test_manifest_rejects_a_checksum_mismatch(self) -> None:
        archive_name, checksum_name = release._asset_names(
            self.manifest, self.tag, "x86_64-unknown-linux-gnu"
        )
        checksum = self.assets / checksum_name
        checksum.write_text(f"{'0' * 64}  {archive_name}\n", encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "checksum mismatch"):
            self.build()

    def test_manifest_rejects_a_record_digest_mismatch(self) -> None:
        record = json.loads(self.record_path.read_text(encoding="utf-8"))
        record["assets"][0]["sha256"] = "0" * 64
        self.record_path.write_text(json.dumps(record), encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "record digest mismatch"):
            self.build()

    def test_manifest_rejects_a_record_for_another_commit(self) -> None:
        self.write_record(commit="d" * 40)
        with self.assertRaisesRegex(release.ReleaseError, "commit does not match"):
            self.build()

    def test_manifest_rejects_a_record_with_extra_or_missing_entries(self) -> None:
        record = json.loads(self.record_path.read_text(encoding="utf-8"))
        record["assets"].pop()
        record["assets"].append({"name": "unexpected", "sha256": "e" * 64})
        self.record_path.write_text(json.dumps(record), encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "do not exactly match"):
            self.build()

    def test_manifest_rejects_a_symlinked_asset(self) -> None:
        victim = self.assets / "symlink-target"
        victim.write_bytes(b"x")
        link = self.assets / "thegn-symlink"
        link.symlink_to(victim)
        with self.assertRaisesRegex(release.ReleaseError, "not a regular file"):
            self.build()

    def test_manifest_output_is_deterministic(self) -> None:
        first = self.temp / "first.json"
        second = self.temp / "second.json"
        for output in (first, second):
            release.write_release_manifest(output, self.assets, self.build())
        self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_manifest_cli_exits_nonzero_on_refusal(self) -> None:
        (self.assets / f"thegn-{self.tag}-completions.tar.gz").unlink()
        stderr = io.StringIO()
        with self.assertRaises(SystemExit) as raised:
            with contextlib.redirect_stderr(stderr):
                release.main(
                    [
                        "manifest",
                        "--tag",
                        self.tag,
                        "--assets-dir",
                        str(self.assets),
                        "--record",
                        str(self.record_path),
                        "--commit",
                        self.commit,
                        "--output",
                        str(self.temp / "manifest.json"),
                    ]
                )
        self.assertEqual(2, raised.exception.code)
        self.assertIn("missing:", stderr.getvalue())
        self.assertIn("completions", stderr.getvalue())

    def test_manifest_cli_writes_and_prints_the_manifest(self) -> None:
        output = self.temp / "manifest.json"
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            result = release.main(
                [
                    "manifest",
                    "--tag",
                    self.tag,
                    "--assets-dir",
                    str(self.assets),
                    "--record",
                    str(self.record_path),
                    "--commit",
                    self.commit,
                    "--output",
                    str(output),
                ]
            )
        self.assertEqual(0, result)
        self.assertTrue(output.is_file())
        self.assertEqual(self.build(), json.loads(output.read_text(encoding="utf-8")))
        self.assertEqual(output.as_posix(), stdout.getvalue().strip())


class TrustedDigestTests(unittest.TestCase):
    make_asset = ReleaseRendererTests.make_asset
    setUp = ReleaseManifestTests.setUp
    write_record = ReleaseManifestTests.write_record
    build = ReleaseManifestTests.build

    def trusted(self) -> dict[str, str]:
        return {name: release._sha256(self.assets / name) for name in self.expected}

    def write_digest_file(self, name: str, value: object) -> Path:
        path = self.temp / name
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def downloaded_assets(self) -> tuple[Path, list[str]]:
        download = self.temp / "download"
        download.mkdir()
        names: list[str] = []
        for target, config in self.manifest["targets"].items():
            if config["enabled"]:
                names.extend(release._asset_names(self.manifest, self.tag, target))
        completions = f"thegn-{self.tag}-completions.tar.gz"
        names.extend((completions, f"{completions}.sha256"))
        for name in names:
            (download / name).write_bytes((self.assets / name).read_bytes())
        return download, sorted(names)

    def test_verify_digests_accepts_matching_producer_digests(self) -> None:
        download, names = self.downloaded_assets()
        trusted = {name: release._sha256(download / name) for name in names}
        self.assertEqual(
            names,
            release.verify_downloaded_assets(self.manifest, self.tag, download, trusted),
        )

    def test_verify_digests_rejects_archive_and_checksum_replaced_together(self) -> None:
        target = "x86_64-unknown-linux-gnu"
        archive_name, checksum_name = release._asset_names(self.manifest, self.tag, target)
        download, names = self.downloaded_assets()
        required = list(self.manifest["archive"]["root_files"])
        required[0] = self.manifest["targets"][target].get(
            "binary", self.manifest["project"]["binary"]
        )
        replacement = download / archive_name
        with tarfile.open(replacement, "w:gz") as output:
            for name in required:
                content = f"substituted:{name}\n".encode()
                info = tarfile.TarInfo(name)
                info.size = len(content)
                info.mode = 0o755 if name == "thegn" else 0o644
                info.mtime = 0
                output.addfile(info, io.BytesIO(content))
        digest = release._sha256(replacement)
        (download / checksum_name).write_text(
            f"{digest}  {archive_name}\n", encoding="utf-8"
        )
        managers = release.selected_managers(self.manifest, None)
        release.validate_assets(self.manifest, self.tag, download, managers)
        trusted = {name: release._sha256(self.assets / name) for name in names}
        archive_stem = archive_name.removesuffix(".tar.gz")
        with self.assertRaisesRegex(release.ReleaseError, archive_stem):
            release.verify_downloaded_assets(self.manifest, self.tag, download, trusted)
        trusted[checksum_name] = release._sha256(download / checksum_name)
        with self.assertRaisesRegex(release.ReleaseError, archive_name):
            release.verify_downloaded_assets(self.manifest, self.tag, download, trusted)

    def test_verify_digests_rejects_a_missing_trusted_entry(self) -> None:
        download, names = self.downloaded_assets()
        trusted = {name: release._sha256(download / name) for name in names[1:]}
        with self.assertRaisesRegex(release.ReleaseError, f"no trusted digest for {names[0]}"):
            release.verify_downloaded_assets(self.manifest, self.tag, download, trusted)

    def test_verify_digests_rejects_an_unexpected_downloaded_file(self) -> None:
        download, _ = self.downloaded_assets()
        (download / "unexpected.txt").write_text("extra", encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "unexpected: unexpected.txt"):
            release.verify_downloaded_assets(self.manifest, self.tag, download, self.trusted())

    def test_trusted_digest_loader_rejects_malformed_input(self) -> None:
        digest = "a" * 64
        cases = (
            ("array", [], "JSON object"),
            ("uppercase", {"thegn-a.tar.gz": "A" * 64}, "lowercase 64-hex"),
            ("short", {"thegn-a.tar.gz": "a" * 63}, "lowercase 64-hex"),
            ("slash", {"thegn/a": digest}, "invalid trusted digest asset name"),
            ("dotdot", {"..": digest}, "invalid trusted digest asset name"),
            ("empty", {}, "must not be empty"),
        )
        for label, value, message in cases:
            with self.subTest(case=label):
                path = self.write_digest_file(f"{label}.json", value)
                with self.assertRaisesRegex(release.ReleaseError, message):
                    release.load_trusted_digests([path])

        with self.subTest(case="malformed-json"):
            malformed = self.temp / "malformed.json"
            malformed.write_text("{not-json", encoding="utf-8")
            with self.assertRaisesRegex(release.ReleaseError, "cannot read trusted digests"):
                release.load_trusted_digests([malformed])

        with self.subTest(case="duplicate"):
            first = self.write_digest_file("first.json", {"thegn-a.tar.gz": digest})
            second = self.write_digest_file("second.json", {"thegn-a.tar.gz": digest})
            with self.assertRaisesRegex(release.ReleaseError, "given twice"):
                release.load_trusted_digests([first, second])
            duplicate = self.temp / "duplicate.json"
            duplicate.write_text(
                json.dumps({"thegn-a.tar.gz": digest})[:-1]
                + f',"thegn-a.tar.gz":"{digest}"}}',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(release.ReleaseError, "given twice"):
                release.load_trusted_digests([duplicate])

        with self.subTest(case="oversized"):
            oversized = self.temp / "oversized.json"
            oversized.write_bytes(b" " * (release.MAX_TRUSTED_DIGESTS_BYTES + 1))
            with self.assertRaisesRegex(release.ReleaseError, "exceed"):
                release.load_trusted_digests([oversized])

        with self.subTest(case="symlink"):
            target = self.write_digest_file("symlink-target.json", {"thegn-a.tar.gz": digest})
            link = self.temp / "symlink.json"
            link.symlink_to(target)
            with self.assertRaisesRegex(release.ReleaseError, "not a regular file"):
                release.load_trusted_digests([link])

    def test_manifest_with_trusted_digests_rejects_a_replaced_package(self) -> None:
        trusted = self.trusted()
        package = next(name for name in self.expected if name.endswith(".deb"))
        (self.assets / package).write_bytes(b"substituted package")
        with self.assertRaisesRegex(release.ReleaseError, f"{package} does not match"):
            release.build_release_manifest(
                self.manifest, self.tag, self.commit, self.assets, self.record_path, trusted=trusted
            )

    def test_manifest_with_trusted_digests_requires_the_exact_set(self) -> None:
        trusted = self.trusted()
        trusted.pop(self.expected[0])
        with self.assertRaisesRegex(release.ReleaseError, "missing:"):
            release.build_release_manifest(
                self.manifest, self.tag, self.commit, self.assets, self.record_path, trusted=trusted
            )
        trusted = self.trusted()
        trusted["thegn-extra"] = "a" * 64
        with self.assertRaisesRegex(release.ReleaseError, "unexpected: thegn-extra"):
            release.build_release_manifest(
                self.manifest, self.tag, self.commit, self.assets, self.record_path, trusted=trusted
            )

    def test_manifest_without_trusted_digests_is_unchanged(self) -> None:
        result = self.build()
        self.assertEqual(self.expected, [asset["name"] for asset in result["assets"]])

    def test_verify_digests_cli_exits_nonzero_on_refusal(self) -> None:
        download, names = self.downloaded_assets()
        wrong = {name: release._sha256(download / name) for name in names}
        wrong[names[0]] = "0" * 64
        digests = self.write_digest_file("cli-digests.json", wrong)
        stderr = io.StringIO()
        with self.assertRaises(SystemExit) as raised:
            with contextlib.redirect_stderr(stderr):
                release.main(
                    [
                        "verify-digests",
                        "--tag",
                        self.tag,
                        "--assets-dir",
                        str(download),
                        "--trusted-digests",
                        str(digests),
                    ]
                )
        self.assertEqual(2, raised.exception.code)
        self.assertIn(names[0], stderr.getvalue())


class StagedPayloadTests(unittest.TestCase):
    setUp = ReleaseRendererTests.setUp
    make_asset = ReleaseRendererTests.make_asset
    write_tar_asset = ReleaseRendererTests.write_tar_asset

    target = "x86_64-unknown-linux-gnu"

    def archive_and_trusted(self) -> tuple[Path, str, dict[str, str]]:
        archive, _ = self.make_asset(self.target)
        archive_name, _ = release._asset_names(self.manifest, self.tag, self.target)
        return archive, archive_name, {archive_name: release._sha256(archive)}

    def stage(self, trusted: dict[str, str] | None = None) -> tuple[Path, dict[str, str]]:
        archive, archive_name, original_trusted = self.archive_and_trusted()
        parent = self.temp / "package"
        parent.mkdir(exist_ok=True)
        output = parent / "stage"
        staged = release.stage_archive(
            self.manifest,
            self.tag,
            self.assets,
            self.target,
            output,
            original_trusted if trusted is None else trusted,
        )
        return output, staged

    def test_stage_writes_only_root_files_with_digests(self) -> None:
        archive, _, trusted = self.archive_and_trusted()
        parent = self.temp / "package"
        parent.mkdir()
        output = parent / "stage"
        staged = release.stage_archive(
            self.manifest, self.tag, self.assets, self.target, output, trusted
        )
        names = list(self.manifest["archive"]["root_files"])
        self.assertEqual(set(names), {path.name for path in output.iterdir()})
        with tarfile.open(archive, "r:gz") as source:
            contents = {member.name: source.extractfile(member).read() for member in source}
        for name in names:
            self.assertEqual(contents[name], (output / name).read_bytes())
            self.assertEqual(hashlib.sha256(contents[name]).hexdigest(), staged[name])
            expected_mode = 0o755 if name == "thegn" else 0o644
            self.assertEqual(expected_mode, stat.S_IMODE((output / name).stat().st_mode))
        self.assertEqual([], list(parent.glob(".stage-*")))

    def test_stage_cleans_temp_dir_when_handoff_fails(self) -> None:
        _, _, trusted = self.archive_and_trusted()
        parent = self.temp / "package"
        parent.mkdir()
        output = parent / "stage"
        with mock.patch.object(release.os, "rename", side_effect=OSError("boom")):
            with self.assertRaisesRegex(release.ReleaseError, "cannot stage release archive"):
                release.stage_archive(
                    self.manifest, self.tag, self.assets, self.target, output, trusted
                )
        self.assertFalse(output.exists())
        self.assertEqual([], list(parent.glob(".stage-*")))

    def test_stage_refuses_existing_output_dir(self) -> None:
        _, _, trusted = self.archive_and_trusted()
        parent = self.temp / "package"
        parent.mkdir()
        for kind in ("directory", "dangling-symlink"):
            with self.subTest(kind=kind):
                output = parent / kind
                if kind == "directory":
                    output.mkdir()
                else:
                    output.symlink_to(parent / "missing")
                before = sorted(path.name for path in parent.iterdir())
                with self.assertRaisesRegex(release.ReleaseError, "already exists"):
                    release.stage_archive(
                        self.manifest, self.tag, self.assets, self.target, output, trusted
                    )
                self.assertEqual(before, sorted(path.name for path in parent.iterdir()))

    def test_stage_refuses_archive_not_matching_trusted_digest(self) -> None:
        archive, archive_name, trusted = self.archive_and_trusted()
        archive.write_bytes(archive.read_bytes() + b"changed")
        checksum_name = release._asset_names(self.manifest, self.tag, self.target)[1]
        (self.assets / checksum_name).write_text(
            f"{release._sha256(archive)}  {archive_name}\n", encoding="utf-8"
        )
        parent = self.temp / "package"
        parent.mkdir()
        output = parent / "stage"
        with self.assertRaisesRegex(release.ReleaseError, archive_name):
            release.stage_archive(
                self.manifest, self.tag, self.assets, self.target, output, trusted
            )
        self.assertFalse(output.exists())
        self.assertEqual([], list(parent.glob(".stage-*")))

    def test_stage_refuses_invalid_layout_without_residue(self) -> None:
        archive, archive_name, _ = self.archive_and_trusted()
        target_config = self.manifest["targets"][self.target]
        required = list(self.manifest["archive"]["root_files"])
        required[0] = target_config.get("binary", self.manifest["project"]["binary"])
        with tarfile.open(archive, "w:gz") as output:
            for name in required:
                content = f"fixture:{name}\n".encode()
                info = tarfile.TarInfo(name)
                info.size = len(content)
                info.mode = 0o755 if name == "thegn" else 0o644
                output.addfile(info, io.BytesIO(content))
            content = b"extra"
            info = tarfile.TarInfo("extra")
            info.size = len(content)
            info.mode = 0o644
            output.addfile(info, io.BytesIO(content))
        trusted = {archive_name: release._sha256(archive)}
        parent = self.temp / "package"
        parent.mkdir()
        output = parent / "stage"
        with self.assertRaisesRegex(release.ReleaseError, "unexpected"):
            release.stage_archive(
                self.manifest, self.tag, self.assets, self.target, output, trusted
            )
        self.assertFalse(output.exists())
        self.assertEqual([], list(parent.glob(".stage-*")))

    def test_stage_never_uses_tar_extraction(self) -> None:
        _, _, trusted = self.archive_and_trusted()
        parent = self.temp / "package"
        parent.mkdir()
        with (
            mock.patch.object(tarfile.TarFile, "extract", side_effect=AssertionError),
            mock.patch.object(tarfile.TarFile, "extractall", side_effect=AssertionError),
        ):
            release.stage_archive(
                self.manifest, self.tag, self.assets, self.target, parent / "stage", trusted
            )

    def test_stage_refuses_a_non_unix_target(self) -> None:
        parent = self.temp / "package"
        parent.mkdir()
        with self.assertRaisesRegex(release.ReleaseError, "supports unix targets only"):
            release.stage_archive(
                self.manifest,
                self.tag,
                self.assets,
                "x86_64-pc-windows-msvc",
                parent / "stage",
                {},
            )

    def make_payload(self) -> Path:
        payload = self.temp / "payload"
        binary = payload / "usr/bin/thegn"
        binary.parent.mkdir(parents=True)
        binary.write_bytes((self.temp / "package/stage/thegn").read_bytes())
        (binary.parent / "tg").symlink_to("/usr/bin/thegn")
        licenses = payload / "usr/share/licenses/thegn"
        licenses.mkdir(parents=True)
        for name in ("LICENSE-MIT", "LICENSE-APACHE"):
            (licenses / name).write_bytes((self.temp / "package/stage" / name).read_bytes())
        return payload

    def staged_payload(self) -> tuple[Path, dict[str, str]]:
        return self.stage()

    def test_payload_accepts_matching_tree(self) -> None:
        _, digests = self.staged_payload()
        payload = self.make_payload()
        release.verify_package_payload(digests, payload, "deb")

    def test_payload_rejects_replaced_binary(self) -> None:
        _, digests = self.staged_payload()
        payload = self.make_payload()
        (payload / "usr/bin/thegn").write_bytes(b"replaced")
        with self.assertRaisesRegex(release.ReleaseError, "deb payload bytes differ at usr/bin/thegn"):
            release.verify_package_payload(digests, payload, "deb")

    def test_payload_rejects_symlinked_binary(self) -> None:
        _, digests = self.staged_payload()
        payload = self.make_payload()
        binary = payload / "usr/bin/thegn"
        binary.unlink()
        binary.symlink_to("/bin/sh")
        with self.assertRaisesRegex(release.ReleaseError, "invalid entry usr/bin/thegn"):
            release.verify_package_payload(digests, payload, "deb")

    def test_payload_rejects_extra_file(self) -> None:
        _, digests = self.staged_payload()
        payload = self.make_payload()
        extra = payload / "usr/share/doc/thegn/x"
        extra.parent.mkdir(parents=True)
        extra.write_text("extra", encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "unexpected entry usr/share/doc/thegn/x"):
            release.verify_package_payload(digests, payload, "deb")

    def test_payload_rejects_wrong_alias_target(self) -> None:
        _, digests = self.staged_payload()
        payload = self.make_payload()
        alias = payload / "usr/bin/tg"
        alias.unlink()
        alias.symlink_to("/bin/sh")
        with self.assertRaisesRegex(release.ReleaseError, "invalid entry usr/bin/tg"):
            release.verify_package_payload(digests, payload, "rpm")

    def test_payload_rejects_missing_license(self) -> None:
        _, digests = self.staged_payload()
        payload = self.make_payload()
        (payload / "usr/share/licenses/thegn/LICENSE-MIT").unlink()
        with self.assertRaisesRegex(release.ReleaseError, "missing entry"):
            release.verify_package_payload(digests, payload, "rpm")

    def test_staged_digest_loader_rejects_malformed_input(self) -> None:
        names = ("thegn", "LICENSE-MIT", "LICENSE-APACHE", "README.md")
        valid = {name: "a" * 64 for name in names}
        cases = (
            ("array", [], "JSON object"),
            ("missing", {name: digest for name, digest in valid.items() if name != "thegn"}, "exactly"),
            ("extra", {**valid, "extra": "a" * 64}, "exactly"),
            ("uppercase", {**valid, "thegn": "A" * 64}, "lowercase 64-hex"),
        )
        for label, value, message in cases:
            with self.subTest(case=label):
                path = self.temp / f"{label}.json"
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaisesRegex(release.ReleaseError, message):
                    release.load_staged_digests(path)
        target = self.temp / "staged-target.json"
        target.write_text(json.dumps(valid), encoding="utf-8")
        link = self.temp / "staged-link.json"
        link.symlink_to(target)
        with self.assertRaisesRegex(release.ReleaseError, "not a regular file"):
            release.load_staged_digests(link)

    def test_stage_and_verify_payload_cli_exit_nonzero_on_refusal(self) -> None:
        archive, archive_name, trusted = self.archive_and_trusted()
        wrong = dict(trusted)
        wrong[archive_name] = "0" * 64
        digests = self.temp / "trusted.json"
        digests.write_text(json.dumps(wrong), encoding="utf-8")
        parent = self.temp / "package"
        parent.mkdir()
        stderr = io.StringIO()
        with self.assertRaises(SystemExit) as raised:
            with contextlib.redirect_stderr(stderr):
                release.main(
                    [
                        "stage", "--tag", self.tag, "--assets-dir", str(self.assets),
                        "--target", self.target, "--trusted-digests", str(digests),
                        "--output-dir", str(parent / "stage"), "--digests-out", str(self.temp / "digests.json"),
                    ]
                )
        self.assertEqual(2, raised.exception.code)
        self.assertIn(archive_name, stderr.getvalue())
        self.assertFalse((parent / "stage").exists())
        self.assertFalse((self.temp / "digests.json").exists())

        _, staged = self.stage()
        payload = self.make_payload()
        (payload / "usr/bin/thegn").write_bytes(b"tampered")
        staged_path = self.temp / "staged.json"
        staged_path.write_text(json.dumps(staged), encoding="utf-8")
        stderr = io.StringIO()
        with self.assertRaises(SystemExit) as raised:
            with contextlib.redirect_stderr(stderr):
                release.main(
                    [
                        "verify-payload", "--tag", self.tag, "--staged-digests", str(staged_path),
                        "--payload-root", str(payload), "--label", "deb",
                    ]
                )
        self.assertEqual(2, raised.exception.code)
        self.assertIn("deb payload bytes differ at usr/bin/thegn", stderr.getvalue())


class ReleaseAssetManifestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.temp = Path(self.temporary.name)
        self.tag = f"v{release.workspace_version()}"
        self.asset = "thegn_0.1.0-amd64.deb"
        self.content = b"package bytes"
        self.digest = hashlib.sha256(self.content).hexdigest()

    def write_manifest(self, assets: list[dict[str, object]], **overrides: object) -> Path:
        value: dict[str, object] = {
            "schema_version": 1,
            "tag": self.tag,
            "commit": "fixture-commit",
            "tag_object": "fixture-tag-object",
            "workflow_sha": "fixture-workflow-sha",
            "run_id": "fixture-run-id",
            "assets": assets,
        }
        value.update(overrides)
        path = self.temp / "manifest.json"
        path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return path

    def valid_asset(self, **overrides: object) -> dict[str, object]:
        value: dict[str, object] = {"name": self.asset, "sha256": self.digest, "size": len(self.content)}
        value.update(overrides)
        return value

    def test_load_release_manifest_accepts_the_producer_shape(self) -> None:
        path = self.write_manifest([self.valid_asset()])
        self.assertEqual({self.asset: (self.digest, len(self.content))}, release.load_release_manifest(path, self.tag))
        output = self.temp / "round-trip.json"
        value = {
            "schema_version": 1,
            "tag": self.tag,
            "commit": "fixture-commit",
            "tag_object": "fixture-tag-object",
            "workflow_sha": "fixture-workflow-sha",
            "run_id": "fixture-run-id",
            "assets": [self.valid_asset()],
        }
        release.write_release_manifest(output, self.temp / "assets", value)
        self.assertEqual({self.asset: (self.digest, len(self.content))}, release.load_release_manifest(output, self.tag))

    def test_load_release_manifest_rejects_wrong_tag(self) -> None:
        path = self.write_manifest([self.valid_asset()], tag="v0.0.0")
        with self.assertRaisesRegex(release.ReleaseError, "does not match requested tag"):
            release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_duplicate_asset_name(self) -> None:
        path = self.write_manifest([self.valid_asset(), self.valid_asset()])
        with self.assertRaisesRegex(release.ReleaseError, "duplicate asset name"):
            release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_duplicate_json_key(self) -> None:
        path = self.write_manifest([self.valid_asset()])
        content = path.read_text(encoding="utf-8")
        tag_pair = f'  "tag": "{self.tag}",\n'
        self.assertIn(tag_pair, content)
        path.write_text(content.replace(tag_pair, tag_pair + tag_pair, 1), encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "duplicate JSON key"):
            release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_converts_json_recursion_error_to_release_error(self) -> None:
        path = self.write_manifest([self.valid_asset()])
        with mock.patch.object(release.json, "loads", side_effect=RecursionError("maximum recursion depth exceeded")):
            with self.assertRaisesRegex(release.ReleaseError, "cannot read release manifest"):
                release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_uppercase_or_short_sha256(self) -> None:
        for digest in ("A" * 64, "a" * 63):
            with self.subTest(digest=digest):
                path = self.write_manifest([self.valid_asset(sha256=digest)])
                with self.assertRaisesRegex(release.ReleaseError, "sha256"):
                    release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_bool_or_negative_size(self) -> None:
        for size in (True, -1):
            with self.subTest(size=size):
                path = self.write_manifest([self.valid_asset(size=size)])
                with self.assertRaisesRegex(release.ReleaseError, "size"):
                    release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_extra_top_level_key(self) -> None:
        path = self.write_manifest([self.valid_asset()], extra="field")
        with self.assertRaisesRegex(release.ReleaseError, "exactly the producer fields"):
            release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_extra_asset_key(self) -> None:
        path = self.write_manifest([self.valid_asset(extra="field")])
        with self.assertRaisesRegex(release.ReleaseError, "exactly name, sha256, and size"):
            release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_schema_version_2(self) -> None:
        path = self.write_manifest([self.valid_asset()], schema_version=2)
        with self.assertRaisesRegex(release.ReleaseError, "schema_version"):
            release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_a_symlinked_manifest(self) -> None:
        target = self.write_manifest([self.valid_asset()])
        linked = self.temp / "linked.json"
        linked.symlink_to(target)
        with self.assertRaisesRegex(release.ReleaseError, "regular file"):
            release.load_release_manifest(linked, self.tag)

    def test_load_release_manifest_rejects_an_oversized_manifest(self) -> None:
        path = self.temp / "large.json"
        path.write_bytes(b" " * (release.MAX_TRUSTED_DIGESTS_BYTES + 1))
        with self.assertRaisesRegex(release.ReleaseError, "exceed"):
            release.load_release_manifest(path, self.tag)

    def test_load_release_manifest_rejects_empty_assets(self) -> None:
        path = self.write_manifest([])
        with self.assertRaisesRegex(release.ReleaseError, "non-empty"):
            release.load_release_manifest(path, self.tag)

    def test_verify_release_asset_files_accepts_matching_bytes(self) -> None:
        path = self.temp / self.asset
        path.write_bytes(self.content)
        self.assertEqual({self.asset: self.digest}, release.verify_release_asset_files({self.asset: (self.digest, len(self.content))}, [path]))

    def test_verify_release_asset_files_rejects_digest_mismatch(self) -> None:
        path = self.temp / self.asset
        path.write_bytes(self.content)
        with self.assertRaisesRegex(release.ReleaseError, "does not match"):
            release.verify_release_asset_files({self.asset: ("0" * 64, len(self.content))}, [path])

    def test_verify_release_asset_files_rejects_size_mismatch(self) -> None:
        path = self.temp / self.asset
        path.write_bytes(self.content)
        with self.assertRaisesRegex(release.ReleaseError, "size does not match"):
            release.verify_release_asset_files({self.asset: (self.digest, len(self.content) + 1)}, [path])

    def test_verify_release_asset_files_rejects_unknown_name(self) -> None:
        path = self.temp / self.asset
        path.write_bytes(self.content)
        with self.assertRaisesRegex(release.ReleaseError, "not in the release manifest"):
            release.verify_release_asset_files({}, [path])

    def test_verify_release_asset_files_rejects_a_symlinked_asset(self) -> None:
        target = self.temp / "target"
        target.write_bytes(self.content)
        linked = self.temp / self.asset
        linked.symlink_to(target)
        with self.assertRaisesRegex(release.ReleaseError, "not a regular file"):
            release.verify_release_asset_files({self.asset: (self.digest, len(self.content))}, [linked])

    def test_verify_release_asset_files_rejects_a_name_given_twice(self) -> None:
        path = self.temp / self.asset
        path.write_bytes(self.content)
        with self.assertRaisesRegex(release.ReleaseError, "more than once"):
            release.verify_release_asset_files({self.asset: (self.digest, len(self.content))}, [path, path])

    def test_verify_assets_cli_reports_each_match_and_exits_2_on_mismatch(self) -> None:
        manifest = self.write_manifest([self.valid_asset()])
        asset = self.temp / self.asset
        asset.write_bytes(self.content)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            release.main(["verify-assets", "--tag", self.tag, "--release-manifest", str(manifest), "--asset", str(asset)])
        self.assertIn(f"{self.asset}: matches the release manifest", stdout.getvalue())
        asset.write_bytes(b"package byteX")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as raised:
            release.main(["verify-assets", "--tag", self.tag, "--release-manifest", str(manifest), "--asset", str(asset)])
        self.assertEqual(2, raised.exception.code)
        self.assertIn("does not match the release manifest", stderr.getvalue())

    def test_verify_assets_cli_does_not_require_the_workspace_version(self) -> None:
        tag = "v0.0.1-fixture"
        manifest = self.write_manifest([self.valid_asset()], tag=tag)
        asset = self.temp / self.asset
        asset.write_bytes(self.content)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            release.main(["verify-assets", "--tag", tag, "--release-manifest", str(manifest), "--asset", str(asset)])
        self.assertIn(f"{self.asset}: matches the release manifest", stdout.getvalue())

    def test_verify_assets_cli_rejects_an_invalid_tag_syntax(self) -> None:
        manifest = self.write_manifest([self.valid_asset()])
        asset = self.temp / self.asset
        asset.write_bytes(self.content)
        for tag in ("1.2.3", "v1.2.3;rm"):
            with self.subTest(tag=tag):
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as raised:
                    release.main(
                        ["verify-assets", "--tag", tag, "--release-manifest", str(manifest), "--asset", str(asset)]
                    )
                self.assertEqual(2, raised.exception.code)
                self.assertIn("invalid release tag", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
