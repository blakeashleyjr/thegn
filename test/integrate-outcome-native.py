#!/usr/bin/env python3
"""Explicit local native regression; preserves its private root as evidence.

Usage: python3 test/integrate-outcome-native.py --binary /absolute/thegn \
    --expected-sha256 <reviewed-binary-sha256>
No HOME/CODEX_HOME reassignment, daemon, network, live state, or automatic cleanup.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import tempfile
import time


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


class Harness:
    def __init__(self, binary, expected):
        self.binary = binary.resolve(strict=True)
        assert self.binary.is_file() and os.access(self.binary, os.X_OK)
        assert digest(self.binary) == expected, "binary hash does not match reviewed artifact"
        self.root = Path(tempfile.mkdtemp(prefix="thegn-integrate-native-"))
        os.chmod(self.root, 0o700)
        print(f"Evidence root: {self.root}", flush=True)
        self.records = []
        for name in ("state", "config", "runtime", "cache", "tmp", "app", "hooks", "evidence"):
            (self.root / name).mkdir(mode=0o700)
        self.git_config = self.root / "gitconfig"
        self.git_config.write_text("")
        self.git = Path(shutil.which("git")).resolve(strict=True)
        self.shell = Path(shutil.which("sh")).resolve(strict=True)
        self.env = {
            "PATH": os.pathsep.join(dict.fromkeys((str(self.git.parent), str(self.shell.parent)))),
            "LANG": "C.UTF-8",
            "XDG_STATE_HOME": str(self.root / "state"),
            "LOCALAPPDATA": str(self.root / "state"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_RUNTIME_DIR": str(self.root / "runtime"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "TMPDIR": str(self.root / "tmp"),
            "THEGN_DIR": str(self.root / "app"),
            "THEGN_NO_MIGRATE": "1",
            "GIT_CONFIG_GLOBAL": str(self.git_config),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_COUNT": "0",
            "GIT_CONFIG_PARAMETERS": "",
            "GIT_TEMPLATE_DIR": str(self.root / "hooks"),
            "GIT_OPTIONAL_LOCKS": "0",
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_AUTHOR_NAME": "Private Native Test",
            "GIT_AUTHOR_EMAIL": "private@example.invalid",
            "GIT_COMMITTER_NAME": "Private Native Test",
            "GIT_COMMITTER_EMAIL": "private@example.invalid",
            "NATIVE_GATE_JOURNAL": str(self.root / "gate-journal"),
        }
        assert "HOME" not in self.env and "CODEX_HOME" not in self.env
        self.config = self.root / "fixture.toml"
        self.db = self.root / "state/thegn/thegn.db"
        # Every application/Git state locator above was created under this exact
        # owned root before permitting schema creation in the explicit config.
        assert self.db.parent.parent.resolve().is_relative_to(self.root)
        assert not self.db.exists()
        self.write_config(snapshot=False, retain=False)
        (self.root / "evidence/binary.json").write_text(json.dumps({
            "binary": str(self.binary), "sha256": expected, "environment": self.env,
        }, indent=2))

    def run(self, argv, cwd, expected=0):
        assert cwd.resolve().is_relative_to(self.root), "non-private command cwd"
        number = len(self.records)
        outpath = self.root / f"evidence/{number:03d}.stdout"
        errpath = self.root / f"evidence/{number:03d}.stderr"
        with outpath.open("wb") as out, errpath.open("wb") as err:
            process = subprocess.Popen(argv, cwd=cwd, env=self.env,
                                       stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                                       start_new_session=True)
            try:
                code = process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                # Sole waiter; TimeoutExpired has not reaped this owned group
                # leader. Kill before waiting, never after a successful reap.
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
                raise AssertionError(f"private command timed out: {argv!r}")
        self.records.append({"argv": list(map(str, argv)), "cwd": str(cwd), "exit": code})
        (self.root / "evidence/commands.json").write_text(json.dumps(self.records, indent=2))
        # Runtime is bounded above. Output is retained to files and size-checked
        # after exit, not capped during execution; oversized evidence fails here.
        assert outpath.stat().st_size <= 1024 * 1024 and errpath.stat().st_size <= 1024 * 1024
        output = outpath.read_text(errors="replace") + errpath.read_text(errors="replace")
        assert (code != 0 if expected == "nonzero" else code == expected), output
        return output

    def git_run(self, repo, *args):
        return self.run([self.git, "-C", repo, *args], repo).strip()

    def native(self, repo, *args, expected=0):
        return self.run([self.binary, "--profile", "default", "--config", self.config, *args],
                        repo, expected)

    def write_config(self, snapshot, retain):
        self.config.write_text(f"""worktrees_dir = {json.dumps(str(self.root / 'worktrees'))}
