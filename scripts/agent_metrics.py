"""Summarize task-attributed AI development measurements for accepted issues."""

import argparse
import csv
import json
import math
from collections import defaultdict
from pathlib import Path
from statistics import median

METRICS = ("uncached_input_tokens", "minutes_to_first_green", "combined_repair_rounds")
FIELDS = ("cohort", "kind", "issue", "evidence", *METRICS)


def codex_inputs(paths):
    """Use the final cumulative counter once per issue-attributed session."""
    sessions = {}
    for path in paths:
        session_id = None
        usage = None
        with path.open("rb") as stream:
            for line in stream:
                if b'"session_meta"' not in line and b'"token_count"' not in line:
                    continue
                event = json.loads(line)
                payload = event.get("payload")
                if not isinstance(payload, dict):
                    continue
                if event.get("type") == "session_meta":
                    session_id = payload["id"]
                    if payload.get("forked_from_id"):
                        raise ValueError(f"forked history requires separate attribution: {path.name}")
                if event.get("type") == "event_msg" and payload.get("type") == "token_count":
                    info = payload.get("info") or {}
                    usage = info.get("total_token_usage") or usage
        if not session_id or usage is None or session_id in sessions:
            raise ValueError(f"missing counters, session ID, or repeated session: {path.name}")
        sessions[session_id] = usage["input_tokens"] - usage["cached_input_tokens"]
    return sum(sessions.values())


def summarize(path):
    groups = defaultdict(list)
    seen = set()
    with path.open(newline="") as stream:
        reader = csv.DictReader(stream)
        if reader.fieldnames != list(FIELDS):
            raise ValueError("expected CSV header: " + ",".join(FIELDS))
        for row in reader:
            issue = row["issue"]
            if (None in row or None in row.values() or row["cohort"] not in ("before", "after")
                    or not all(row[key].strip() for key in ("kind", "issue", "evidence"))):
                raise ValueError(f"invalid or incomplete row at line {reader.line_num}")
            if issue in seen:
                raise ValueError(f"duplicate accepted issue: {issue}")
            seen.add(issue)
            values = {}
            for key in METRICS:
                if row[key].strip():
                    value = float(row[key]) if key == "minutes_to_first_green" else int(row[key])
                    if value < 0 or not math.isfinite(value):
                        raise ValueError(f"invalid {key} for {issue}")
                    values[key] = value
            groups[row["kind"], row["cohort"]].append(values)
    for (kind, cohort), rows in sorted(groups.items()):
        print(f"{kind} / {cohort} ({len(rows)} accepted)")
        for key in METRICS:
            values = [r[key] for r in rows if key in r]
            summary = f"{median(values):,.2f}".rstrip("0").rstrip(".") if values else "unavailable"
            print(f"  {key}: median {summary} ({len(values)}/{len(rows)} measured)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("csv", nargs="?", type=Path)
    group.add_argument("--codex-rollouts", nargs="+", type=Path, help="total uncached input across these issue-specific sessions")
    try:
        args = parser.parse_args()
        if args.codex_rollouts:
            print(codex_inputs(args.codex_rollouts))
        else:
            summarize(args.csv)
    except (OSError, ValueError, KeyError, csv.Error) as error:
        parser.exit(1, f"{error}\n")
