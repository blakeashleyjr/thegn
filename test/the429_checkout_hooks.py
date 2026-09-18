#!/usr/bin/env python3
"""Focused THE-429 fixtures; intentionally independent of Cargo and builds."""

from __future__ import annotations

import os
import shlex
import hashlib
import shutil
import stat
import subprocess
import sys
import tempfile
import time
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


def shell_entry_body() -> str:
    # Execute the actual hookExtras body, substituting only the two immutable
    # Nix store references. This detects an installer reintroduced beside the
    # detector, which invoking the detector alone would miss.
    flake = (ROOT / "flake.nix").read_text()
    body = flake.split("      hookExtras = ''\n", 1)[1].split("\n      '';", 1)[0]
    body = body.replace("${pkgs.python3}/bin/python3", shlex.quote(sys.executable))
    body = body.replace("${./nix/detect-legacy-post-checkout.py}", shlex.quote(str(DETECTOR)))
    body = body.replace("''${", "${")
    return body


def shell_entry(repo: Path, env: dict[str, str]) -> str:
    return run("sh", "-c", shell_entry_body(), cwd=repo, env={**env, "CI": ""}).stderr


def shell_entry_async(repo: Path, env: dict[str, str]) -> subprocess.Popen[str]:
    return subprocess.Popen(
        ["sh", "-c", shell_entry_body()],
        cwd=repo,
        env={**env, "CI": ""},
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def fake_git(sandbox: Path, mode: str) -> dict[str, str]:
    fake_bin = sandbox / f"fake-git-{mode}"
    fake_bin.mkdir()
    executable = fake_bin / "git"
    executable.write_text(
        "#!/usr/bin/env python3\n"
        "import os\n"
        "import sys\n"
        "import time\n"
        f"mode = {mode!r}\n"
        "if mode == 'hang':\n"
        "    time.sleep(30)\n"
        "elif mode == 'oversized':\n"
        "    sys.stdout.write('x' * (128 * 1024))\n"
        "elif mode == 'invalid':\n"
        "    sys.stdout.buffer.write(b'/tmp/invalid-\\xff\\n')\n"
        "elif mode == 'eof-race':\n"
        "    sys.stdout.close()\n"
        "    sys.stderr.close()\n"
        "    time.sleep(0.05)\n"
        "elif mode.startswith('fork-holder'):\n"
        "    holder = os.fork()\n"
        "    if holder == 0:\n"
        "        with open(os.environ['THE429_HOLDER_PID'], 'w') as pid_file:\n"
        "            pid_file.write(str(os.getpid()))\n"
        "            pid_file.flush()\n"
        "        time.sleep(30)\n"
        "        os._exit(0)\n"
        "    os._exit(0)\n"
    )
    executable.chmod(0o755)
    environment = os.environ.copy()
    environment["PATH"] = f"{fake_bin}{os.pathsep}{environment['PATH']}"
    if mode.startswith("fork-holder"):
        environment["THE429_HOLDER_PID"] = str(sandbox / f"holder-{mode}.pid")
    return environment


def detector_module():
    import importlib.util

    spec = importlib.util.spec_from_file_location("the429_detector_test", DETECTOR)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def remove_entry(path: Path) -> None:
    try:
        entry = os.lstat(path)
    except FileNotFoundError:
        return
    if stat.S_ISDIR(entry.st_mode) and not stat.S_ISLNK(entry.st_mode):
        shutil.rmtree(path)
    else:
        path.unlink()


def snapshot(path: Path) -> tuple[object, ...]:
    """Snapshot without opening a FIFO or following a symlink."""

    entry = os.lstat(path)
    identity = (
        entry.st_dev,
        entry.st_ino,
        entry.st_mode,
        entry.st_uid,
        entry.st_gid,
        entry.st_size,
        entry.st_mtime_ns,
    )
    if stat.S_ISLNK(entry.st_mode):
        return ("symlink", os.readlink(path), identity)
    if stat.S_ISREG(entry.st_mode):
        return ("regular", path.read_bytes(), identity)
    if stat.S_ISDIR(entry.st_mode):
        return ("directory", identity)
    return ("special", identity)


def process_running(pid: int) -> bool:
    """Treat a Linux zombie as reaped for the fixture's process-tree check."""

    proc_stat = Path(f"/proc/{pid}/stat")
    try:
        fields = proc_stat.read_text().split()
    except FileNotFoundError:
        return False
    return len(fields) < 3 or fields[2] != "Z"


def assert_process_gone(pid_file: Path) -> None:
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        if pid_file.exists():
            pid = int(pid_file.read_text())
            if not process_running(pid):
                return
        time.sleep(0.02)
    assert pid_file.exists(), "fork-holder did not publish its pid"
    assert not process_running(int(pid_file.read_text())), "Git process-group descendant survived cleanup"


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

        # The former checkout hook also seeded this path. Exercise the actual
        # shell-entry body around real checkout operations and retain every
        # collision type without opening the FIFO case.
        config_path = worktree / ".pre-commit-config.yaml"
        config_target = sandbox / "config-target"
        config_cases = ("regular", "directory", "valid-symlink", "dangling-symlink", "fifo", "hardlink")
        for case in config_cases:
            remove_entry(config_path)
            if config_target.exists() or config_target.is_symlink():
                remove_entry(config_target)
            if case == "regular":
                config_path.write_bytes(b"foreign regular config\n")
            elif case == "directory":
                config_path.mkdir()
            elif case == "valid-symlink":
                config_target.write_bytes(b"valid symlink target\n")
                config_path.symlink_to(config_target)
            elif case == "dangling-symlink":
                config_path.symlink_to(config_target)
            elif case == "fifo":
                os.mkfifo(config_path)
            else:
                config_target.write_bytes(b"hardlink bytes\n")
                fixed_ns = 1_700_000_000_123_456_789
                os.utime(config_target, ns=(fixed_ns, fixed_ns))
                config_path.hardlink_to(config_target)
            before_config_path = snapshot(config_path)
            before_config_target = snapshot(config_target) if config_target.exists() else None
            assert "main checkout" in shell_entry(worktree, env)
            git("checkout", "-q", "hostile", cwd=worktree, env=env)
            git("checkout", "-q", "fixture-clean", cwd=worktree, env=env)
            assert snapshot(config_path) == before_config_path, case
            if before_config_target is not None:
                assert snapshot(config_target) == before_config_target, case
        remove_entry(config_path)
        remove_entry(config_target)

        # The no-op shell setup must remain harmless even while a real checkout
        # changes the selected branch. This overlaps several immutable shell
        # entries with ordinary checkout operations around a foreign config;
        # there is no installer/uninstaller race left to win.
        concurrent_config = repo / ".pre-commit-config.yaml"
        concurrent_config.write_bytes(b"foreign concurrent shell config\n")
        concurrent_before = snapshot(concurrent_config)
        shell_entries = [shell_entry_async(repo, env) for _ in range(3)]
        for branch in ("hostile", "main", "hostile", "main"):
            git("checkout", "-q", branch, cwd=repo, env=env)
        for process in shell_entries:
            stdout, stderr = process.communicate(timeout=10)
            assert process.returncode == 0, (stdout, stderr)
        assert snapshot(concurrent_config) == concurrent_before
        assert not sentinel.exists(), "a concurrent branch-controlled checkout payload ran"
        concurrent_config.unlink()

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
            native_root = sandbox / "native-worktrees"
            if native_root.exists():
                assert not list(native_root.rglob(".pre-commit-config.yaml")), (
                    "native worktree creation seeded branch-controlled config"
                )
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
        os.chmod(candidate, 0o755)
        before = metadata(candidate)
        before_mode = stat.S_IMODE(os.lstat(candidate).st_mode)
        assert "foreign post-checkout" in detector(repo, env)
        assert metadata(candidate) == before
        assert stat.S_IMODE(os.lstat(candidate).st_mode) == before_mode
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

        # EOF may arrive just before waitpid observes a successful leader exit;
        # the helper must reap that leader rather than killing it spuriously.
        detector_impl = detector_module()
        eof_env = fake_git(sandbox, "eof-race")
        original_path = os.environ.get("PATH")
        try:
            os.environ["PATH"] = eof_env["PATH"]
            eof_result = detector_impl.git(repo, "--eof-race")
        finally:
            if original_path is None:
                os.environ.pop("PATH", None)
            else:
                os.environ["PATH"] = original_path
        assert eof_result.returncode == 0, eof_result

        # A direct fake-Git leader exits while a descendant retains both pipes.
        # Repeat it to catch process/fd growth and require the exact isolated
        # process group to be gone before continuing.
        for index in range(3):
            holder_env = fake_git(sandbox, f"fork-holder-{index}")
            started = time.monotonic()
            assert "timed out" in detector(repo, holder_env)
            assert time.monotonic() - started < 4.5, "bounded Git cleanup exceeded its deadline"
            assert_process_gone(Path(holder_env["THE429_HOLDER_PID"]))

        # Repository identity must come from cwd, not ambient directory,
        # index, object, namespace, discovery, or config redirection variables.
        redirected = sandbox / "redirected-repo"
        redirected.mkdir()
        git("init", "-q", cwd=redirected, env=env)
        redirected_candidate = redirected / ".git/hooks/post-checkout"
        redirected_candidate.write_bytes(legacy)
        redirected_before = snapshot(redirected_candidate)
        redirected_env = {
            **env,
            "GIT_DIR": str(redirected / ".git"),
            "GIT_WORK_TREE": str(redirected),
            "GIT_COMMON_DIR": str(redirected / ".git"),
            "GIT_INDEX_FILE": str(redirected / ".git/index"),
            "GIT_OBJECT_DIRECTORY": str(redirected / ".git/objects"),
            "GIT_ALTERNATE_OBJECT_DIRECTORIES": str(redirected / ".git/objects"),
            "GIT_NAMESPACE": "redirected",
            "GIT_CEILING_DIRECTORIES": str(redirected.parent),
            "GIT_DISCOVERY_ACROSS_FILESYSTEM": "1",
            "GIT_CONFIG_SYSTEM": str(redirected / ".git/config"),
        }
        remove_entry(candidate)
        assert "no legacy post-checkout hook" in detector(repo, redirected_env)
        assert snapshot(redirected_candidate) == redirected_before
        candidate.write_bytes(b"foreign after environment scrub\n")

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

        # A symlinked parent is also out of scope; the detector must not follow
        # it merely because the target contains an otherwise valid hook.
        real_hooks = repo / ".git/hooks-real"
        hooks.rename(real_hooks)
        hooks.symlink_to(real_hooks, target_is_directory=True)
        try:
            hooks_before = metadata(hooks)
            assert "not a real local directory" in detector(repo, env)
            assert metadata(hooks) == hooks_before
        finally:
            hooks.unlink()
            real_hooks.rename(hooks)

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
