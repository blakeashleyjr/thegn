#!/usr/bin/env python3
"""Regression tests for the remote TUI parity probe."""

from __future__ import annotations

import json
import subprocess
import sys
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
        self.assertEqual(len(payload["first_key_ms"]), 1)
        self.assertEqual(len(payload["next_key_ms"]), 5)
        self.assertLess(payload["first_p50"], 50)
        self.assertLess(payload["next_p50"], 50)


if __name__ == "__main__":
    unittest.main()
