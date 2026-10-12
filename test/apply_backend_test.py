#!/usr/bin/env python3
"""Regression coverage for the justfile's repository backend overlay writer."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile


REPO = Path(__file__).resolve().parents[1]
JUSTFILE = REPO / "justfile"


def run_backend(worktree: Path, home: Path) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update({"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config")})
    return subprocess.run(
        ["just", "--justfile", str(JUSTFILE), "_apply-backend", "backend=podman"],
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

        # An absent overlay is generated; an identical rerun is a byte/mtime no-op.
        overlay.unlink()
        result = run_backend(worktree, home)
        assert result.returncode == 0, result.stdout + result.stderr
        expected = b'[sandbox] backend = "podman"\n'
        assert overlay.read_bytes() == expected
        before = overlay.stat().st_mtime_ns
        result = run_backend(worktree, home)
        assert result.returncode == 0, result.stdout + result.stderr
        assert overlay.read_bytes() == expected
        assert overlay.stat().st_mtime_ns == before

    print("apply-backend checks passed")


if __name__ == "__main__":
    main()
