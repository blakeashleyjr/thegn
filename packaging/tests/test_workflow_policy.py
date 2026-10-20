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
        steps = read_workflow("release.yml")["jobs"]["package"]["steps"]
        action_index = next(
            index
            for index, step in enumerate(steps)
            if step.get("uses") == "./.github/actions/ci-setup"
        )
        self.assertGreater(action_index, 0)
        preparation = steps[action_index - 1]
        self.assertEqual("Prepare trusted workflow action", preparation.get("name"))
        script = preparation.get("run", "")
        self.assertIn(
            'git archive "$TRUSTED_WORKFLOW_SHA" .github/actions/ci-setup', script
        )
        self.assertIn("rm -rf .github/actions/ci-setup", script)

    def test_sandbox_publication_is_serialized(self) -> None:
        workflow = read_workflow("sandbox-image.yml")
        self.assertEqual("sandbox-image-publish", workflow.get("concurrency", {}).get("group"))
        self.assertFalse(workflow["concurrency"].get("cancel-in-progress"))

    def test_sandbox_image_builds_only_the_admitted_sha(self) -> None:
        jobs = read_workflow("sandbox-image.yml")["jobs"]
        expected = "${{ needs.admit.outputs.sha }}"
        build = jobs["build"]
        build_needs = build["needs"]
        build_needs = build_needs if isinstance(build_needs, list) else [build_needs]
        self.assertIn("admit", build_needs)
        checkouts = [
            step
            for step in build["steps"]
            if step.get("uses", "").startswith("actions/checkout@")
        ]
        self.assertTrue(checkouts, "build must check out the admitted source")
        for step in checkouts:
            self.assertEqual(expected, step.get("with", {}).get("ref"))
        manifest_needs = jobs["manifest"]["needs"]
        manifest_needs = manifest_needs if isinstance(manifest_needs, list) else [manifest_needs]
        self.assertCountEqual(["admit", "build"], manifest_needs)

    def test_sandbox_image_admit_is_read_only(self) -> None:
        admit = read_workflow("sandbox-image.yml")["jobs"]["admit"]
        self.assertEqual({"contents": "read"}, admit["permissions"])
        scripts = "\n".join(step.get("run", "") for step in admit["steps"])
        self.assertIn("merge-base --is-ancestor", scripts)
        self.assertIn("refs/heads/main", scripts)

    def test_sandbox_image_tag_resolved_once(self) -> None:
        jobs = read_workflow("sandbox-image.yml")["jobs"]
        expected = "${{ needs.admit.outputs.tag }}"
        for job_name in ("build", "manifest"):
            steps = jobs[job_name]["steps"]
            self.assertFalse(
                any(step.get("name") == "resolve tag" for step in steps),
                f"{job_name} must use the admitted tag",
            )
            tag_envs = [
                step.get("env", {})["TAG"]
                for step in steps
                if "TAG" in step.get("env", {})
            ]
            self.assertTrue(tag_envs, f"{job_name} must pass TAG to publication steps")
            self.assertTrue(all(tag == expected for tag in tag_envs), tag_envs)


if __name__ == "__main__":
    unittest.main()
