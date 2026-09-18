#!/usr/bin/env python3
"""Focused THE-429 fixtures; intentionally independent of Cargo and builds."""

from __future__ import annotations

import os
import shlex
import hashlib
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DETECTOR = ROOT / "nix" / "detect-legacy-post-checkout.py"
LEGACY_FIXTURE = ROOT / "test/fixtures/the429/legacy-post-checkout.sh"
LEGACY_SHA256 = "90b85945a3c30c3a9aa806f87fe9585e9ce660e450672e3e5e24b40dca140365"


def run(*args: str, cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        list(args), cwd=cwd, env=env, check=True, text=True, capture_output=True, timeout=10
    )


def git(*args: str, cwd: Path, env: dict[str, str]) -> None:
    run("git", *args, cwd=cwd, env=env)


def detector(repo: Path, env: dict[str, str]) -> str:
    result = subprocess.run(
        [sys.executable, str(DETECTOR)],
        cwd=repo,
        env=env,
        check=True,
        text=True,
        capture_output=True,
        timeout=5,
    )
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    return result.stderr


def shell_entry(repo: Path, env: dict[str, str]) -> str:
    # Execute the actual hookExtras body, substituting only the two immutable
    # Nix store references. This detects an installer reintroduced beside the
    # detector, which invoking the detector alone would miss.
    flake = (ROOT / "flake.nix").read_text()
    body = flake.split("      hookExtras = ''\n", 1)[1].split("\n      '';", 1)[0]
    body = body.replace("${pkgs.python3}/bin/python3", shlex.quote(sys.executable))
    body = body.replace("${./nix/detect-legacy-post-checkout.py}", shlex.quote(str(DETECTOR)))
    body = body.replace("''${", "${")
    return run("sh", "-c", body, cwd=repo, env={**env, "CI": ""}).stderr


def fake_git(sandbox: Path, mode: str) -> dict[str, str]:
    fake_bin = sandbox / f"fake-git-{mode}"
    fake_bin.mkdir()
    executable = fake_bin / "git"
    executable.write_text(
        "#!/usr/bin/env python3\n"
        "import sys\n"
        "import time\n"
        f"mode = {mode!r}\n"
        "if mode == 'hang':\n"
        "    time.sleep(30)\n"
        "elif mode == 'oversized':\n"
        "    sys.stdout.write('x' * (128 * 1024))\n"
        "elif mode == 'invalid':\n"
        "    sys.stdout.buffer.write(b'/tmp/invalid-\\xff\\n')\n"
    )
    executable.chmod(0o755)
    environment = os.environ.copy()
    environment["PATH"] = f"{fake_bin}{os.pathsep}{environment['PATH']}"
    return environment


def injected_replacement(repo: Path, env: dict[str, str], kind: str) -> str:
    """Replace the candidate after lstat in a bounded child process."""

    child = r'''
import importlib.util
import os
import sys

detector_path, repo_path, replacement_kind = sys.argv[1:]
spec = importlib.util.spec_from_file_location("the429_detector", detector_path)
module = importlib.util.module_from_spec(spec)
assert spec.loader is not None
sys.modules[spec.name] = module
spec.loader.exec_module(module)
original_open = os.open
replaced = False

def replacing_open(path, flags, mode=0o777, *, dir_fd=None):
    global replaced
    if not replaced and path == "post-checkout" and dir_fd is not None:
        replaced = True
        os.unlink(path, dir_fd=dir_fd)
        if replacement_kind == "fifo":
            os.mkfifo(path, mode=0o600, dir_fd=dir_fd)
        elif replacement_kind == "symlink":
            os.symlink("replacement-target", path, dir_fd=dir_fd)
        elif replacement_kind == "regular":
            fd = original_open(path, os.O_WRONLY | os.O_CREAT, 0o600, dir_fd=dir_fd)
            try:
                os.write(fd, b"replacement")
            finally:
                os.close(fd)
    return original_open(path, flags, mode, dir_fd=dir_fd)

module.os.open = replacing_open
os.chdir(repo_path)
raise SystemExit(module.main())
'''
    result = subprocess.run(
        [sys.executable, "-c", child, str(DETECTOR), str(repo), kind],
        cwd=repo,
        env=env,
        check=False,
        text=True,
        capture_output=True,
        timeout=5,
    )
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    return result.stderr


def metadata(path: Path) -> tuple[int, int, int, int]:
    entry = os.lstat(path)
    return (entry.st_dev, entry.st_ino, entry.st_size, entry.st_mtime_ns)


