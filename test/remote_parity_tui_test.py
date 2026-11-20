#!/usr/bin/env python3
"""Regression tests for the remote TUI parity probe."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("remote_parity_tui.py")


class RemoteParityTuiTests(unittest.TestCase):
    def test_self_test_uses_shell_and_reports_latency_json(self) -> None:
        completed = subprocess.run(
            [
                sys.executable,
                "-I",
                str(SCRIPT),
                "--bin",
                "/bin/false",
                "--repo",
                str(SCRIPT.parent.parent),
                "--samples",
                "1",
                "--timeout",
                "10",
                "--self-test",
            ],
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(len(completed.stdout.splitlines()), 1)
        payload = json.loads(completed.stdout)
        self.assertEqual(payload["status"], "PASS")
        self.assertEqual(set(payload), {"status", "note", "first", "next", "samples_file"})
        self.assertEqual(payload["first"]["n"], 1)
        self.assertEqual(payload["next"]["n"], 5)
        self.assertLess(payload["first"]["p50"], 50)
        self.assertLess(payload["next"]["p50"], 50)

    def test_samples_file_keeps_raw_timings_out_of_stdout(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples_path = Path(directory) / "samples.json"
            environment = os.environ.copy()
            environment["RP_TUI_SAMPLES_OUT"] = str(samples_path)
            completed = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    str(SCRIPT),
                    "--bin",
                    "/bin/false",
                    "--repo",
                    str(SCRIPT.parent.parent),
                    "--samples",
                    "1",
                    "--timeout",
                    "10",
                    "--self-test",
                ],
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
                env=environment,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            payload = json.loads(completed.stdout)
            self.assertEqual(payload["samples_file"], str(samples_path))
            samples = json.loads(samples_path.read_text(encoding="utf-8"))
            self.assertEqual(set(samples), {"first_key_ms", "next_key_ms"})
            self.assertEqual(len(samples["first_key_ms"]), 1)
            self.assertEqual(len(samples["next_key_ms"]), 5)


if __name__ == "__main__":
    unittest.main()
