"""Offline workflow checks. Shared services and developer indexes stay optional."""

import argparse
import contextlib
import io
import json
from pathlib import Path
import re
import tempfile
import unittest

import agent_metrics
import delivery_state


class AgentWorkflowTests(unittest.TestCase):
    def test_agent_entry_points(self):
        root = delivery_state.ROOT
        for guide in ("AGENTS.md", "docs/agent-workflow.md"):
            path = root / guide
            for link in re.findall(r"\]\(([^)]+)\)", path.read_text()):
                if not link.startswith(("https://", "#")):
                    self.assertTrue((path.parent / link.split("#")[0]).is_file(), link)

    def test_issue_context_handles_historical_and_tracking_work(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "issues").write_text(json.dumps({"issues": [
                {"issue": "THE-2", "project": "test", "no_spec_rationale": "Parent only."}
            ]}))
            (root / "index").write_text(json.dumps({"entries": [
                {"change": "old", "owners": [{"issue": "THE-1"}], "lifecycle": "archived",
                 "summary": "Historical behavior.", "archive_path": "openspec/changes/archive/old/"}
            ]}))
            (root / ".thegn/pipeline/THE-1").mkdir(parents=True)
            for issue, expected in [("the-1", "openspec/changes/archive/old/"), ("THE-2", "Parent only."),
                                    ("THE-3", "Outside the reviewed active inventory")]:
                args = argparse.Namespace(root=root, index="index", issues="issues", issue=issue)
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    self.assertEqual(0, delivery_state.command_context(args))
                self.assertIn(expected, out.getvalue())
                if issue == "the-1":
                    self.assertIn("Handoffs: .thegn/pipeline/THE-1/", out.getvalue())

    def test_counter_is_cumulative_and_counts_session_once(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "session.jsonl"
            events = [{"type": "session_meta", "payload": {"id": "session"}}]
            for total, cached in [(18, 8), (40, 18)]:
                usage = {"input_tokens": total, "cached_input_tokens": cached}
                events.append({"type": "event_msg", "payload": {"type": "token_count",
                               "info": {"total_token_usage": usage}}})
            path.write_text("".join(json.dumps(event) + "\n" for event in events))
            self.assertEqual(22, agent_metrics.codex_inputs([path]))
            with self.assertRaises(ValueError):
                agent_metrics.codex_inputs([path, path])


if __name__ == "__main__":
    unittest.main()
