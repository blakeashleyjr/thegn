#!/usr/bin/env python3
"""Print only exact-name and label proof flags from Apple container JSON."""

import json
import subprocess
import sys


def rows_from_json(data):
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        for key in ("containers", "Containers", "items"):
            if isinstance(data.get(key), list):
                return data[key]
    return []


def names(row):
    result = []
    for key in ("name", "Name", "names", "Names"):
        value = row.get(key)
        result.extend(value if isinstance(value, list) else [value])
    return [str(value).lstrip("/") for value in result if value is not None]


def has_label(value, marker):
    if isinstance(value, dict):
        for key, item in value.items():
            if str(key).lower() == "labels":
                if isinstance(item, dict) and item.get("thegn.spike") == marker:
                    return True
                if isinstance(item, list) and "thegn.spike=" + marker in item:
                    return True
            if has_label(item, marker):
                return True
    elif isinstance(value, list):
        return any(has_label(item, marker) for item in value)
    return False


def proof_flags(stdout, expected_name, marker):
    """Return parseability and proof for only this exact container row."""
    try:
        data = json.loads(stdout)
    except (TypeError, ValueError):
        return False, False, False
    rows = rows_from_json(data)
    matched = [
        row for row in rows
        if isinstance(row, dict) and expected_name in names(row)
    ]
    return True, bool(matched), any(has_label(row, marker) for row in matched)


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: apple-container-list-probe.py NAME MARKER")
    name, marker = sys.argv[1:]
    proc = subprocess.run(
        ["container", "ls", "-a", "--format", "json"],
        capture_output=True,
        text=True,
    )
    parseable, exact_name, label_visible = proof_flags(proc.stdout, name, marker)
    print("__THEGN_LIST_RC=%d" % proc.returncode)
    print("__THEGN_LIST_PARSEABLE=" + str(parseable).lower())
    print("__THEGN_LIST_NAME=" + str(exact_name).lower())
    print("__THEGN_LIST_LABEL=" + str(label_visible).lower())


if __name__ == "__main__":
    main()
