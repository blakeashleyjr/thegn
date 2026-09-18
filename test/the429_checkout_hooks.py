#!/usr/bin/env python3
"""Focused THE-429 fixtures; intentionally independent of Cargo and builds."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DETECTOR = ROOT / "nix" / "detect-legacy-post-checkout.py"
BASE_COMMIT = "0be575cecab318db9049352b0c7ae5cb45b18581"


def run(*args: str, cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        list(args), cwd=cwd, env=env, check=True, text=True, capture_output=True
    )


def git(*args: str, cwd: Path, env: dict[str, str]) -> None:
    run("git", *args, cwd=cwd, env=env)


def detector(repo: Path, env: dict[str, str]) -> str:
    result = run(sys.executable, str(DETECTOR), cwd=repo, env=env)
    return result.stderr


def metadata(path: Path) -> tuple[int, int, int, int]:
    entry = os.lstat(path)
    return (entry.st_dev, entry.st_ino, entry.st_size, entry.st_mtime_ns)


def main() -> int:
    assert not (ROOT / "test/git-hooks/post-checkout.sh").exists()
    flake = (ROOT / "flake.nix").read_text()
    assert "test/git-hooks/post-checkout.sh" not in flake
    assert "PREK_ALLOW_NO_CONFIG" in flake

    with tempfile.TemporaryDirectory(prefix="the429-") as temporary:
        sandbox = Path(temporary)
        repo = sandbox / "repo"
        repo.mkdir()
        config_home = sandbox / "config"
        config_home.mkdir()
        global_config = config_home / "gitconfig"
        global_config.write_text("")
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(sandbox / "home"),
                "XDG_CONFIG_HOME": str(config_home),
                "GIT_CONFIG_GLOBAL": str(global_config),
                "GIT_CONFIG_NOSYSTEM": "1",
            }
        )
        (sandbox / "home").mkdir()

        git("init", "-q", cwd=repo, env=env)
        git("config", "user.email", "the429@example.invalid", cwd=repo, env=env)
        git("config", "user.name", "THE-429 fixture", cwd=repo, env=env)
        (repo / ".gitignore").write_text(".pre-commit-config.yaml\n")
        (repo / "README").write_text("base\n")
        git("add", ".", cwd=repo, env=env)
        git("commit", "-qm", "base", cwd=repo, env=env)
        git("branch", "-M", "main", cwd=repo, env=env)

        sentinel = sandbox / "sentinel"
        payload = (
            "#!/bin/sh\n"
            f"printf '%s\\n' branch-payload >> {sentinel}\n"
            "exit 0\n"
        )
        git("checkout", "-qb", "hostile", cwd=repo, env=env)
        (repo / "test/git-hooks").mkdir(parents=True)
        (repo / "test/git-hooks/post-checkout.sh").write_text(payload)
        (repo / "test/git-hooks/heal-worktree.sh").write_text(payload)
        os.chmod(repo / "test/git-hooks/post-checkout.sh", 0o755)
        os.chmod(repo / "test/git-hooks/heal-worktree.sh", 0o755)
        git("add", ".", cwd=repo, env=env)
        git("commit", "-qm", "hostile branch payloads", cwd=repo, env=env)
        git("checkout", "-q", "main", cwd=repo, env=env)

        # These are the real Git checkout/worktree operations that used to fire
        # the installed shared post-checkout hook. There is no installed hook in
        # this hermetic repository, so branch-controlled files remain inert.
        git("checkout", "-q", "hostile", cwd=repo, env=env)
        git("checkout", "-q", "main", cwd=repo, env=env)
        worktree = sandbox / "worktree"
        git("worktree", "add", "-q", "-b", "fixture-clean", str(worktree), "main", cwd=repo, env=env)
        (worktree / ".pre-commit-config.yaml").write_bytes(b"foreign local config\n")
        before_config = (worktree / ".pre-commit-config.yaml").read_bytes()
        before_config_meta = metadata(worktree / ".pre-commit-config.yaml")
        git("checkout", "-q", "hostile", cwd=worktree, env=env)
        git("checkout", "-q", "fixture-clean", cwd=worktree, env=env)
        assert not sentinel.exists(), "a branch-controlled checkout payload ran"
        assert (worktree / ".pre-commit-config.yaml").read_bytes() == before_config
        assert metadata(worktree / ".pre-commit-config.yaml") == before_config_meta

        # Obtain the exact legacy bytes from the reviewed pre-remediation base;
        # the current tracked executable is intentionally gone.
        legacy = subprocess.run(
            ["git", "show", f"{BASE_COMMIT}:test/git-hooks/post-checkout.sh"],
            cwd=ROOT,
            env=env,
            check=True,
            capture_output=True,
        ).stdout
        hooks = repo / ".git/hooks"
        candidate = hooks / "post-checkout"
        candidate.write_bytes(legacy)
        os.chmod(candidate, 0o755)
        before = metadata(candidate)
        report = detector(repo, env)
        assert "exact legacy checkout hook" in report
        assert metadata(candidate) == before
        assert candidate.read_bytes() == legacy

        candidate.write_bytes(b"foreign hook\n")
        before = metadata(candidate)
        assert "foreign post-checkout" in detector(repo, env)
        assert metadata(candidate) == before
        assert candidate.read_bytes() == b"foreign hook\n"

        # All collision types are read-only and no-follow. In particular, the
        # FIFO test would hang if the detector accidentally opened it normally.
        candidate.unlink()
        candidate.mkdir()
        directory_before = metadata(candidate)
        assert "not an ordinary regular file" in detector(repo, env)
        assert metadata(candidate) == directory_before
        candidate.rmdir()
        target = hooks / "foreign-target"
        target.write_bytes(b"target\n")
        candidate.symlink_to(target)
        symlink_before = metadata(candidate)
        target_before = metadata(target)
        assert "not an ordinary regular file" in detector(repo, env)
        assert metadata(candidate) == symlink_before
        assert metadata(target) == target_before
        candidate.unlink()
        candidate.symlink_to(hooks / "missing-target")
        dangling_before = metadata(candidate)
        assert "not an ordinary regular file" in detector(repo, env)
        assert metadata(candidate) == dangling_before
        candidate.unlink()
        os.mkfifo(candidate)
        fifo_before = metadata(candidate)
        assert "not an ordinary regular file" in detector(repo, env)
        assert metadata(candidate) == fifo_before
        candidate.unlink()
        candidate.write_bytes(b"x" * (64 * 1024 + 1))
        assert "read bound" in detector(repo, env)

        # Concurrent read-only setup attempts cannot overwrite the candidate.
        candidate.write_bytes(b"foreign concurrent hook\n")
        before = metadata(candidate)
        processes = [
            subprocess.Popen(
                [sys.executable, str(DETECTOR)], cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            for _ in range(2)
        ]
        for process in processes:
            assert process.wait(timeout=5) == 0
        assert metadata(candidate) == before
        assert candidate.read_bytes() == b"foreign concurrent hook\n"

        # A local custom path and a global path are both refused without even
        # inspecting their post-checkout entries. The exact local .git/hooks
        # value used by generated prek setup remains eligible.
        git("config", "--local", "core.hooksPath", str(hooks), cwd=repo, env=env)
        assert "foreign post-checkout" in detector(repo, env)
        git("config", "--local", "--unset", "core.hooksPath", cwd=repo, env=env)
        custom = sandbox / "custom-hooks"
        custom.mkdir()
        custom_candidate = custom / "post-checkout"
        custom_candidate.write_bytes(b"custom\n")
        git("config", "--local", "core.hooksPath", str(custom), cwd=repo, env=env)
        assert "custom" in detector(repo, env)
        assert custom_candidate.read_bytes() == b"custom\n"
        git("config", "--local", "--unset", "core.hooksPath", cwd=repo, env=env)
        global_hooks = sandbox / "global-hooks"
        global_hooks.mkdir()
        global_candidate = global_hooks / "post-checkout"
        global_candidate.write_bytes(b"global\n")
        global_config.write_text(f"[core]\n\thooksPath = {global_hooks}\n")
        assert "configured" in detector(repo, env)
        assert global_candidate.read_bytes() == b"global\n"
        global_config.write_text("")

        # Linked worktrees have a .git file and may not inspect or mutate the
        # shared main checkout's hook slot.
        assert "main checkout" in detector(worktree, env)

        shutil.rmtree(worktree)
        git("worktree", "prune", cwd=repo, env=env)
    print("THE-429 focused checkout-hook and detector fixtures: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
