#!/usr/bin/env python3
"""Offline delivery-index validation and read-only reconciliation reporting."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sys
import tempfile
from collections import Counter, defaultdict
from datetime import date
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
INDEX = Path("delivery/index.json")
SCHEMA = Path("delivery/schema.json")
ISSUES = Path("delivery/issues.json")
ISSUES_SCHEMA = Path("delivery/issues-schema.json")
ACTIVE_LIFECYCLES = {"proposed", "active", "delivered-awaiting-archive"}
ARCHIVE_RECONCILIATION_DAYS = 7
MAX_LINEAR_DRIFT_ITEMS = 100
LIVE_COUNT_DOCS = (Path("docs/PORTFOLIO.md"), Path("docs/help/release-channels.md"))
LIVE_COUNT_PATTERNS = (
    re.compile(
        r"\b\d+\s+(?:active changes?|accepted capability specs?|passed(?:\s+and)?|failed)\b",
        re.IGNORECASE,
    ),
    re.compile(
        r"\b(?:active task ledger|live queue|priority distribution)\b[^\n]*\b\d+\b",
        re.IGNORECASE,
    ),
    re.compile(r"\bowns\s+\d+\b", re.IGNORECASE),
    re.compile(r"\b(?:ten|twenty|thirty|forty|fifty)\s+(?:projects?|milestones?)\b", re.IGNORECASE),
)
TASK_RE = re.compile(r"^- \[([ xX])\]", re.MULTILINE)


class DuplicateKey(ValueError):
    pass


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateKey(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(), object_pairs_hook=_unique_object)
    except (OSError, json.JSONDecodeError, DuplicateKey) as error:
        raise ValueError(f"{path}: invalid JSON: {error}") from error


def _kind(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, int):
        return "integer"
    if isinstance(value, float):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return type(value).__name__


def validate_schema(value: Any, schema: dict[str, Any], at: str = "$index") -> list[str]:
    """Validate the small JSON-Schema subset used by delivery/schema.json."""
    errors: list[str] = []
    expected = schema.get("type")
    if expected and _kind(value) != expected:
        return [f"{at}: expected {expected}, got {_kind(value)}"]
    if "const" in schema and value != schema["const"]:
        errors.append(f"{at}: expected constant {schema['const']!r}, got {value!r}")
    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{at}: {value!r} is not one of {schema['enum']!r}")
    if isinstance(value, str):
        if len(value) < schema.get("minLength", 0):
            errors.append(f"{at}: string is shorter than {schema['minLength']} characters")
        if pattern := schema.get("pattern"):
            if re.fullmatch(pattern, value) is None:
                errors.append(f"{at}: {value!r} does not match {pattern!r}")
    if isinstance(value, list):
        if len(value) < schema.get("minItems", 0):
            errors.append(f"{at}: needs at least {schema['minItems']} item(s)")
        if schema.get("uniqueItems"):
            encoded = [json.dumps(item, sort_keys=True) for item in value]
            if len(encoded) != len(set(encoded)):
                errors.append(f"{at}: items must be unique")
        if item_schema := schema.get("items"):
            for index, item in enumerate(value):
                errors.extend(validate_schema(item, item_schema, f"{at}[{index}]"))
    if isinstance(value, dict):
        required = schema.get("required", [])
        for key in required:
            if key not in value:
                errors.append(f"{at}: missing required property {key!r}")
        properties = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            for key in value.keys() - properties.keys():
                errors.append(f"{at}: unknown property {key!r}")
        for key in value.keys() & properties.keys():
            errors.extend(validate_schema(value[key], properties[key], f"{at}.{key}"))
    return errors


def active_change_dirs(root: Path) -> set[str]:
    changes = root / "openspec/changes"
    if not changes.is_dir():
        return set()
    return {
        path.name
        for path in changes.iterdir()
        if path.is_dir() and path.name != "archive" and not path.name.startswith(".")
    }


def archive_change_dirs(root: Path) -> set[str]:
    archive = root / "openspec/changes/archive"
    if not archive.is_dir():
        return set()
    return {path.name for path in archive.iterdir() if path.is_dir()}


def task_progress(root: Path, change: str) -> tuple[int, int]:
    task_file = root / "openspec/changes" / change / "tasks.md"
    try:
        states = TASK_RE.findall(task_file.read_text())
    except OSError:
        return (0, 0)
    checked = sum(state.lower() == "x" for state in states)
    return checked, len(states) - checked


def issue_sort_key(identifier: str) -> int:
    match = re.fullmatch(r"THE-([1-9][0-9]*)", identifier)
    return int(match.group(1)) if match else sys.maxsize


def validate_delivery_semantics(root: Path, index: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    projects = set(index.get("projects", []))
    entries = index.get("entries", [])
    names = [entry.get("change", "") for entry in entries if isinstance(entry, dict)]
    if names != sorted(names):
        errors.append("delivery index entries must be sorted by change")
    duplicates = sorted(name for name, count in Counter(names).items() if count > 1)
    if duplicates:
        errors.append(f"delivery index has duplicate changes: {', '.join(duplicates)}")

    actual_active = active_change_dirs(root)
    indexed_active = {
        entry.get("change")
        for entry in entries
        if isinstance(entry, dict) and entry.get("lifecycle") in ACTIVE_LIFECYCLES
    }
    missing = sorted(actual_active - indexed_active)
    stale = sorted(indexed_active - actual_active)
    if missing:
        errors.append(f"active OpenSpec changes missing from delivery index: {', '.join(missing)}")
    if stale:
        errors.append(f"delivery index calls missing changes active: {', '.join(stale)}")

    archived = archive_change_dirs(root)
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        change = entry.get("change", "")
        lifecycle = entry.get("lifecycle")
        owners = entry.get("owners", [])
        rationale = entry.get("untracked_rationale", "").strip()
        if not owners and not rationale:
            errors.append(
                f"{change}: active delivery item has neither Linear/project owner nor reviewed rationale"
            )
        if owners and rationale:
            errors.append(f"{change}: use owners or untracked_rationale, not both")
        for owner in owners:
            if owner.get("project") not in projects:
                errors.append(
                    f"{change}: owner {owner.get('issue', '?')} names unknown project "
                    f"{owner.get('project')!r}"
                )

        archive_path = entry.get("archive_path")
        if lifecycle == "archived":
            if not archive_path:
                errors.append(f"{change}: archived lifecycle needs archive_path")
            elif not (root / archive_path).is_dir():
                errors.append(f"{change}: archive_path does not exist: {archive_path}")
        elif archive_path:
            errors.append(f"{change}: non-archived lifecycle cannot carry archive_path")

        delivered_at = entry.get("delivered_at")
        if lifecycle == "delivered-awaiting-archive":
            if not delivered_at:
                errors.append(
                    f"{change}: delivered-awaiting-archive lifecycle needs delivered_at"
                )
            else:
                try:
                    delivered_date = date.fromisoformat(delivered_at)
                except (TypeError, ValueError):
                    errors.append(f"{change}: delivered_at is not a valid ISO date")
                else:
                    age = (date.today() - delivered_date).days
                    if age < 0:
                        errors.append(f"{change}: delivered_at cannot be in the future")
                    elif age > ARCHIVE_RECONCILIATION_DAYS:
                        errors.append(
                            f"{change}: exceeded the {ARCHIVE_RECONCILIATION_DAYS}-day "
                            "archive reconciliation window"
                        )
        elif delivered_at:
            errors.append(
                f"{change}: only delivered-awaiting-archive may carry delivered_at"
            )

        archived_matches = [name for name in archived if name.endswith(f"-{change}")]
        if lifecycle in ACTIVE_LIFECYCLES and change not in actual_active and archived_matches:
            errors.append(
                f"{change}: archived change is still listed as active ({archived_matches[0]})"
            )

        checked, unchecked = task_progress(root, change)
        if checked and not unchecked and lifecycle in {"proposed", "active"}:
            errors.append(
                f"{change}: all tracked tasks are complete but lifecycle is {lifecycle}; "
                "use delivered-awaiting-archive or reconcile the task ledger"
            )
        if unchecked and lifecycle == "delivered-awaiting-archive":
            errors.append(
                f"{change}: delivered-awaiting-archive still has {unchecked} unchecked task(s)"
            )

        for successor in entry.get("successors", []):
            if not (root / successor).is_dir():
                errors.append(f"{change}: successor path does not exist: {successor}")
    return errors


def validate_issue_semantics(
    index: dict[str, Any], issue_inventory: dict[str, Any]
) -> list[str]:
    """Require the checked-in issue and change views to describe the same graph."""
    errors: list[str] = []
    projects = set(index.get("projects", []))
    all_change_entries = {
        entry.get("change"): entry
        for entry in index.get("entries", [])
        if isinstance(entry, dict)
    }
    change_entries = {
        change: entry
        for change, entry in all_change_entries.items()
        if entry.get("lifecycle") in ACTIVE_LIFECYCLES
    }
    issue_entries = [
        entry for entry in issue_inventory.get("issues", []) if isinstance(entry, dict)
    ]
    identifiers = [entry.get("issue", "") for entry in issue_entries]
    if identifiers != sorted(identifiers, key=issue_sort_key):
        errors.append("active delivery issue inventory must be sorted by numeric issue identifier")
    duplicates = sorted(
        (identifier for identifier, count in Counter(identifiers).items() if count > 1),
        key=issue_sort_key,
    )
    if duplicates:
        errors.append(f"active delivery issue inventory has duplicates: {', '.join(duplicates)}")
    by_issue = {entry.get("issue"): entry for entry in issue_entries}

    for issue in issue_entries:
        identifier = issue.get("issue", "")
        project = issue.get("project")
        changes = issue.get("changes", [])
        rationale = issue.get("no_spec_rationale", "").strip()
        if project not in projects:
            errors.append(f"{identifier}: issue inventory names unknown project {project!r}")
        if changes != sorted(changes):
            errors.append(f"{identifier}: mapped changes must be sorted")
        if not changes and not rationale:
            errors.append(
                f"{identifier}: active delivery issue has neither a mapped change nor "
                "an explicit no-spec rationale"
            )
        if changes and rationale:
            errors.append(
                f"{identifier}: active delivery issue must use mapped changes or a "
                "no-spec rationale, not both"
            )
        for change in changes:
            change_entry = change_entries.get(change)
            if change_entry is None:
                historical_entry = all_change_entries.get(change)
                if historical_entry is not None:
                    errors.append(
                        f"{identifier}: mapped change is not active: {change} "
                        f"({historical_entry.get('lifecycle')})"
                    )
                else:
                    errors.append(
                        f"{identifier}: mapped change is absent from delivery index: {change}"
                    )
                continue
            matching_owners = [
                owner
                for owner in change_entry.get("owners", [])
                if owner.get("issue") == identifier
            ]
            if not matching_owners:
                errors.append(
                    f"{identifier}: issue inventory maps {change}, but the change does not map back"
                )
            elif any(owner.get("project") != project for owner in matching_owners):
                errors.append(
                    f"{identifier}: project differs between issue inventory and {change} owner"
                )

    for change, entry in change_entries.items():
        for owner in entry.get("owners", []):
            identifier = owner.get("issue")
            issue = by_issue.get(identifier)
            if issue is None:
                errors.append(
                    f"{change}: owner {identifier} is missing from active delivery issue inventory"
                )
                continue
            if change not in issue.get("changes", []):
                errors.append(
                    f"{change}: owner {identifier} does not map back to the change in issue inventory"
                )
            if owner.get("project") != issue.get("project"):
                errors.append(
                    f"{change}: owner {identifier} project differs from issue inventory"
                )
    return errors


def rust_plugin_version(source: str) -> str | None:
    match = re.search(
        r"pub const API_VERSION: ApiVersion = ApiVersion\s*\{"
        r".*?major:\s*(\d+),.*?minor:\s*(\d+),.*?patch:\s*(\d+),.*?\};",
        source,
        re.DOTALL,
    )
    return ".".join(match.groups()) if match else None


def _clean_cell(cell: str) -> str:
    return cell.strip().strip("`").strip()


def _list_cell(cell: str) -> list[str]:
    value = _clean_cell(cell)
    if value in {"", "—", "-"}:
        return []
    return sorted(
        part.strip().replace("-", "_")
        for part in value.split(",")
        if part.strip()
    )


def plugin_support_table(markdown: str) -> dict[str, dict[str, Any]]:
    lines = markdown.splitlines()
    try:
        heading = next(
            i
            for i, line in enumerate(lines)
            if re.match(r"^\|\s*Extension point\s*\|", line)
        )
    except StopIteration:
        return {}
    rows: dict[str, dict[str, Any]] = {}
    for line in lines[heading + 2 :]:
        if not line.startswith("|"):
            break
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if len(cells) < 6:
            continue
        point = _clean_cell(cells[0])
        rows[point] = {
            "state": _clean_cell(cells[1]).replace(" ", "_"),
            "required_capability": _clean_cell(cells[2]),
            "modes": _list_cell(cells[3]),
            "cadences": _list_cell(cells[4]),
        }
    return rows


def validate_contract_ratchets(root: Path) -> list[str]:
    errors: list[str] = []
    try:
        source = (root / "crates/thegn-core/src/plugin_api.rs").read_text()
        version = rust_plugin_version(source)
    except OSError as error:
        return [f"plugin API source unavailable: {error}"]
    if version is None:
        return ["plugin API version could not be derived from API_VERSION"]
    major_minor = ".".join(version.split(".")[:2])
    snapshot_path = root / f"docs/api/plugin-api-{major_minor}.json"
    try:
        snapshot = load_json(snapshot_path)
    except ValueError as error:
        return [str(error)]
    if snapshot.get("x-thegn-api-version") != version:
        errors.append(
            f"plugin snapshot version drift: code={version}, "
            f"snapshot={snapshot.get('x-thegn-api-version')!r}"
        )

    developer_doc = root / "docs/extending/plugin.md"
    help_doc = root / "docs/help/plugins.md"
    try:
        developer_text = developer_doc.read_text()
        help_text = help_doc.read_text()
    except OSError as error:
        return errors + [f"plugin documentation unavailable: {error}"]
    markers = {
        developer_doc: (version, f"Current host support (API {major_minor})"),
        help_doc: (f'api = "{version}"', f"plugin-api-{major_minor}.json"),
    }
    for path, expected in markers.items():
        text = developer_text if path == developer_doc else help_text
        for marker in expected:
            if marker not in text:
                errors.append(
                    f"plugin docs version drift: {path.relative_to(root)} needs {marker!r} "
                    f"for API_VERSION {version}"
                )

    documented = plugin_support_table(developer_text)
    generated = {
        row["extension_point"]: {
            "state": row["state"],
            "required_capability": row["required_capability"],
            "modes": sorted(row["modes"]),
            "cadences": sorted(row["cadences"]),
        }
        for row in snapshot.get("x-thegn-extension-support", [])
    }
    if documented.keys() != generated.keys():
        errors.append(
            "plugin extension-point docs drift: "
            f"documented={sorted(documented)}, generated={sorted(generated)}"
        )
    for point in sorted(documented.keys() & generated.keys()):
        if documented[point] != generated[point]:
            errors.append(
                f"plugin extension-point docs drift for {point}: "
                f"documented={documented[point]}, generated={generated[point]}"
            )

    scope_rows = snapshot.get("definitions", {}).get("Scope", {}).get("oneOf", [])
    scopes = {
        value
        for row in scope_rows
        for value in row.get("enum", [])
        if isinstance(value, str)
    }
    if not scopes:
        errors.append("plugin snapshot does not expose the generated control Scope enum")
    for path, text in ((developer_doc, developer_text), (help_doc, help_text)):
        missing_scopes = sorted(scope for scope in scopes if f"`{scope}`" not in text)
        if missing_scopes:
            errors.append(
                f"control-scope docs drift in {path.relative_to(root)}: missing {missing_scopes}"
            )

    try:
        control = load_json(root / "docs/api/control-v1.json")
    except ValueError as error:
        errors.append(str(error))
    else:
        if control.get("version") != "1" or not control.get("routes") or not control.get("types"):
            errors.append("control schema snapshot lacks version, routes, or types")
        for number, route in enumerate(control.get("routes", []), 1):
            if not all(route.get(key) for key in ("cap", "method", "path")):
                errors.append(f"control schema route {number} lacks owning cap/method/path")

    gap_path = root / "test/surface-gaps-ratchet.txt"
    try:
        gap_lines = [
            line.strip()
            for line in gap_path.read_text().splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        ]
    except OSError as error:
        errors.append(f"surface-gap ratchet unavailable: {error}")
    else:
        keys: list[tuple[str, str]] = []
        for line in gap_lines:
            columns = line.split("\t")
            if len(columns) < 3 or not all(column.strip() for column in columns[:3]):
                errors.append(f"surface-gap row lacks capability, surface, or reason: {line!r}")
                continue
            keys.append((columns[0], columns[1]))
        duplicates = sorted(key for key, count in Counter(keys).items() if count > 1)
        if duplicates:
            errors.append(f"duplicate surface-gap capability rows: {duplicates}")
    return errors


def validate_live_count_docs(root: Path) -> list[str]:
    errors: list[str] = []
    for relative in LIVE_COUNT_DOCS:
        path = root / relative
        try:
            text = path.read_text()
        except OSError:
            continue
        for line_number, line in enumerate(text.splitlines(), 1):
            if any(pattern.search(line) for pattern in LIVE_COUNT_PATTERNS):
                errors.append(
                    f"{relative}:{line_number}: hard-coded live count; derive it with "
                    "scripts/delivery_state.py report"
                )
    return errors


def validate_portfolio_links(root: Path) -> list[str]:
    errors: list[str] = []
    path = root / "docs/PORTFOLIO.md"
    try:
        text = path.read_text()
    except OSError:
        return errors
    active = active_change_dirs(root)
    archived = archive_change_dirs(root)
    for match in re.finditer(r"openspec/changes/([a-z0-9-]+)/", text):
        change = match.group(1)
        if change not in active:
            archived_matches = sorted(name for name in archived if name.endswith(f"-{change}"))
            detail = f"; archived as {archived_matches[0]}" if archived_matches else ""
            line = text.count("\n", 0, match.start()) + 1
            errors.append(
                f"docs/PORTFOLIO.md:{line}: stale active change reference {change}{detail}"
            )
    return errors


def validate_repository(
    root: Path,
    index_path: Path = INDEX,
    issue_path: Path = ISSUES,
    *,
    contracts: bool = True,
) -> tuple[tuple[dict[str, Any], dict[str, Any]] | None, list[str]]:
    errors: list[str] = []
    try:
        schema = load_json(root / SCHEMA)
        index = load_json(root / index_path)
        issue_schema = load_json(root / ISSUES_SCHEMA)
        issue_inventory = load_json(root / issue_path)
    except ValueError as error:
        return None, [str(error)]
    errors.extend(validate_schema(index, schema))
    errors.extend(validate_schema(issue_inventory, issue_schema, "$issues"))
    if not errors:
        errors.extend(validate_delivery_semantics(root, index))
        errors.extend(validate_issue_semantics(index, issue_inventory))
    errors.extend(validate_live_count_docs(root))
    errors.extend(validate_portfolio_links(root))
    if contracts:
        errors.extend(validate_contract_ratchets(root))
    return (index, issue_inventory), errors


def print_errors(errors: list[str]) -> None:
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)


def report_data(
    root: Path, index: dict[str, Any], issue_inventory: dict[str, Any]
) -> dict[str, Any]:
    lifecycle = Counter(entry["lifecycle"] for entry in index["entries"])
    disposition = Counter(entry["disposition"] for entry in index["entries"])
    projects: Counter[str] = Counter()
    checked = 0
    unchecked = 0
    for entry in index["entries"]:
        for owner in entry["owners"]:
            projects[owner["project"]] += 1
        done, todo = task_progress(root, entry["change"])
        checked += done
        unchecked += todo
    return {
        "entries": len(index["entries"]),
        "active_delivery_issues": len(issue_inventory["issues"]),
        "no_spec_issues": sum(
            not issue["changes"] for issue in issue_inventory["issues"]
        ),
        "lifecycle": dict(sorted(lifecycle.items())),
        "disposition": dict(sorted(disposition.items())),
        "project_links": dict(sorted(projects.items())),
        "tasks": {"checked": checked, "unchecked": unchecked},
    }


def linear_snapshot_drift(
    index: dict[str, Any], issue_inventory: dict[str, Any], snapshot: Any
) -> list[str]:
    if isinstance(snapshot, dict):
        issues = snapshot.get("issues", [])
    elif isinstance(snapshot, list):
        issues = snapshot
    else:
        return ["Linear snapshot must be an issue array or an object with an issues array"]
    by_id = {
        issue.get("identifier", issue.get("id")): issue
        for issue in issues
        if isinstance(issue, dict)
    }
    drift: list[str] = []
    checked_in = {issue["issue"]: issue for issue in issue_inventory["issues"]}
    terminal = {"done", "completed", "canceled", "cancelled"}
    for identifier, expected in checked_in.items():
        issue = by_id.get(identifier)
        if issue is None:
            drift.append(f"{identifier}: active delivery issue missing from Linear snapshot")
            continue
        project = issue.get("project")
        if isinstance(project, dict):
            project = project.get("name")
        if project != expected["project"]:
            drift.append(
                f"{identifier}: project is {project!r}, inventory expects {expected['project']!r}"
            )
        status = issue.get("status")
        if isinstance(status, dict):
            status = status.get("name")
        if str(status).lower() in terminal:
            drift.append(f"{identifier}: inventory calls issue active while Linear status is {status}")

    delivery_projects = set(index["projects"])
    for identifier, issue in by_id.items():
        project = issue.get("project")
        if isinstance(project, dict):
            project = project.get("name")
        status = issue.get("status")
        if isinstance(status, dict):
            status = status.get("name")
        if (
            isinstance(identifier, str)
            and re.fullmatch(r"THE-[1-9][0-9]*", identifier)
            and project in delivery_projects
            and str(status).lower() not in terminal
            and identifier not in checked_in
        ):
            drift.append(
                f"{identifier}: active Linear delivery issue is absent from checked-in issue inventory"
            )
    return drift


def materialize_fixture(root: Path, fixture: dict[str, Any], schema_source: Path) -> None:
    (root / "delivery").mkdir(parents=True)
    shutil.copyfile(schema_source, root / SCHEMA)
    shutil.copyfile(ROOT / ISSUES_SCHEMA, root / ISSUES_SCHEMA)
    (root / INDEX).write_text(json.dumps(fixture["index"], indent=2) + "\n")
    issue_inventory = fixture.get("issue_inventory")
    if issue_inventory is None:
        aggregated: dict[str, dict[str, Any]] = {}
        for entry in fixture["index"].get("entries", []):
            for owner in entry.get("owners", []):
                issue = aggregated.setdefault(
                    owner["issue"],
                    {"issue": owner["issue"], "project": owner["project"], "changes": []},
                )
                issue["changes"].append(entry["change"])
        issue_inventory = {
            "$schema": "./issues-schema.json",
            "schema_version": 1,
            "scope": "Synthetic active issue inventory for delivery-state validation fixtures.",
            "issues": sorted(aggregated.values(), key=lambda row: issue_sort_key(row["issue"])),
        }
    (root / ISSUES).write_text(json.dumps(issue_inventory, indent=2) + "\n")
    for change, tasks in fixture.get("active_changes", {}).items():
        directory = root / "openspec/changes" / change
        directory.mkdir(parents=True)
        (directory / "tasks.md").write_text(tasks)
    for change in fixture.get("archived_changes", []):
        directory = root / "openspec/changes/archive" / change
        directory.mkdir(parents=True)
        (directory / "tasks.md").write_text("# Tasks\n\n- [x] archived\n")
    portfolio = fixture.get("portfolio")
    if portfolio is not None:
        path = root / "docs/PORTFOLIO.md"
        path.parent.mkdir(parents=True)
        path.write_text(portfolio)
    plugin = fixture.get("plugin_contract")
    if plugin is not None:
        source_path = root / "crates/thegn-core/src/plugin_api.rs"
        source_path.parent.mkdir(parents=True)
        source_path.write_text(plugin["source"])
        snapshot_path = root / plugin["snapshot_path"]
        snapshot_path.parent.mkdir(parents=True)
        snapshot_path.write_text(json.dumps(plugin["snapshot"], indent=2) + "\n")
        developer_path = root / "docs/extending/plugin.md"
        developer_path.parent.mkdir(parents=True)
        developer_path.write_text(plugin["developer_doc"])
        help_path = root / "docs/help/plugins.md"
        help_path.parent.mkdir(parents=True)
        help_path.write_text(plugin["help_doc"])
        control_path = root / "docs/api/control-v1.json"
        control_path.parent.mkdir(parents=True, exist_ok=True)
        control_path.write_text(
            json.dumps(
                {
                    "version": "1",
                    "routes": [{"cap": "fixture.read", "method": "GET", "path": "/v1/fixture"}],
                    "types": {"Fixture": {"type": "object"}},
                }
            )
            + "\n"
        )
        ratchet_path = root / "test/surface-gaps-ratchet.txt"
        ratchet_path.parent.mkdir(parents=True)
        ratchet_path.write_text("fixture.read\thttp\tfixture ownership reason\n")


def run_fixtures(root: Path) -> int:
    fixture_dir = root / "test/delivery-state/fixtures"
    failures: list[str] = []
    for path in sorted(fixture_dir.glob("*.json")):
        fixture = load_json(path)
        with tempfile.TemporaryDirectory(prefix="thegn-delivery-fixture-") as temporary:
            fixture_root = Path(temporary)
            materialize_fixture(fixture_root, fixture, root / SCHEMA)
            state, errors = validate_repository(
                fixture_root, contracts=fixture.get("contracts", False)
            )
        expected = fixture.get("expected_error")
        if expected is None and errors:
            failures.append(f"{path.name}: expected success, got {errors}")
        elif expected is not None and not any(expected in error for error in errors):
            failures.append(f"{path.name}: missing {expected!r}; got {errors}")
        expected_linear = fixture.get("expected_linear_error")
        if expected_linear is not None and state is not None and not errors:
            index, issue_inventory = state
            drift = linear_snapshot_drift(
                index, issue_inventory, fixture.get("linear_snapshot", [])
            )
            if not any(expected_linear in error for error in drift):
                failures.append(
                    f"{path.name}: missing Linear drift {expected_linear!r}; got {drift}"
                )
    if failures:
        print_errors(failures)
        return 1
    print(f"delivery fixtures: {len(list(fixture_dir.glob('*.json')))} passed")
    return 0


def command_validate(args: argparse.Namespace) -> int:
    _, errors = validate_repository(args.root, args.index, args.issues)
    if errors:
        print_errors(errors)
        return 1
    print(
        "delivery state: change/issue schemas, bidirectional ownership, lifecycle, "
        "docs, and contract ratchets valid"
    )
    return 0


def command_report(args: argparse.Namespace) -> int:
    state, errors = validate_repository(args.root, args.index, args.issues)
    if errors or state is None:
        print_errors(errors)
        return 1
    index, issue_inventory = state
    report = report_data(args.root, index, issue_inventory)
    drift: list[str] = []
    if args.linear_json:
        try:
            drift = linear_snapshot_drift(
                index, issue_inventory, load_json(args.linear_json)
            )
        except ValueError as error:
            print_errors([str(error)])
            return 1
        report["linear_snapshot_drift_count"] = len(drift)
        report["linear_snapshot_drift"] = drift[:MAX_LINEAR_DRIFT_ITEMS]
        report["linear_snapshot_drift_truncated"] = len(drift) > MAX_LINEAR_DRIFT_ITEMS
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(f"delivery entries: {report['entries']}")
        print(
            f"active delivery issues: {report['active_delivery_issues']} "
            f"(no-spec={report['no_spec_issues']})"
        )
        print("lifecycle: " + ", ".join(f"{key}={value}" for key, value in report["lifecycle"].items()))
        print("disposition: " + ", ".join(f"{key}={value}" for key, value in report["disposition"].items()))
        print(
            f"tasks: checked={report['tasks']['checked']}, "
            f"unchecked={report['tasks']['unchecked']}"
        )
        if args.linear_json:
            if drift:
                print("Linear snapshot drift:")
                for item in drift[:MAX_LINEAR_DRIFT_ITEMS]:
                    print(f"  - {item}")
                if len(drift) > MAX_LINEAR_DRIFT_ITEMS:
                    print(
                        f"  - ... {len(drift) - MAX_LINEAR_DRIFT_ITEMS} additional "
                        "drift item(s) omitted"
                    )
            else:
                print("Linear snapshot drift: none")
    return 1 if drift else 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--root", type=Path, default=ROOT)
    result.add_argument("--index", type=Path, default=INDEX)
    result.add_argument("--issues", type=Path, default=ISSUES)
    subcommands = result.add_subparsers(dest="command", required=True)
    validate = subcommands.add_parser("validate", help="run the offline repository gate")
    validate.set_defaults(func=command_validate)
    report = subcommands.add_parser("report", help="derive current delivery totals")
    report.add_argument("--json", action="store_true")
    report.add_argument(
        "--linear-json",
        type=Path,
        help="compare a separately exported read-only Linear issue snapshot; never fetches",
    )
    report.set_defaults(func=command_report)
    fixtures = subcommands.add_parser("fixtures", help="run checked-in negative fixtures")
    fixtures.set_defaults(func=lambda args: run_fixtures(args.root))
    return result


def main() -> int:
    args = parser().parse_args()
    args.root = args.root.resolve()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