repo_roots = [{json.dumps(str(self.root))}]
[database]
migration_authority = "any"
[daemon]
enabled = false
[automations]
enabled = false
[git]
override_gpg = true
auto_fetch = false
[sandbox.limits]
cpu = ""
cpu_total = "off"
memory = ""
memory_total = ""
[merge_queue]
enabled = true
target_branch = "main"
require_enqueue = true
snapshot_dirty = {str(snapshot).lower()}
gate_on = true
gate_command = "sh ./gate.sh"
gate_setup_command = ""
gate_reuse_worktree = true
gate_target_dir = {json.dumps(str(self.root / 'gate-target'))}
bisect_on_red = true
auto_land = false
organize_folders = false
on_landed = "{'off' if retain else 'expire'}"
merged_ttl_secs = 1
sign_commits = false
""")

    def repo(self, name):
        repo = self.root / name
        repo.mkdir()
        self.git_run(repo, "init", "-q", "-b", "main")
        self.git_run(repo, "config", "commit.gpgsign", "false")
        self.git_run(repo, "config", "core.hooksPath", str(self.root / "hooks"))
        (repo / "tracked").write_text("base\n")
        (repo / "gate.sh").write_text("""set -eu
test "$THEGN_GATE" = 1
test "$THEGN_WORKTREE" = "$PWD"
test "$(git rev-parse HEAD)" = "$THEGN_GATE_OID"
printf '%s\\n' "$THEGN_GATE_OID" >> "$NATIVE_GATE_JOURNAL"
if test -f regression.red; then
  printf '%s\\n' 'NATIVE_GATE_RED'
  exit 1
fi
printf '%s\\n' 'NATIVE_GATE_GREEN'
""")
        self.git_run(repo, "add", "tracked", "gate.sh")
        self.git_run(repo, "commit", "-q", "-m", "private base")
        return repo

    def worktree(self, repo, name):
        path = self.root / name
        self.git_run(repo, "worktree", "add", "-q", "-b", name, str(path))
        return path

    def rows(self):
        with sqlite3.connect(f"file:{self.db}?mode=ro", uri=True) as db:
            return db.execute("SELECT * FROM merge_queue ORDER BY worktree").fetchall()

    def logical_database(self):
        with sqlite3.connect(f"file:{self.db}?mode=ro", uri=True) as db:
            return tuple(db.iterdump())

    def state(self, paths):
        result = []
        for path in paths:
            gitdir = Path(self.git_run(path, "rev-parse", "--absolute-git-dir"))
            files = {str(p.relative_to(path)): digest(p) for p in path.rglob("*")
                     if p.is_file() and p.relative_to(path).parts[0] != ".git"}
            result.append((self.git_run(path, "rev-parse", "HEAD"),
                           digest(gitdir / "index"), files))
        return result, self.rows()

    def audit(self):
        with sqlite3.connect(f"file:{self.db}?mode=rw", uri=True) as db:
            db.executescript("""
CREATE TABLE fixture_outcomes(worktree TEXT, status TEXT, result_oid TEXT);
CREATE TRIGGER fixture_insert AFTER INSERT ON merge_queue BEGIN
  INSERT INTO fixture_outcomes VALUES(NEW.worktree, NEW.status, NEW.result_oid);
END;
CREATE TRIGGER fixture_update AFTER UPDATE ON merge_queue BEGIN
  INSERT INTO fixture_outcomes VALUES(NEW.worktree, NEW.status, NEW.result_oid);
