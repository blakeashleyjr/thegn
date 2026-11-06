from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[2]


def read_workflow() -> dict:
    with (ROOT / ".github" / "workflows" / "ci.yml").open(encoding="utf-8") as handle:
        workflow = yaml.safe_load(handle)
    if not isinstance(workflow, dict):
        raise AssertionError("ci.yml is not a YAML mapping")
    return workflow


def variable_runner_expressions() -> list[str]:
    jobs = read_workflow()["jobs"]
    return [
        runner
        for job in jobs.values()
        if isinstance((runner := job.get("runs-on")), str) and "CI_RUNS_ON" in runner
    ]


class CiRunnerPolicyTests(unittest.TestCase):
    def test_security_e2e_job_runs_required_on_github_hosted_runners(self) -> None:
        job = read_workflow()["jobs"]["security-e2e"]
        self.assertEqual("ubuntu-latest", job["runs-on"])
        self.assertEqual({"contents": "read"}, job["permissions"])
        steps = job["steps"]
        self.assertTrue(
            any("just security-e2e" in step.get("run", "") for step in steps),
            "security-e2e must run just security-e2e in required mode",
        )
        self.assertTrue(
            any("bubblewrap" in step.get("run", "") for step in steps),
            "security-e2e must provision bubblewrap",
        )
        self.assertFalse(
            any("|| true" in step.get("run", "") for step in steps),
            "security-e2e provisioning must fail closed",
        )

    def test_security_e2e_job_never_uses_the_self_hosted_toggle(self) -> None:
        runner = read_workflow()["jobs"]["security-e2e"]["runs-on"]
        for forbidden in ("CI_RUNS_ON", "self-hosted", "ageless"):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, runner)

    def test_every_variable_runner_uses_the_trusted_event_gate(self) -> None:
        variable_runners = variable_runner_expressions()
        self.assertEqual(
            15,
            len(variable_runners),
            f"expected 15 Linux jobs using CI_RUNS_ON, found {len(variable_runners)}",
        )
        self.assertEqual(1, len(set(variable_runners)), "all variable runner expressions must match")

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
        runner_expressions = variable_runner_expressions()
        self.assertTrue(runner_expressions, "ci.yml must have a CI_RUNS_ON runner expression")
        self.assertEqual(1, len(set(runner_expressions)), "all variable runner expressions must match")
        runner_expr = runner_expressions[0]
        match = re.fullmatch(
            r"\$\{\{\s*fromJSON\(\((?P<condition>.*?)\)\s*\|\|\s*'(?P<fallback>[^']+)'\)\s*\}\}",
            runner_expr,
        )
        self.assertIsNotNone(match, "ci.yml runner must use the supported gated fromJSON form")
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

        def values_for(event: str, repository: str, configured_runners: str) -> dict[str, bool | str]:
            return {
                "github.event_name == 'workflow_dispatch'": event == "workflow_dispatch",
                "github.repository == 'blakeashleyjr/thegn'": repository == "blakeashleyjr/thegn",
                "vars.CI_RUNS_ON": configured_runners,
            }

        values = values_for("workflow_dispatch", "blakeashleyjr/thegn", "configured")
        self.assertEqual(set(values), set(conjuncts), "condition operands must match the modeled operands")

        def select(event: str, repository: str, configured_runners: str) -> list[str]:
            values = values_for(event, repository, configured_runners)
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

    def test_pull_request_trigger_is_lean_and_unprivileged(self) -> None:
        workflow = read_workflow()
        triggers = workflow.get("on", workflow.get(True, {}))
        self.assertIn("pull_request", triggers)
        self.assertEqual(["main"], triggers["pull_request"]["branches"])
        self.assertIn("schedule", triggers)
        self.assertIn("workflow_dispatch", triggers)
        self.assertNotIn("pull_request_target", triggers)

    def test_workflow_token_is_read_only(self) -> None:
        self.assertEqual({"contents": "read"}, read_workflow()["permissions"])

    def test_pull_request_runs_exactly_the_lean_jobs(self) -> None:
        jobs = read_workflow()["jobs"]
        ran = {
            name
            for name, job in jobs.items()
            if "inputs.extras" not in str(job.get("if", ""))
            and "github.event_name != 'pull_request'" not in str(job.get("if", ""))
        }
        self.assertEqual({"fmt", "lint", "test", "smoke"}, ran)

    def test_heavy_jobs_never_run_on_pull_requests(self) -> None:
        jobs = read_workflow()["jobs"]
        heavy_jobs = {
            "deps-audit",
            "doc",
            "check-features",
            "check-msrv",
            "term-check",
            "check-cross",
            "openspec",
            "coverage",
            "sandbox-e2e",
            "security-e2e",
            "nix-build",
        }
        for name in heavy_jobs:
            with self.subTest(job=name):
                self.assertIn("github.event_name != 'pull_request'", jobs[name]["if"])

    def test_extras_jobs_stay_opt_in(self) -> None:
        jobs = read_workflow()["jobs"]
        for name in ("e2e", "macos", "windows", "windows-artifact-smoke"):
            with self.subTest(job=name):
                self.assertEqual("${{ inputs.extras }}", jobs[name]["if"])

    def test_header_requires_owner_approval_of_the_spend(self) -> None:
        workflow_text = (ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("OWNER MUST APPROVE THE CI SPEND BEFORE THIS MERGES.", workflow_text)
        self.assertNotIn("TEMPORARILY DISABLED", workflow_text)

    def test_pull_request_jobs_use_github_hosted_runners_only(self) -> None:
        for name, job in read_workflow()["jobs"].items():
            runner = job.get("runs-on", "")
            if isinstance(runner, str) and "CI_RUNS_ON" in runner:
                with self.subTest(job=name):
                    self.assertIn("github.event_name == 'workflow_dispatch'", runner)
