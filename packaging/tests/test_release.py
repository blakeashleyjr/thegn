from __future__ import annotations

import copy
import hashlib
import importlib.util
import io
import sys
import tarfile
import tempfile
import tomllib
import unittest
import zipfile
from pathlib import Path


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


if __name__ == "__main__":
    unittest.main()
