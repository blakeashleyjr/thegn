#!/usr/bin/env python3
"""Regression coverage for the justfile's repository backend overlay writer."""

import os
from pathlib import Path
import shutil
import stat
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


def run_backend(
    worktree: Path,
    home: Path,
    backend: str = "podman",
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update({"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config")})
    if extra_env is not None:
        env.update(extra_env)
    return subprocess.run(
        [
            "just",
            "--justfile",
            str(JUSTFILE),
            "--working-directory",
            str(worktree),
            "_apply-backend",
            backend,
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
            print("apply-backend podman user-content refusal passed")

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
            print("apply-backend podman symlink refusal passed")

            # A two-line overlay from the prior recipe is generated content.
            overlay.unlink()
            overlay.write_bytes(b'[sandbox]\nbackend = "docker"\n')
            result = run_backend(worktree, home)
            assert result.returncode == 0, result.stdout + result.stderr
            assert overlay.read_bytes() == b'[sandbox]\nbackend = "podman"\n'
            print("apply-backend podman generated-overlay replacement passed")

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
            print("apply-backend podman identical-rerun no-op passed")
        finally:
            assert os.path.lexists(REAL_REPO_OVERLAY) == real_overlay_was_present

        sprites_env = {"SPRITES_TOKEN": "test-token"}

        invalid_root = root / "sprites-invalid-config"
        invalid_root.mkdir()
        invalid_repo, invalid_home = make_repo(invalid_root)
        invalid_cfg = invalid_home / ".config" / "thegn" / "config.toml"
        invalid_cfg.parent.mkdir(parents=True)
        invalid_cfg.write_bytes(b"[broken\n")
        result = run_backend(invalid_repo, invalid_home, "sprites", sprites_env)
        assert result.returncode != 0, result.stdout + result.stderr
        assert "Traceback" not in result.stderr
        assert invalid_cfg.read_bytes() == b"[broken\n"
        assert not (invalid_repo / ".thegn.toml").exists()
        assert not list(invalid_cfg.parent.glob(".config.toml.*"))
        print("apply-backend sprites invalid-config refusal passed")

        duplicate_root = root / "sprites-duplicate-table"
        duplicate_root.mkdir()
        duplicate_repo, duplicate_home = make_repo(duplicate_root)
        duplicate_cfg = duplicate_home / ".config" / "thegn" / "config.toml"
        duplicate_cfg.parent.mkdir(parents=True)
        duplicate_bytes = b'[env.sprites.provider]\nprovider = "x"\n'
        duplicate_cfg.write_bytes(duplicate_bytes)
        result = run_backend(duplicate_repo, duplicate_home, "sprites", sprites_env)
        assert result.returncode != 0, result.stdout + result.stderr
        assert duplicate_cfg.read_bytes() == duplicate_bytes
        assert not (duplicate_repo / ".thegn.toml").exists()
        assert not list(duplicate_cfg.parent.glob(".config.toml.*"))
        print("apply-backend sprites duplicate-table refusal passed")

        if os.geteuid() == 0:
            print("apply-backend sprites overlay-stage failure skipped (running as root)")
        else:
            readonly_root = root / "sprites-readonly-overlay-stage"
            readonly_root.mkdir()
            readonly_repo, readonly_home = make_repo(readonly_root)
            try:
                os.chmod(readonly_repo, 0o500)
                result = run_backend(readonly_repo, readonly_home, "sprites", sprites_env)
                assert result.returncode != 0, result.stdout + result.stderr
                readonly_cfg = readonly_home / ".config" / "thegn" / "config.toml"
                assert not readonly_cfg.exists()
                assert not list(readonly_cfg.parent.glob(".config.toml.*"))
                print("apply-backend sprites overlay-stage failure leaves config absent")
            finally:
                os.chmod(readonly_repo, 0o755)

        valid_root = root / "sprites-valid-config"
        valid_root.mkdir()
        valid_repo, valid_home = make_repo(valid_root)
        valid_cfg = valid_home / ".config" / "thegn" / "config.toml"
        valid_cfg.parent.mkdir(parents=True)
        valid_original = b'theme = "x"\n# keep me\n'
        valid_cfg.write_bytes(valid_original)
        result = run_backend(valid_repo, valid_home, "sprites", sprites_env)
        assert result.returncode == 0, result.stdout + result.stderr
        assert valid_cfg.read_bytes().startswith(valid_original)
        parsed_cfg = tomllib.loads(valid_cfg.read_text())
        assert parsed_cfg["env"]["sprites"]["provider"]["provider"] == "sprites"
        valid_overlay = valid_repo / ".thegn.toml"
        assert valid_overlay.read_bytes() == b'env = "sprites"\n'
        cfg_bytes = valid_cfg.read_bytes()
        overlay_bytes = valid_overlay.read_bytes()
        cfg_mtime = valid_cfg.stat().st_mtime_ns
        overlay_mtime = valid_overlay.stat().st_mtime_ns
        result = run_backend(valid_repo, valid_home, "sprites", sprites_env)
        assert result.returncode == 0, result.stdout + result.stderr
        assert valid_cfg.read_bytes() == cfg_bytes
        assert valid_overlay.read_bytes() == overlay_bytes
        assert valid_cfg.stat().st_mtime_ns == cfg_mtime
        assert valid_overlay.stat().st_mtime_ns == overlay_mtime
        assert not list(valid_cfg.parent.glob(".config.toml.*"))
        print("apply-backend sprites valid-config and identical-rerun passed")

        absent_root = root / "sprites-absent-config"
        absent_root.mkdir()
        absent_repo, absent_home = make_repo(absent_root)
        result = run_backend(absent_repo, absent_home, "sprites", sprites_env)
        assert result.returncode == 0, result.stdout + result.stderr
        absent_cfg = absent_home / ".config" / "thegn" / "config.toml"
        assert tomllib.loads(absent_cfg.read_text())["env"]["sprites"]["provider"]["provider"] == "sprites"
        assert stat.S_IMODE(absent_cfg.stat().st_mode) == 0o600
        assert not list(absent_cfg.parent.glob(".config.toml.*"))
        print("apply-backend sprites absent-config scaffold passed")

        symlink_root = root / "sprites-symlink-config"
        symlink_root.mkdir()
        symlink_repo, symlink_home = make_repo(symlink_root)
        symlink_cfg = symlink_home / ".config" / "thegn" / "config.toml"
        symlink_cfg.parent.mkdir(parents=True)
        config_target = symlink_root / "config-target.toml"
        config_bytes = b'theme = "target"\n'
        config_target.write_bytes(config_bytes)
        symlink_cfg.symlink_to(config_target)
        result = run_backend(symlink_repo, symlink_home, "sprites", sprites_env)
        assert result.returncode != 0, result.stdout + result.stderr
        assert symlink_cfg.is_symlink()
        assert config_target.read_bytes() == config_bytes
        assert not (symlink_repo / ".thegn.toml").exists()
        assert not list(symlink_cfg.parent.glob(".config.toml.*"))
        print("apply-backend sprites symlink-config refusal passed")

        assert os.path.lexists(REAL_REPO_OVERLAY) == real_overlay_was_present

    print("apply-backend checks passed")


if __name__ == "__main__":
    main()
