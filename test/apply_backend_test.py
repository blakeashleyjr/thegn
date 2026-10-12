#!/usr/bin/env python3
"""Regression coverage for the justfile's repository backend overlay writer."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


REPO = Path(__file__).resolve().parents[1]
JUSTFILE = REPO / "justfile"
COMMON_DIR = Path(
    subprocess.check_output(
        ["git", "-C", str(REPO), "rev-parse", "--git-common-dir"], text=True
    ).strip()
)
if not COMMON_DIR.is_absolute():
    COMMON_DIR = (REPO / COMMON_DIR).resolve()
REAL_REPO_OVERLAY = COMMON_DIR.parent / ".thegn.toml"


def run_backend(worktree: Path, home: Path) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update({"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config")})
    return subprocess.run(
        [
            "just",
            "--justfile",
            str(JUSTFILE),
            "--working-directory",
            str(worktree),
            "_apply-backend",
            "podman",
        ],
        cwd=worktree,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )


def make_repo(root: Path) -> tuple[Path, Path]:
    worktree = root / "repo"
    home = root / "home"
    worktree.mkdir()
    home.mkdir()
    subprocess.run(["git", "init", "-q", str(worktree)], check=True)
    return worktree, home


def main() -> None:
    if shutil.which("just") is None:
        raise SystemExit("test-apply-backend requires just on PATH")

    with tempfile.TemporaryDirectory(prefix="thegn-apply-backend-") as temporary:
        root = Path(temporary)
        worktree, home = make_repo(root)
        overlay = worktree / ".thegn.toml"
        real_overlay_was_present = os.path.lexists(REAL_REPO_OVERLAY)

        try:
            # Arbitrary valid overlay content must fail closed, byte-for-byte.
            original = b'theme = "x"\n'
            overlay.write_bytes(original)
            result = run_backend(worktree, home)
            assert result.returncode != 0, result.stdout + result.stderr
            assert "edit it by hand" in result.stderr
            assert overlay.read_bytes() == original

            # A symlink destination is refused, and its target remains untouched.
            overlay.unlink()
            target = root / "overlay-target.toml"
            target.write_bytes(b'theme = "target"\n')
            overlay.symlink_to(target)
            result = run_backend(worktree, home)
            assert result.returncode != 0, result.stdout + result.stderr
            assert "symlink" in result.stderr
            assert overlay.is_symlink()
            assert target.read_bytes() == b'theme = "target"\n'

            # A two-line overlay from the prior recipe is generated content.
            overlay.unlink()
            overlay.write_bytes(b'[sandbox]\nbackend = "docker"\n')
            result = run_backend(worktree, home)
            assert result.returncode == 0, result.stdout + result.stderr
            assert overlay.read_bytes() == b'[sandbox]\nbackend = "podman"\n'

            # An absent overlay is generated; an identical rerun is a byte/mtime no-op.
            overlay.unlink()
            result = run_backend(worktree, home)
            assert result.returncode == 0, result.stdout + result.stderr
            expected = b'[sandbox]\nbackend = "podman"\n'
            assert overlay.read_bytes() == expected
            assert tomllib.loads(overlay.read_text()) == {
                "sandbox": {"backend": "podman"}
            }
            before = overlay.stat().st_mtime_ns
            result = run_backend(worktree, home)
            assert result.returncode == 0, result.stdout + result.stderr
            assert overlay.read_bytes() == expected
            assert overlay.stat().st_mtime_ns == before
        finally:
            assert os.path.lexists(REAL_REPO_OVERLAY) == real_overlay_was_present

    print("apply-backend checks passed")


if __name__ == "__main__":
    main()
