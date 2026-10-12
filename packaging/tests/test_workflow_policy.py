from __future__ import annotations

import unittest
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[2]


def read_workflow(name: str) -> dict:
    with (ROOT / ".github" / "workflows" / name).open(encoding="utf-8") as handle:
        workflow = yaml.safe_load(handle)
    if not isinstance(workflow, dict):
        raise AssertionError(f"{name} is not a YAML mapping")
    return workflow


class WorkflowPolicyTests(unittest.TestCase):
    def test_run_blocks_do_not_interpolate_untrusted_or_secret_expressions(self) -> None:
        forbidden = ("${{ github.event.inputs", "${{ inputs.", "${{ secrets.", "${{ steps.")
        for name in ("release.yml", "sandbox-image.yml"):
            workflow = read_workflow(name)
            for job_name, job in workflow["jobs"].items():
                for step in job.get("steps", []):
                    script = step.get("run", "")
                    if not isinstance(script, str):
                        continue
                    with self.subTest(workflow=name, job=job_name, step=step.get("name")):
                        for expression in forbidden:
                            self.assertNotIn(expression, script)

    def test_release_source_checkouts_use_admitted_sha(self) -> None:
        workflow = read_workflow("release.yml")
        expected = "${{ needs.admit.outputs.sha }}"
        for job_name in ("upload", "completions", "package"):
            checkouts = [
                step
                for step in workflow["jobs"][job_name]["steps"]
                if step.get("uses", "").startswith("actions/checkout@")
            ]
            self.assertTrue(checkouts, f"{job_name} must check out admitted source")
            for step in checkouts:
                self.assertEqual(expected, step.get("with", {}).get("ref"))

    def test_external_publication_uses_trusted_workflow_checkout(self) -> None:
        workflow = read_workflow("release.yml")
        checkouts = [
            step
            for step in workflow["jobs"]["publish-external"]["steps"]
            if step.get("uses", "").startswith("actions/checkout@")
        ]
        self.assertEqual(1, len(checkouts))
        self.assertNotIn("ref", checkouts[0].get("with", {}))

    def test_package_setup_action_comes_from_admitted_workflow_revision(self) -> None:
        workflow = read_workflow("release.yml")
        actions = [
            step.get("uses")
            for step in workflow["jobs"]["package"]["steps"]
            if step.get("uses", "").endswith(".github/actions/ci-setup")
        ]
        self.assertEqual(["./trusted-workflow/.github/actions/ci-setup"], actions)

    def test_sandbox_publication_is_serialized(self) -> None:
        workflow = read_workflow("sandbox-image.yml")
        self.assertEqual("sandbox-image-publish", workflow.get("concurrency", {}).get("group"))
        self.assertFalse(workflow["concurrency"].get("cancel-in-progress"))


if __name__ == "__main__":
    unittest.main()
