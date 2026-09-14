#!/usr/bin/env python3
"""Exercise the production build script in real, dependency-free Cargo builds."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class MetadataTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="thegn-metadata-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "CARGO_"))}
        self.env.update(RUSTC_WRAPPER="", GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                        GIT_AUTHOR_NAME="Fixture", GIT_COMMITTER_NAME="Fixture",
                        GIT_AUTHOR_EMAIL="fixture@example.invalid", GIT_COMMITTER_EMAIL="fixture@example.invalid",
                        CARGO_TARGET_DIR=str(self.root / "target"))
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.command(self.repo, "git", "init", "-b", "main")
        self.package(self.repo)
        self.command(self.repo, "git", "add", ".")
        self.command(self.repo, "git", "commit", "-m", "initial")

    def package(self, root):
        package = root / "crates/thegn-host"
        (package / "src").mkdir(parents=True)
        (root / "Cargo.toml").write_text('[workspace]\nmembers=["crates/thegn-host"]\nresolver="2"\n')
        (package / "Cargo.toml").write_text('[package]\nname="identity-fixture"\nversion="0.1.0"\nedition="2024"\n')
        (package / "src/main.rs").write_text('fn main() { println!("{}|{}", env!("THEGN_GIT_SHA"), env!("THEGN_BUILD_TIME")); }\n')
        shutil.copy2(ROOT / "crates/thegn-host/build.rs", package / "build.rs")

    def command(self, cwd, *args, env=None):
        return subprocess.run(args, cwd=cwd, env=env or self.env, check=True,
                              capture_output=True, text=True, timeout=60)

    def build(self, root, fresh=False, env=None):
        result = self.command(root, "cargo", "build", "--offline", "-v", env=env)
        if fresh:
            self.assertIn("Fresh identity-fixture", result.stderr, result.stderr)
            self.assertNotIn("Compiling identity-fixture", result.stderr)
        output = self.command(root, str(self.root / "target/debug/identity-fixture")).stdout.strip()
        return output.split("|")[0]

    def head(self, root):
        return self.command(root, "git", "rev-parse", "--short", "HEAD").stdout.strip()

    def test_main_ref_packing_switch_and_detached(self):
        self.assertEqual(self.build(self.repo), self.head(self.repo))
        self.build(self.repo, fresh=True)
        self.command(self.repo, "git", "commit", "--allow-empty", "-m", "metadata only")
        self.assertEqual(self.build(self.repo), self.head(self.repo))
        self.command(self.repo, "git", "pack-refs", "--all", "--prune")
        self.assertEqual(self.build(self.repo), self.head(self.repo))
        self.build(self.repo, fresh=True)
        self.command(self.repo, "git", "commit", "--allow-empty", "-m", "packed to loose")
        self.assertEqual(self.build(self.repo), self.head(self.repo))
        self.command(self.repo, "git", "checkout", "-b", "other", "HEAD~1")
        self.assertEqual(self.build(self.repo), self.head(self.repo))
        self.command(self.repo, "git", "checkout", "--detach", "main")
        self.assertEqual(self.build(self.repo), self.head(self.repo))
        self.build(self.repo, fresh=True)

    def test_linked_worktree_and_outer_hook_environment(self):
        linked = self.root / "linked"
        self.command(self.repo, "git", "worktree", "add", "-b", "linked", str(linked))
        self.command(linked, "git", "commit", "--allow-empty", "-m", "linked identity")
        hostile = dict(self.env, GIT_DIR=str(self.repo / ".git"), GIT_WORK_TREE=str(self.repo))
        self.assertEqual(self.build(linked, env=hostile), self.head(linked))
        self.build(linked, fresh=True, env=hostile)
        self.command(linked, "git", "commit", "--allow-empty", "-m", "advance")
        self.assertEqual(self.build(linked, env=hostile), self.head(linked))
        self.build(linked, fresh=True, env=hostile)

    def test_archive_stays_fresh_without_git_metadata(self):
        archive = self.root / "archive"
        self.package(archive)
        self.assertEqual(self.build(archive), "")
        self.assertEqual(self.build(archive, fresh=True), "")


if __name__ == "__main__":
    unittest.main()
