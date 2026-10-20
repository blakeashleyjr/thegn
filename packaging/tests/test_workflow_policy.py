from __future__ import annotations

import base64
import hashlib
import os
import shutil
import subprocess
import tempfile
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

    def test_no_workflow_trusts_first_seen_host_keys(self) -> None:
        for path in sorted((ROOT / ".github" / "workflows").rglob("*.yml")):
            content = path.read_text(encoding="utf-8")
            with self.subTest(workflow=path.name):
                self.assertNotIn("accept-new", content)
                self.assertNotIn("StrictHostKeyChecking=no", content)

    def test_external_publication_pins_host_keys(self) -> None:
        steps = read_workflow("release.yml")["jobs"]["publish-external"]["steps"]
        ssh_scripts = [
            step.get("run", "")
            for step in steps
            if "ssh -i" in step.get("run", "")
        ]
        self.assertTrue(ssh_scripts, "publication must contain SSH commands")
        for script in ssh_scripts:
            for required in (
                "StrictHostKeyChecking=yes",
                "UserKnownHostsFile=",
                ".github/known_hosts",
                "GlobalKnownHostsFile=/dev/null",
            ):
                with self.subTest(required=required):
                    self.assertIn(required, script)

    def test_committed_known_hosts_match_published_fingerprints(self) -> None:
        known_hosts = ROOT / ".github" / "known_hosts"
        entries = []
        for line in known_hosts.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            fields = line.split()
            self.assertEqual(3, len(fields), f"invalid known_hosts entry: {line}")
            host, key_type, blob = fields
            self.assertEqual("ssh-ed25519", key_type)
            self.assertIn(host, {"github.com", "aur.archlinux.org"})
            entries.append((host, key_type, blob))
        github_keys = [
            entry
            for entry in entries
            if entry[:2] == ("github.com", "ssh-ed25519")
        ]
        self.assertTrue(github_keys, "github.com ed25519 host key must be committed")
        for _, _, blob in github_keys:
            fingerprint = base64.b64encode(hashlib.sha256(base64.b64decode(blob)).digest())
            self.assertEqual(
                "+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU",
                fingerprint.decode("ascii").rstrip("="),
            )

    def test_create_release_never_deletes(self) -> None:
        workflow_path = ROOT / ".github" / "workflows" / "release.yml"
        content = workflow_path.read_text(encoding="utf-8")
        self.assertNotIn("gh release delete", content)
        self.assertNotIn("create-gh-release-action", content)
        steps = read_workflow("release.yml")["jobs"]["create-release"]["steps"]
        self.assertTrue(steps, "create-release must have a rehearsal step")
        self.assertTrue(
            all("uses" not in step for step in steps),
            "create-release must not invoke an action that manages releases",
        )

    def test_create_release_rehearsal(self) -> None:
        if shutil.which("bash") is None or shutil.which("awk") is None:
            self.skipTest("bash and awk are required for the release rehearsal")
        steps = read_workflow("release.yml")["jobs"]["create-release"]["steps"]
        step = next(
            (
                candidate
                for candidate in steps
                if candidate.get("name") == "Create the draft release only if absent"
            ),
            None,
        )
        self.assertIsNotNone(
            step, "create-release must expose a shell script for rehearsal"
        )
        script = step["run"]
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary_path = Path(temporary_directory)
            stub_dir = temporary_path / "bin"
            stub_dir.mkdir()
            log_path = temporary_path / "gh.log"
            stub = stub_dir / "gh"
            stub.write_text(
                "#!/bin/sh\n"
                'printf \'%s\\n\' "$*" >> "$GH_LOG"\n'
                'if [ "$1" = api ]; then\n'
                '  [ "$GH_CASE" != api_error ] || exit 1\n'
                '  printf \'%s\\n\' "$GH_ROWS"\n'
                'fi\n',
                encoding="utf-8",
            )
            stub.chmod(0o755)
            tag = "v1.2.3-alpha.1"
            cases = (
                ("absent", "v0.9.0\ttrue\n", 0, True, True),
                ("draft", f"{tag}\ttrue\n", 0, False, False),
                ("published", f"{tag}\tfalse\n", 1, False, False),
                ("duplicate", f"{tag}\ttrue\n{tag}\tfalse\n", 1, False, False),
                ("api_error", "", 1, False, False),
            )
            for case, rows, expected_status, expect_release, expect_prerelease in cases:
                with self.subTest(case=case):
                    log_path.write_text("", encoding="utf-8")
                    environment = os.environ.copy()
                    environment.update(
                        {
                            "PATH": f"{stub_dir}{os.pathsep}{environment.get('PATH', '')}",
                            "GH_CASE": case,
                            "GH_LOG": str(log_path),
                            "GH_ROWS": rows,
                            "GH_REPO": "o/r",
                            "GH_TOKEN": "x",
                            "TAG": tag,
                        }
                    )
                    result = subprocess.run(
                        ["bash", "-c", script],
                        env=environment,
                        check=False,
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(expected_status, result.returncode, result.stderr)
                    calls = log_path.read_text(encoding="utf-8").splitlines()
                    release_calls = [call for call in calls if call.startswith("release ")]
                    self.assertEqual(expect_release, bool(release_calls))
                    if expect_release:
                        self.assertEqual(1, len(release_calls))
                        self.assertTrue(
                            release_calls[0].startswith(f"release create {tag} ")
                        )
                        self.assertIn("--draft", release_calls[0])
                        self.assertIn("--verify-tag", release_calls[0])
                        self.assertEqual(
                            expect_prerelease, "--prerelease" in release_calls[0]
                        )

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

    def test_sandbox_image_manifest_uses_digests_only(self) -> None:
        manifest = read_workflow("sandbox-image.yml")["jobs"]["manifest"]
        scripts = "\n".join(step.get("run", "") for step in manifest["steps"])
        create_line = next(
            line for line in scripts.splitlines() if "podman manifest create" in line
        )
        following_lines = scripts.splitlines()
        create_index = following_lines.index(create_line)
        create_command = "\n".join(following_lines[create_index : create_index + 3])
        self.assertIn("@$amd64_digest", create_command)
        self.assertIn("@$arm64_digest", create_command)
        self.assertNotIn("$TAG-amd64", create_command)
        self.assertNotIn("$TAG-arm64", create_command)

    def test_sandbox_image_pushes_are_generation_qualified(self) -> None:
        build = read_workflow("sandbox-image.yml")["jobs"]["build"]
        scripts = "\n".join(step.get("run", "") for step in build["steps"])
        self.assertIn("$GITHUB_RUN_ID", scripts)
        self.assertIn("$GITHUB_RUN_ATTEMPT", scripts)
        self.assertIn('podman push --digestfile "$digest_file"', scripts)
        artifact = next(
            step
            for step in build["steps"]
            if step.get("uses", "").startswith("actions/upload-artifact@")
        )
        self.assertEqual("digest-${{ matrix.oci_arch }}", artifact["with"]["name"])
        self.assertEqual("arch-digest.txt", artifact["with"]["path"])
        self.assertEqual("error", artifact["with"]["if-no-files-found"])

    def test_sandbox_image_packages_write_is_per_job(self) -> None:
        workflow = read_workflow("sandbox-image.yml")
        jobs = workflow["jobs"]
        self.assertEqual({"contents": "read"}, workflow["permissions"])
        self.assertNotIn("packages", jobs["admit"]["permissions"])
        for job_name in ("build", "manifest"):
            with self.subTest(job=job_name):
                self.assertEqual("write", jobs[job_name]["permissions"]["packages"])
        self.assertEqual("write", jobs["manifest"]["permissions"]["id-token"])
        self.assertEqual("write", jobs["manifest"]["permissions"]["attestations"])

    def test_sandbox_image_list_is_verified_and_attested(self) -> None:
        manifest = read_workflow("sandbox-image.yml")["jobs"]["manifest"]
        steps = manifest["steps"]
        verify_index = next(
            index
            for index, step in enumerate(steps)
            if step.get("name") == "verify pushed manifest by digest"
        )
        verify_script = steps[verify_index]["run"]
        self.assertIn(
            'skopeo inspect --raw "docker://$IMAGE@$LIST_DIGEST"', verify_script
        )
        self.assertIn('set(actual) != set(expected)', verify_script)
        attest_index = next(
            index
            for index, step in enumerate(steps)
            if step.get("uses", "").startswith(
                "actions/attest-build-provenance@"
            )
        )
        attestation = steps[attest_index]
        self.assertGreater(attest_index, verify_index)
        self.assertEqual("${{ env.IMAGE }}", attestation["with"]["subject-name"])
        self.assertEqual(
            "${{ steps.push.outputs.digest }}",
            attestation["with"]["subject-digest"],
        )
        self.assertIs(attestation["with"]["push-to-registry"], True)
        print_index = next(
            index
            for index, step in enumerate(steps)
            if step.get("name") == "print the list digest (pin as DEFAULT_BASE_DIGEST)"
        )
        self.assertGreater(print_index, attest_index)
        self.assertGreater(print_index, verify_index)


if __name__ == "__main__":
    unittest.main()
