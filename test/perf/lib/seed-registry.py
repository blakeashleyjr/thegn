#!/usr/bin/env python3
"""Register fixture linked worktrees in the isolated thegn database."""

import os
import pwd
import sqlite3
import sys
import time
from pathlib import Path


REQUIRED_COLUMNS = {
    "worktree",
    "session_name",
    "tab_name",
    "repo_path",
    "branch",
    "agent",
    "created_at",
    "location",
    "position",
}


def within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: seed-registry.py DB REPO_ROOT WORKTREES", file=sys.stderr)
        return 2

    perf_tmp = os.environ.get("PERF_TMP")
    if not perf_tmp:
        print("seed-registry: PERF_TMP is unset", file=sys.stderr)
        return 2
    try:
        tmp_root = Path(perf_tmp).resolve(strict=True)
        db_path = Path(sys.argv[1]).resolve()
        repo_root = Path(sys.argv[2]).resolve(strict=True)
        live_state = (Path(pwd.getpwuid(os.getuid()).pw_dir) / ".local/state/thegn").resolve()
    except OSError as error:
        print(f"seed-registry: cannot resolve fixture paths: {error}", file=sys.stderr)
        return 2
    if not tmp_root.name.startswith("tg-perf."):
        print(f"seed-registry: refusing non-perf temp root: {tmp_root}", file=sys.stderr)
        return 2
    if within(db_path, live_state):
        print(f"seed-registry: refusing live thegn DB: {db_path}", file=sys.stderr)
        return 2
    if not within(db_path, tmp_root):
        print(f"seed-registry: refusing DB outside PERF_TMP: {db_path}", file=sys.stderr)
        return 2
    if not db_path.is_file():
        print(f"seed-registry: DB does not exist: {db_path}", file=sys.stderr)
        return 2
    if not within(repo_root, tmp_root):
        print(f"seed-registry: refusing repo outside PERF_TMP: {repo_root}", file=sys.stderr)
        return 2
    try:
        count = int(sys.argv[3])
    except ValueError:
        print(f"seed-registry: invalid worktree count: {sys.argv[3]}", file=sys.stderr)
        return 2
    if count < 0:
        print("seed-registry: worktree count must be non-negative", file=sys.stderr)
        return 2

    slug = repo_root.name
    connection = None
    try:
        connection = sqlite3.connect(db_path)
        for registered_path, registered_slug in connection.execute(
            "SELECT repo_path, slug FROM repo_slugs"
        ):
            if os.path.realpath(registered_path) == str(repo_root):
                slug = registered_slug
                break
        columns = {
            row[1] for row in connection.execute("PRAGMA table_info(worktrees)")
        }
        missing = REQUIRED_COLUMNS - columns
        if missing:
            raise RuntimeError(
                "worktrees schema mismatch; missing columns: " + ", ".join(sorted(missing))
            )

        with connection:
            for index in range(1, count + 1):
                branch = f"wt-{index}"
                worktree = (tmp_root / "worktrees" / branch).resolve(strict=True)
                if not within(worktree, tmp_root):
                    raise RuntimeError(f"fixture worktree escapes PERF_TMP: {worktree}")
                connection.execute(
                    """INSERT INTO worktrees
                       (worktree, session_name, tab_name, repo_path, branch,
                        agent, created_at, location, position)
                       VALUES (?, ?, ?, ?, ?, '', ?, '', ?)
                       ON CONFLICT(worktree) DO UPDATE SET
                         session_name=excluded.session_name,
                         tab_name=excluded.tab_name,
                         repo_path=excluded.repo_path,
                         branch=excluded.branch,
                         agent=excluded.agent,
                         created_at=excluded.created_at,
                         location=excluded.location,
                         position=excluded.position""",
                    (
                        str(worktree),
                        str(repo_root),
                        f"{slug}/{branch}",
                        str(repo_root),
                        branch,
                        int(time.time()),
                        index,
                    ),
                )
    except (OSError, sqlite3.Error, RuntimeError) as error:
        print(f"seed-registry: {error}", file=sys.stderr)
        return 1
    finally:
        if connection is not None:
            connection.close()

    print(f"seed-registry: registered {count} worktrees in {db_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
