from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[2]
RUNNER_EXPR = (
    "${{ fromJSON((github.event_name == 'workflow_dispatch' && "
    "github.repository == 'blakeashleyjr/thegn' && vars.CI_RUNS_ON) || "
    "'[\"ubuntu-latest\"]') }}"
)


def read_workflow() -> dict:
    with (ROOT / ".github" / "workflows" / "ci.yml").open(encoding="utf-8") as handle:
        workflow = yaml.safe_load(handle)
    if not isinstance(workflow, dict):
        raise AssertionError("ci.yml is not a YAML mapping")
    return workflow


class CiRunnerPolicyTests(unittest.TestCase):
    def test_every_variable_runner_uses_the_trusted_event_gate(self) -> None:
        jobs = read_workflow()["jobs"]
        variable_runners = {
            job_name: job.get("runs-on")
            for job_name, job in jobs.items()
            if isinstance(job.get("runs-on"), str) and "CI_RUNS_ON" in job["runs-on"]
        }
        self.assertEqual(
            15,
            len(variable_runners),
            f"expected 15 Linux jobs using CI_RUNS_ON, found {sorted(variable_runners)}",
        )
        for job_name, runner in variable_runners.items():
            with self.subTest(job=job_name):
                self.assertEqual(RUNNER_EXPR, runner)

    def test_no_job_names_self_hosted_directly(self) -> None:
        # Positive control: this policy already passes on the current workflow.
        for job_name, job in read_workflow()["jobs"].items():
            runner = job.get("runs-on", "")
            values = runner if isinstance(runner, list) else [runner]
            with self.subTest(job=job_name):
                for value in values:
                    label = str(value).lower()
                    self.assertNotIn("self-hosted", label)
                    self.assertNotIn("ageless", label)

    def test_untrusted_triggers_are_absent(self) -> None:
        # Positive control: this passes today and guards future trigger restoration.
        workflow = read_workflow()
        triggers = workflow.get("on", workflow.get(True, {}))
        self.assertIsInstance(triggers, dict)
        for trigger in ("pull_request_target", "workflow_run", "workflow_call"):
            with self.subTest(trigger=trigger):
                self.assertNotIn(trigger, triggers)

    def test_runner_selection_matrix(self) -> None:
        match = re.fullmatch(
            r"\$\{\{\s*fromJSON\(\((?P<condition>.*?)\)\s*\|\|\s*'(?P<fallback>[^']+)'\)\s*\}\}",
            RUNNER_EXPR,
        )
        self.assertIsNotNone(match, "RUNNER_EXPR must use the supported gated fromJSON form")
        assert match is not None
        conjuncts = [part.strip() for part in match.group("condition").split("&&")]
        for required in (
            "github.event_name == 'workflow_dispatch'",
            "github.repository == 'blakeashleyjr/thegn'",
            "vars.CI_RUNS_ON",
        ):
            with self.subTest(conjunct=required):
                self.assertIn(required, conjuncts)
        fallback = json.loads(match.group("fallback"))

        def select(event: str, repository: str, configured_runners: str) -> list[str]:
            values = {
                "github.event_name == 'workflow_dispatch'": event == "workflow_dispatch",
                "github.repository == 'blakeashleyjr/thegn'": repository == "blakeashleyjr/thegn",
                "vars.CI_RUNS_ON": configured_runners,
            }
            operands = [values[conjunct] for conjunct in conjuncts]
            selected = operands[0]
            for operand in operands[1:]:
                if not selected:
                    break
                selected = operand
            return json.loads(selected) if selected else fallback

        cases = (
            ("push", "blakeashleyjr/thegn", '["self-hosted","ageless"]', ["ubuntu-latest"]),
            ("pull_request", "blakeashleyjr/thegn", '["self-hosted","ageless"]', ["ubuntu-latest"]),
            ("pull_request_target", "blakeashleyjr/thegn", '["self-hosted","ageless"]', ["ubuntu-latest"]),
            ("schedule", "blakeashleyjr/thegn", '["self-hosted","ageless"]', ["ubuntu-latest"]),
            ("workflow_dispatch", "someone/thegn", '["self-hosted","ageless"]', ["ubuntu-latest"]),
            ("workflow_dispatch", "blakeashleyjr/thegn", "", ["ubuntu-latest"]),
            (
                "workflow_dispatch",
                "blakeashleyjr/thegn",
                '["self-hosted","ageless"]',
                ["self-hosted", "ageless"],
            ),
        )
        for event, repository, configured_runners, expected in cases:
            with self.subTest(event=event, repository=repository, runners=configured_runners):
                self.assertEqual(expected, select(event, repository, configured_runners))