def main() -> int:
    assert not (ROOT / "test/git-hooks/post-checkout.sh").exists()
    flake = (ROOT / "flake.nix").read_text()
    assert "test/git-hooks/post-checkout.sh" not in flake
    assert "nix/detect-legacy-post-checkout.py" in flake
    assert "PREK_ALLOW_NO_CONFIG" in flake
    legacy = LEGACY_FIXTURE.read_bytes()
    assert hashlib.sha256(legacy).hexdigest() == LEGACY_SHA256
    assert not (LEGACY_FIXTURE.stat().st_mode & 0o111)

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
        (repo / "test/git-hooks").mkdir(parents=True)
        (repo / "test/git-hooks/post-checkout.sh").write_bytes(legacy)
        (repo / "test/git-hooks/heal-worktree.sh").write_text("#!/bin/sh\nexit 0\n")
        git("add", ".", cwd=repo, env=env)
        git("commit", "-qm", "base", cwd=repo, env=env)
        git("branch", "-M", "main", cwd=repo, env=env)

        # This is the same immutable helper invoked by hookExtras in flake.nix.
        # Run it before creating or entering the hostile branch so the fixture
        # exercises the actual shell-entry setup seam, not only a source check.
        assert "no legacy post-checkout hook" in shell_entry(repo, env)

        sentinel = sandbox / "sentinel"
        payload = (
            "#!/bin/sh\n"
            f"printf '%s\\n' branch-payload >> {sentinel}\n"
            "exit 0\n"
        )
        git("checkout", "-qb", "hostile", cwd=repo, env=env)
        (repo / "test/git-hooks/post-checkout.sh").write_text(payload)
        (repo / "test/git-hooks/heal-worktree.sh").write_text(payload)
        os.chmod(repo / "test/git-hooks/post-checkout.sh", 0o755)
        os.chmod(repo / "test/git-hooks/heal-worktree.sh", 0o755)
        git("add", ".", cwd=repo, env=env)
        git("commit", "-qm", "hostile branch payloads", cwd=repo, env=env)
        git("checkout", "-q", "main", cwd=repo, env=env)

        # Historical probe: install the preserved bytes only in an isolated
        # throwaway clone to prove that the malicious branch payload is reached
        # by the old behavior. This is not product setup or a migration path.
        legacy_repo = sandbox / "legacy-repo"
        git("clone", "-q", str(repo), str(legacy_repo), cwd=sandbox, env=env)
        legacy_candidate = legacy_repo / ".git/hooks/post-checkout"
        legacy_candidate.write_bytes(legacy)
        os.chmod(legacy_candidate, 0o755)
        git("checkout", "-q", "-b", "hostile", "origin/hostile", cwd=legacy_repo, env=env)
        assert sentinel.exists(), "historical hook probe did not reach branch payload"
        sentinel.unlink()

        # These are the real Git checkout/worktree operations that used to fire
        # the installed shared post-checkout hook. The current shell-entry seam
        # installed no hook, so branch-controlled files remain inert.
        git("checkout", "-q", "hostile", cwd=repo, env=env)
        git("checkout", "-q", "main", cwd=repo, env=env)
        assert not sentinel.exists(), "a branch-controlled checkout payload ran"
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

        binary = os.environ.get("THEGN_TEST_BINARY")
        if binary:
            native_env = {**env, "XDG_STATE_HOME": str(sandbox / "state"),
                          "XDG_RUNTIME_DIR": str(sandbox / "run"), "THEGN_CHANNEL": "dev"}
            for key in ["THEGN_WORKTREE", "THEGN_PROFILE", "THEGN_CONFIG"]:
                native_env.pop(key, None)
            Path(native_env["XDG_RUNTIME_DIR"]).mkdir(mode=0o700)
            cfg = sandbox / "native.toml"
            cfg.write_text('worktrees_dir = ' + repr(str(sandbox / "native-worktrees")) + '\n')
            run(binary, "--config", str(cfg), "wt", "new", "native-hook-fixture",
                "--repo", str(repo), "--base", "hostile", "--json", cwd=repo, env=native_env)
            assert not sentinel.exists(), "native worktree creation ran candidate checkout code"
            print("THE-429 native worktree fixture: ok")
        else:
            print("THE-429 native worktree fixture: not run (set THEGN_TEST_BINARY)")

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

        for mode in ("fifo", "symlink", "regular"):
            if candidate.exists() or candidate.is_symlink():
                candidate.unlink()
            candidate.write_bytes(b"candidate before injected replacement\n")
            report = injected_replacement(repo, env, mode)
            assert "replaced" in report or "could not be opened" in report, report
            assert candidate.exists() or candidate.is_symlink()

        # Git queries are bounded, non-interactive, and preserve undecodable
        # bytes for an explicit refusal rather than raising a Unicode error.
        assert "output bound" in detector(repo, fake_git(sandbox, "oversized"))
        assert "timed out" in detector(repo, fake_git(sandbox, "hang"))
        assert "refusing inspection" in detector(repo, fake_git(sandbox, "invalid"))

        command_scope = dict(
            env,
            GIT_CONFIG_COUNT="1",
            GIT_CONFIG_KEY_0="core.hooksPath",
            GIT_CONFIG_VALUE_0=str(hooks),
        )
        assert "command-scope" in detector(repo, command_scope)

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