END;
""")

    def outcome_events(self, path):
        with sqlite3.connect(f"file:{self.db}?mode=ro", uri=True) as db:
            return db.execute("SELECT status,result_oid FROM fixture_outcomes WHERE worktree=?",
                              (str(path),)).fetchall()

    def exercise(self):
        repo = self.repo("repo")
        foreign = self.repo("foreign")
        historical = self.worktree(repo, "historical")
        unrelated = self.worktree(foreign, "unrelated")
        for parent, path in ((repo, historical), (foreign, unrelated)):
            (path / "retention-canary").write_text(f"distinct retained data: {path.name}\n")
            self.git_run(path, "add", "retention-canary")
            self.git_run(path, "commit", "-q", "-m", "committed retention canary")
            self.git_run(parent, "merge", "--ff-only", path.name)
        candidate = self.worktree(repo, "candidate")
        (candidate / "regression.red").write_text("red\n")
        self.git_run(candidate, "add", "regression.red")
        self.git_run(candidate, "commit", "-q", "-m", "red candidate")
        self.native(repo, "config", "validate")
        effective = json.loads(self.native(repo, "config", "show", "--json"))
        assert effective["sandbox"]["limits"] == {
            "cpu": "", "cpu_total": "off", "memory": "", "memory_total": ""}
        assert effective["merge_queue"]["gate_command"] == "sh ./gate.sh"
        (self.root / "evidence/effective-config.json").write_text(json.dumps(effective, indent=2))
        for parent, path in ((repo, historical), (foreign, unrelated), (repo, candidate)):
            self.native(parent, "merge", "add", str(path))
        # These historical tips are genuine ancestors of their own main target.
        # Seed only status/timestamps in this already initialized private DB.
        with sqlite3.connect(f"file:{self.db}?mode=rw", uri=True) as db:
            for parent in (repo, foreign):
                db.execute("INSERT INTO workspaces(repo_path,name) VALUES(?,?)", (str(parent), parent.name))
            for parent, path in ((repo, historical), (foreign, unrelated), (repo, candidate)):
                db.execute("INSERT INTO worktrees(worktree,repo_path,branch,location) VALUES(?,?,?,'')",
                           (str(path), str(parent), path.name))
            for parent, path in ((repo, historical), (foreign, unrelated)):
                tip = self.git_run(parent, "rev-parse", "main")
                db.execute("UPDATE merge_queue SET status='landed',result_oid=?,updated_at=? WHERE worktree=?",
                           (tip, int(time.time()) - 3600, str(path)))
        untouched = self.state([historical, unrelated])
        before = self.state([repo, candidate, historical, unrelated])
        refs_before = self.git_run(repo, "for-each-ref", "--format=%(refname) %(objectname)", "refs/heads")
        database_before = self.logical_database()
        self.native(repo, "integrate", "--dry-run")
        assert self.state([repo, candidate, historical, unrelated]) == before
        assert self.logical_database() == database_before
        assert not (self.root / "gate-journal").exists()
        self.audit()
        output = self.native(repo, "integrate", "--yes", expected="nonzero")
        assert "NATIVE_GATE_RED" in output and "landed candidate" not in output
        assert "not advanced by this fold" in output
        assert self.git_run(repo, "for-each-ref", "--format=%(refname) %(objectname)", "refs/heads") == refs_before
        assert self.state([repo, candidate])[0] == before[0][:2]
        assert self.state([historical, unrelated])[0] == untouched[0]
        historical_rows = [row for row in untouched[1] if row[0] != str(candidate)]
        assert [row for row in self.rows() if row[0] != str(candidate)] == historical_rows
        events = self.outcome_events(candidate)
        assert events and all(status not in ("landed", "queued") and oid is None for status, oid in events)
        with sqlite3.connect(f"file:{self.db}?mode=ro", uri=True) as db:
            row = db.execute("SELECT status,result_oid FROM merge_queue WHERE worktree=?", (str(candidate),)).fetchone()
            assert row[0] in ("gate_error", "gate_failed") and row[1] is None
        assert len((self.root / "gate-journal").read_text().splitlines()) >= 2

        # Actual accepted land uses explicit retention. auto_land=false must not
        # disable the explicit manual integrate operation.
        self.git_run(candidate, "rm", "regression.red")
        (candidate / "feature").write_text("native accepted change\n")
        self.git_run(candidate, "add", "feature")
        self.git_run(candidate, "commit", "-q", "-m", "repair candidate")
        self.write_config(snapshot=False, retain=True)
        self.native(repo, "merge", "retry", "--worktree", str(candidate))
        old_main = self.git_run(repo, "rev-parse", "main")
        self.native(repo, "integrate", "--yes")
        tip = self.git_run(repo, "rev-parse", "main")
        assert tip != old_main and (repo / "feature").read_text() == "native accepted change\n"
        self.git_run(repo, "merge-base", "--is-ancestor", "candidate", "main")
        assert self.outcome_events(candidate)[-1] == ("landed", tip)
        assert candidate.is_dir() and self.git_run(candidate, "symbolic-ref", "--short", "HEAD") == "candidate"

        queued = self.worktree(repo, "dirty-queued")
        unqueued = self.worktree(repo, "dirty-unqueued")
        for path in (queued, unqueued):
            (path / "tracked").write_text("staged\n")
            self.git_run(path, "add", "tracked")
            (path / "tracked").write_text("unstaged\n")
            (path / "untracked").write_text("untracked private state\n")
        self.write_config(snapshot=True, retain=True)
        self.native(repo, "merge", "add", str(queued))
        before = self.state([queued, unqueued])
        database_before = self.logical_database()
        journal = (self.root / "gate-journal").read_bytes()
        preview = self.native(repo, "integrate", "--dry-run")
        assert "snapshot only after confirmation" in preview
        assert self.state([queued, unqueued]) == before
        assert self.logical_database() == database_before
        self.native(repo, "integrate", expected="nonzero")
        assert self.state([queued, unqueued]) == before
        assert self.logical_database() == database_before
        assert (self.root / "gate-journal").read_bytes() == journal
        self.native(repo, "integrate", "--yes")
        assert self.state([unqueued])[0][0] == before[0][1]
        assert self.git_run(queued, "rev-parse", "HEAD") != before[0][0][0]
        assert (repo / "tracked").read_text() == "unstaged\n"
        assert self.outcome_events(queued)[-1] == ("landed", self.git_run(repo, "rev-parse", "main"))
        assert self.state([historical, unrelated])[0] == untouched[0]
        assert digest(self.binary) == json.loads((self.root / "evidence/binary.json").read_text())["sha256"]


def main():
    assert os.name == "posix", "this explicit native harness requires Unix process-group supervision"
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--expected-sha256", required=True)
    args = parser.parse_args()
    harness = Harness(args.binary, args.expected_sha256)
    try:
        harness.exercise()
    except BaseException as error:
        (harness.root / "evidence/result.json").write_text(json.dumps({"passed": False, "error": repr(error)}))
        raise
    (harness.root / "evidence/result.json").write_text(json.dumps({"passed": True, "commands": len(harness.records)}))
    print(f"Native integration regression passed; evidence retained at {harness.root}")


if __name__ == "__main__":
    main()
