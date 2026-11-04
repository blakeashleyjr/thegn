from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "test" / "dup_crates_report.py"
BASELINE = ROOT / "test" / "dup-crates-ratchet.txt"

REG = "registry+https://github.com/rust-lang/crates.io-index"
FIXTURE = f'''version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "bitflags 1.0.0",
 "libx",
 "liby",
]

[[package]]
name = "bitflags"
version = "1.0.0"
source = "{REG}"

[[package]]
name = "bitflags"
version = "2.0.0"
source = "{REG}"

[[package]]
name = "libx"
version = "0.3.0"
source = "{REG}"
dependencies = [
 "bitflags 2.0.0",
]

[[package]]
name = "liby"
version = "0.2.0"
source = "{REG}"
dependencies = [
 "libx",
]
'''

spec = importlib.util.spec_from_file_location("dup_crates_report", SCRIPT)
if spec is None or spec.loader is None:
    raise RuntimeError(f"cannot load {SCRIPT}")
dup_crates_report = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = dup_crates_report
spec.loader.exec_module(dup_crates_report)


class DupCratesReportTests(unittest.TestCase):
    def test_parse_graph_marks_members_and_resolves_bare_and_versioned_refs(self) -> None:
        graph = dup_crates_report.parse_graph(FIXTURE)
        self.assertEqual(graph.members, {("app", "0.1.0")})
        self.assertEqual(
            dup_crates_report.resolve(graph, ("libx", None)), ("libx", "0.3.0")
        )
        self.assertIsNone(dup_crates_report.resolve(graph, ("bitflags", None)))
        self.assertEqual(
            dup_crates_report.resolve(graph, ("bitflags", "2.0.0")),
            ("bitflags", "2.0.0"),
        )

    def test_report_lists_dependents_and_workspace_roots(self) -> None:
        self.assertEqual(
            dup_crates_report.report(FIXTURE)["duplicates"],
            {
                "bitflags": [
                    {
                        "version": "1.0.0",
                        "dependents": ["app 0.1.0"],
                        "workspace_roots": ["app"],
                    },
                    {
                        "version": "2.0.0",
                        "dependents": ["libx 0.3.0"],
                        "workspace_roots": ["app"],
                    },
                ]
            },
        )

    def test_introducers_are_sorted_by_count_then_name(self) -> None:
        baseline = dup_crates_report.report(FIXTURE)["introducers"]
        self.assertEqual(
            baseline,
            [
                {"package": "app 0.1.0", "introduces": ["bitflags 1.0.0"]},
                {"package": "libx 0.3.0", "introduces": ["bitflags 2.0.0"]},
            ],
        )

        expanded = FIXTURE.replace(
            '[[package]]\nname = "libx"',
            f'[[package]]\nname = "bitflags"\nversion = "3.0.0"\nsource = "{REG}"\n\n[[package]]\nname = "libx"',
            1,
        )
        expanded = expanded.replace(
            'dependencies = [\n "libx",\n]\n',
            'dependencies = [\n "libx",\n "bitflags 2.0.0",\n "bitflags 3.0.0",\n]\n',
        )
        self.assertEqual(
            dup_crates_report.report(expanded)["introducers"][0],
            {
                "package": "liby 0.2.0",
                "introduces": ["bitflags 2.0.0", "bitflags 3.0.0"],
            },
        )

    def test_text_output_is_deterministic_and_marks_empty_lists(self) -> None:
        rendered = dup_crates_report.render_text(dup_crates_report.report(FIXTURE))
        self.assertEqual(
            rendered, dup_crates_report.render_text(dup_crates_report.report(FIXTURE))
        )
        self.assertIn("bitflags (2 versions)", rendered)
        self.assertIn("Packages introducing the most duplicates:", rendered)
        empty = dup_crates_report.render_text(
            {
                "duplicates": {
                    "orphan": [
                        {"version": "1.0", "dependents": [], "workspace_roots": []}
                    ]
                },
                "introducers": [],
            }
        )
        self.assertIn("1.0  <- dependents: -  [roots: -]", empty)

    def test_json_cli_round_trips_and_is_sorted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            lock_path = Path(directory) / "Cargo.lock"
            lock_path.write_text(FIXTURE)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                self.assertEqual(
                    dup_crates_report.main(["--json", "--lock", str(lock_path)]), 0
                )
            self.assertEqual(json.loads(stdout.getvalue()), dup_crates_report.report(FIXTURE))
            self.assertLess(
                stdout.getvalue().index('"bitflags"'),
                stdout.getvalue().index('"introducers"'),
            )

            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(
                    dup_crates_report.main(["--lock", str(Path(directory) / "missing.lock")]),
                    1,
                )
            self.assertIn("dup-crates report", stderr.getvalue())

    def test_committed_lock_report_covers_the_ratchet(self) -> None:
        guard_path = ROOT / "test" / "dup_crates_guard.py"
        guard_spec = importlib.util.spec_from_file_location(
            "dup_crates_guard_for_report_test", guard_path
        )
        if guard_spec is None or guard_spec.loader is None:
            raise RuntimeError(f"cannot load {guard_path}")
        guard = importlib.util.module_from_spec(guard_spec)
        sys.modules[guard_spec.name] = guard
        guard_spec.loader.exec_module(guard)

        result = dup_crates_report.report((ROOT / "Cargo.lock").read_text())
        allowlist_names = set(guard.load_allowlist(BASELINE.read_text())[1])
        self.assertEqual(set(result["duplicates"]), allowlist_names)
        for entries in result["duplicates"].values():
            for entry in entries:
                self.assertEqual(
                    set(entry), {"version", "dependents", "workspace_roots"}
                )


if __name__ == "__main__":
    unittest.main()
