"""Refresh or check the current worktree's CodeGraph index."""

import argparse
import json
from pathlib import Path
import subprocess
import sys
import time


def sync():
    if not Path(".codegraph/codegraph.db").is_file():
        subprocess.run(["codegraph", "init", "."], check=True)
    # A connected watcher may be writing exactly the change we want to sync.
    # Retry lock contention without stopping the daemon or touching its lock.
    for attempt in range(3):
        run = subprocess.run(["codegraph", "sync", "."], capture_output=True, text=True)
        if run.returncode == 0:
            return
        message = run.stdout + run.stderr
        if "database is locked" not in message.lower() or attempt == 2:
            sys.exit(message.strip() or f"CodeGraph sync exited {run.returncode}.")
        time.sleep(1 + attempt)


def check():
    root = Path.cwd().resolve()
    if not (root / ".codegraph/codegraph.db").is_file():
        sys.exit("No CodeGraph index in this worktree. Run just graph-sync.")
    result = subprocess.check_output(["codegraph", "status", "--json", "."])
    report = json.loads(result)
    index = report.get("index") or {}
    if not report.get("initialized") or Path(report["projectPath"]).resolve() != root:
        sys.exit("CodeGraph reported a different worktree. Run just graph-sync.")
    if index.get("reindexRecommended"):
        sys.exit("The extractor changed. Run codegraph index . and recheck.")
    pending = report.get("pendingChanges") or {}
    if (index.get("state") != "complete" or report.get("worktreeMismatch")
            or any(pending.get(k) != 0 for k in ("added", "modified", "removed"))):
        sys.exit("CodeGraph reports pending changes or an incomplete index. Run just graph-sync.")
    print(f"Current CodeGraph index: {report['fileCount']} files, {report['nodeCount']} nodes.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sync", action="store_true")
    try:
        if parser.parse_args().sync:
            sync()
        check()
    except (OSError, KeyError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"Could not check the CodeGraph index: {error}")
