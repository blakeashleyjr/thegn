#!/usr/bin/env python3
"""Synthetic Git/design experiments. This is NOT a Thegn sync implementation.

Runs only inside a new TemporaryDirectory with empty HOME and Git config.
No network, application state, real checkout mutations, hooks, or credentials.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def run_experiments(root):
    home = root / "home"
    home.mkdir()
    env = {"PATH": os.environ["PATH"], "HOME": str(home),
           "XDG_CONFIG_HOME": str(home / "config"),
           "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
           "GIT_TERMINAL_PROMPT": "0", "LC_ALL": "C"}

    def git(repo, *args, input=None):
        return subprocess.run(
            ["git", "-c", "core.hooksPath=" + str(root / "no-hooks"),
             "-c", "user.name=Audit", "-c", "user.email=audit@example.invalid",
             "-c", "commit.gpgSign=false", "-C", str(repo), *args],
            env=env, input=input, capture_output=True, check=True, timeout=30,
        ).stdout

    checks = []

    def check(name, condition):
        if not condition:
            raise AssertionError(name)
        checks.append({"check": name, "result": "pass"})

    host = root / "host"
    host.mkdir()
    git(host, "init", "-q", "-b", "main")
    (host / "tracked.txt").write_text("committed\n")
    (host / ".gitignore").write_text("ignored.bin\n")
    git(host, "add", ".")
    git(host, "commit", "-qm", "synthetic baseline")
    linked = root / "linked"
    git(host, "worktree", "add", "-qb", "linked", str(linked))
    bad = root / "copied-linked"
    shutil.copytree(linked, bad)
    check("copying linked directory retains host common Git directory",
          git(bad, "rev-parse", "--path-format=absolute", "--git-common-dir") ==
          git(host, "rev-parse", "--path-format=absolute", "--git-common-dir"))

    (linked / "tracked.txt").write_text("host dirty baseline\n")
    (linked / "untracked.txt").write_text("host untracked baseline\n")
    (linked / "ignored.bin").write_bytes(b"synthetic ignored secret")
    private = root / "private"
    # --no-local uses the Git transport, avoiding shared objects/alternates.
    git(root, "clone", "-q", "--no-local", str(linked), str(private))
    git(private, "remote", "remove", "origin")
    selected = git(linked, "ls-files", "-z", "--cached", "--others",
                   "--exclude-standard").split(b"\0")
    for raw in filter(None, selected):
        name = raw.decode()
        shutil.copyfile(linked / name, private / name)
    git(private, "add", ".")
    git(private, "commit", "-qm", "private imported file baseline")
    check("private .git is a directory inside the private copy", (private / ".git").is_dir())
    check("private common Git directory differs from host",
          git(private, "rev-parse", "--path-format=absolute", "--git-common-dir") !=
          git(host, "rev-parse", "--path-format=absolute", "--git-common-dir"))
    check("private objects have no alternates", not (private / ".git/objects/info/alternates").exists())
    host_object_ids = {(p.stat().st_dev, p.stat().st_ino)
                       for p in (host / ".git/objects").rglob("*") if p.is_file()}
    check("private object files share no host object inodes",
          all((p.stat().st_dev, p.stat().st_ino) not in host_object_ids
              for p in (private / ".git/objects").rglob("*") if p.is_file()))
    check("ignored file excluded; dirty and untracked files imported",
          not (private / "ignored.bin").exists() and
          (private / "tracked.txt").read_text() == "host dirty baseline\n" and
          (private / "untracked.txt").exists())

    def snapshot(directory):
        # Fixed synthetic fixture scope; NOT a hostile-path export parser.
        return {p.name: p.read_bytes() for p in directory.iterdir()
                if p.is_file() and p.name not in ("ignored.bin", ".git")}

    baseline = snapshot(linked)
    host_refs = git(host, "show-ref")
    host_index = (linked / ".git").read_bytes()
    (private / "tracked.txt").write_text("sandbox edited\n")
    (private / "untracked.txt").unlink()
    (private / "new.txt").write_text("sandbox untracked\n")
    (private / "binary.dat").write_bytes(bytes(range(256)))
    git(private, "add", "tracked.txt", "untracked.txt")
    git(private, "commit", "-qm", "private sandbox commit")
    git(private, "branch", "sandbox-only")
    desired = snapshot(private)
    check("sandbox commits and refs leave host refs unchanged", git(host, "show-ref") == host_refs)
    check("host tracked and untracked baseline untouched before sync", snapshot(linked) == baseline)

    export = root / "export"
    export.mkdir()
    git(export, "init", "-q", "-b", "export")
    for name, data in baseline.items():
        (export / name).write_bytes(data)
    git(export, "add", ".")
    git(export, "commit", "-qm", "file baseline")
    for name in baseline:
        if name not in desired:
            (export / name).unlink()
    for name, data in desired.items():
        (export / name).write_bytes(data)
    git(export, "add", "-A")
    patch = git(export, "diff", "--cached", "--binary", "--no-ext-diff", "--no-renames")
    check("file patch includes tracked, deleted untracked, new and binary changes",
          all(x in patch for x in [b"tracked.txt", b"untracked.txt", b"new.txt",
                                   b"binary.dat", b"GIT binary patch"]))
    check("file patch contains no sandbox refs or commit history",
          b"sandbox-only" not in patch and b"private sandbox commit" not in patch)

    # A fresh process independently reads a durable manifest and copy.
    manifest = root / "manifest.json"
    manifest.write_text(json.dumps({name: hashlib.sha256(data).hexdigest()
                                   for name, data in desired.items()}, sort_keys=True))
    subprocess.run([shutil.which("python3"), "-c",
                    "import hashlib,json,pathlib,sys; p=pathlib.Path(sys.argv[1]); "
                    "m=json.loads(pathlib.Path(sys.argv[2]).read_text()); "
                    "assert all(hashlib.sha256((p/n).read_bytes()).hexdigest()==h "
                    "for n,h in m.items())", str(private), str(manifest)],
                   env=env, check=True, timeout=30)
    check("private files and Git repo survive reader process replacement", (private / ".git/HEAD").exists())

    touched = {name for name in baseline.keys() | desired.keys()
               if baseline.get(name) != desired.get(name)}
    (linked / "tracked.txt").write_text("concurrent host edit\n")
    current = snapshot(linked)
    conflicts = {name for name in touched if current.get(name) != baseline.get(name)}
    check("baseline comparison detects host edit without overwriting either side",
          conflicts == {"tracked.txt"} and current["tracked.txt"] == b"concurrent host edit\n" and
          snapshot(private) == desired)
    (linked / "tracked.txt").write_bytes(baseline["tracked.txt"])
    git(linked, "apply", "--check", "--binary", input=patch)
    git(linked, "apply", "--binary", input=patch)
    check("patch applies tracked and untracked edits and binary bytes", snapshot(linked) == desired)
    check("patch leaves host Git refs and pointer unchanged",
          git(host, "show-ref") == host_refs and (linked / ".git").read_bytes() == host_index)
    check("ignored file is unchanged by sync", (linked / "ignored.bin").read_bytes() == b"synthetic ignored secret")
    check("advanced baseline makes repeat sync empty", snapshot(linked) == snapshot(private))

    # Design recovery predicate: only known old/new states can be resumed.
    partial = dict(baseline)
    partial["tracked.txt"] = desired["tracked.txt"]
    check("interrupted journal model recognizes old/new states",
          all(partial.get(name) in (baseline.get(name), desired.get(name)) for name in touched))
    partial["tracked.txt"] = b"third-party edit after interruption\n"
    check("interrupted journal model refuses unexpected concurrent bytes",
          any(partial.get(name) not in (baseline.get(name), desired.get(name)) for name in touched))
    return {"scope": "synthetic Git primitives and recovery predicates; no Thegn private-copy implementation exists",
            "checks": checks, "passed": len(checks), "patch_sha256": hashlib.sha256(patch).hexdigest()}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="thegn-private-copy-audit-") as temporary:
        result = run_experiments(Path(temporary))
    print(json.dumps(result, indent=2, sort_keys=True))
