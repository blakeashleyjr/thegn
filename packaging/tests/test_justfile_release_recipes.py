import os
from pathlib import Path
import stat
import subprocess
import tarfile
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
TEST_TAG = "v0.0.0-test"
HOST_TRIPLE = "x86_64-unknown-linux-gnu"
BINARY_BYTES = b"stub release binary\n"


def extract_recipe_body(recipe_name):
    lines = (REPO_ROOT / "justfile").read_text().splitlines()
    recipe_header = f"{recipe_name} "
    for index, line in enumerate(lines):
        if line.startswith(recipe_header) and line.endswith(":"):
            body = []
            for body_line in lines[index + 1 :]:
                if body_line and not body_line.startswith("    "):
                    break
                body.append(body_line)
            return textwrap.dedent("\n".join(body)).replace("{{tag}}", TEST_TAG)
    raise AssertionError(f"recipe {recipe_name!r} not found in justfile")


class ReleaseArtifactsRecipeTests(unittest.TestCase):
    def _write_executable(self, path, contents):
        path.write_text(contents)
        path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    def _run_recipe(self, cargo_target_dir=None):
        with tempfile.TemporaryDirectory() as temp_root:
            root = Path(temp_root)
            cwd = root / "checkout"
            stub_dir = root / "stubs"
            cwd.mkdir()
            stub_dir.mkdir()
            for filename in ("LICENSE-MIT", "LICENSE-APACHE", "README.md"):
                (cwd / filename).write_text(f"{filename}\n")

            self._write_executable(
                stub_dir / "rustc",
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "if sys.argv[1:] == ['-vV']:\n"
                f"    print('host: {HOST_TRIPLE}')\n",
            )
            self._write_executable(
                stub_dir / "cargo",
                "#!/usr/bin/env python3\n"
                "import os\n"
                "from pathlib import Path\n"
                "import sys\n"
                "args = sys.argv[1:]\n"
                "target = args[args.index('--target') + 1]\n"
                "target_dir = Path(os.environ.get('CARGO_TARGET_DIR', 'target'))\n"
                "binary = target_dir / target / 'release' / 'thegn'\n"
                "binary.parent.mkdir(parents=True, exist_ok=True)\n"
                f"binary.write_bytes({BINARY_BYTES!r})\n"
                "binary.chmod(0o755)\n",
            )
            self._write_executable(
                stub_dir / "shasum",
                "#!/usr/bin/env python3\n"
                "import hashlib\n"
                "from pathlib import Path\n"
                "import sys\n"
                "args = sys.argv[1:]\n"
                "if args[:2] != ['-a', '256']:\n"
                "    raise SystemExit('unsupported shasum arguments')\n"
                "if len(args) == 4 and args[2] == '-c':\n"
                "    for line in Path(args[3]).read_text().splitlines():\n"
                "        expected, filename = line.split('  ', 1)\n"
                "        actual = hashlib.sha256(Path(filename).read_bytes()).hexdigest()\n"
                "        if actual != expected:\n"
                "            raise SystemExit(f'{filename}: FAILED')\n"
                "        print(f'{filename}: OK')\n"
                "elif len(args) == 3:\n"
                "    filename = args[2]\n"
                "    digest = hashlib.sha256(Path(filename).read_bytes()).hexdigest()\n"
                "    print(f'{digest}  {filename}')\n"
                "else:\n"
                "    raise SystemExit('unsupported shasum arguments')\n",
            )

            env = os.environ.copy()
            env["PATH"] = f"{stub_dir}{os.pathsep}{env.get('PATH', '')}"
            if cargo_target_dir is None:
                env.pop("CARGO_TARGET_DIR", None)
            else:
                env["CARGO_TARGET_DIR"] = str(cargo_target_dir)

            result = subprocess.run(
                ["bash", "-c", extract_recipe_body("release-artifacts")],
                cwd=cwd,
                env=env,
                capture_output=True,
                text=True,
                timeout=60,
            )
            archive = (
                cwd
                / "target"
                / "release-artifacts"
                / f"thegn-{TEST_TAG}-{HOST_TRIPLE}.tar.gz"
            )
            archived_binary = None
            if archive.is_file():
                with tarfile.open(archive, "r:gz") as release_archive:
                    archived_binary = release_archive.extractfile("thegn").read()
            return result, archive.is_file(), archived_binary

    def test_copies_the_binary_from_cargo_target_dir(self):
        with tempfile.TemporaryDirectory() as external_target:
            target_dir = Path(external_target) / "cargo-target"
            result, archive_exists, archived_binary = self._run_recipe(target_dir)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(archive_exists, result.stderr)
            self.assertEqual(archived_binary, BINARY_BYTES)

    def test_default_target_dir_still_works(self):
        # Positive control: this also passes with the original hard-coded target path.
        result, archive_exists, archived_binary = self._run_recipe()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(archive_exists, result.stderr)
        self.assertEqual(archived_binary, BINARY_BYTES)

    def test_recipe_has_no_hard_coded_cargo_output_path(self):
        for recipe_name in ("release-artifacts", "release-verify"):
            with self.subTest(recipe=recipe_name):
                self.assertNotIn('"target/$target/release', extract_recipe_body(recipe_name))


if __name__ == "__main__":
    unittest.main()
