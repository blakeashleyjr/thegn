from __future__ import annotations

import base64
import hashlib
import os
import re
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
        for job_name in ("build", "upload", "completions", "package"):
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
        for job_name in ("publish-homebrew", "publish-aur", "aur-srcinfo"):
            checkouts = [
                step
                for step in workflow["jobs"][job_name]["steps"]
                if step.get("uses", "").startswith("actions/checkout@")
            ]
            with self.subTest(job=job_name):
                self.assertEqual(1, len(checkouts))
                self.assertNotIn("ref", checkouts[0].get("with", {}))

    def test_no_workflow_trusts_first_seen_host_keys(self) -> None:
        for path in sorted((ROOT / ".github" / "workflows").rglob("*.yml")):
            content = path.read_text(encoding="utf-8")
            with self.subTest(workflow=path.name):
                self.assertNotIn("accept-new", content)
                self.assertNotIn("StrictHostKeyChecking=no", content)

    def test_external_publication_pins_host_keys(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        for job_name in ("publish-homebrew", "publish-aur"):
            steps = jobs[job_name]["steps"]
            ssh_scripts = [
                step.get("run", "")
                for step in steps
                if "ssh -i" in step.get("run", "")
            ]
            with self.subTest(job=job_name):
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

    def test_build_jobs_are_read_only(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        for job_name in ("build", "completions"):
            with self.subTest(job=job_name):
                self.assertEqual({"contents": "read"}, jobs[job_name].get("permissions"))
        build = jobs["build"]
        action = next(
            step for step in build["steps"]
            if step.get("uses", "").startswith("taiki-e/upload-rust-binary-action@")
        )
        self.assertIs(action.get("with", {}).get("dry-run"), True)
        self.assertIs(action.get("with", {}).get("dry-run-intended"), True)
        self.assertNotIn("GITHUB_TOKEN", action.get("env", {}))

    def test_no_privileged_job_builds_release_code(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        for job_name, job in jobs.items():
            permissions = job.get("permissions", {})
            privileged = any(
                permissions.get(scope) == "write"
                for scope in ("contents", "id-token", "attestations")
            )
            if privileged:
                with self.subTest(job=job_name):
                    job_text = yaml.safe_dump(job)
                    for forbidden in (
                        "cargo build",
                        "upload-rust-binary-action",
                        "rust-toolchain",
                    ):
                        self.assertNotIn(forbidden, job_text)

    def test_upload_broker_publishes_only_verified_artifacts(self) -> None:
        job = read_workflow("release.yml")["jobs"]["upload"]
        self.assertIn("build", job.get("needs", []))
        self.assertIn("completions", job.get("needs", []))
        steps = job.get("steps", [])
        self.assertTrue(
            any(step.get("uses", "").startswith("actions/download-artifact@") for step in steps)
        )
        self.assertTrue(
            any(step.get("uses", "").startswith("actions/attest-build-provenance@") for step in steps)
        )
        job_text = yaml.safe_dump(job)
        self.assertIn("gh release upload", job_text)
        self.assertIn("sha256sum", job_text)

    def test_release_record_asset(self) -> None:
        job = read_workflow("release.yml")["jobs"]["upload"]
        scripts = "\n".join(step.get("run", "") for step in job.get("steps", []))
        for expected in ("release.json", "tag_object", "workflow_sha", "jq"):
            with self.subTest(expected=expected):
                self.assertIn(expected, scripts)

    def test_no_job_holds_both_deploy_keys(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        serialized = {name: yaml.safe_dump(job) for name, job in jobs.items()}
        for name, job_text in serialized.items():
            with self.subTest(job=name):
                self.assertFalse(
                    "HOMEBREW_TAP_DEPLOY_KEY" in job_text
                    and "AUR_DEPLOY_KEY" in job_text
                )
        for key_name in ("HOMEBREW_TAP_DEPLOY_KEY", "AUR_DEPLOY_KEY"):
            containing_jobs = [
                name for name, job_text in serialized.items() if key_name in job_text
            ]
            expected = "publish-homebrew" if key_name.startswith("HOMEBREW") else "publish-aur"
            self.assertEqual([expected], containing_jobs)

    def test_makepkg_runs_only_in_credential_free_job(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        makepkg_jobs = {
            name
            for name, job in jobs.items()
            if "makepkg" in yaml.safe_dump(job)
        }
        self.assertEqual({"aur-srcinfo"}, makepkg_jobs)
        job = jobs["aur-srcinfo"]
        self.assertNotIn("environment", job)
        self.assertEqual({"contents": "read"}, job.get("permissions"))
        self.assertNotIn("secrets.", yaml.safe_dump(job))

    def test_publish_jobs_use_distinct_environments(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        homebrew_environment = jobs["publish-homebrew"].get("environment")
        aur_environment = jobs["publish-aur"].get("environment")
        self.assertTrue(homebrew_environment)
        self.assertTrue(aur_environment)
        self.assertNotEqual(homebrew_environment, aur_environment)

    def test_publish_aur_consumes_srcinfo_artifact(self) -> None:
        job = read_workflow("release.yml")["jobs"]["publish-aur"]
        self.assertIn("aur-srcinfo", job.get("needs", []))
        steps = job.get("steps", [])
        downloads = [
            step for step in steps
            if step.get("uses", "").startswith("actions/download-artifact@")
        ]
        self.assertEqual(1, len(downloads))
        self.assertEqual("aur-metadata", downloads[0].get("with", {}).get("name"))
        srcinfo_job = read_workflow("release.yml")["jobs"]["aur-srcinfo"]
        uploads = [
            step for step in srcinfo_job.get("steps", [])
            if step.get("uses", "").startswith("actions/upload-artifact@")
        ]
        self.assertEqual(1, len(uploads))
        self.assertEqual("aur-metadata", uploads[0].get("with", {}).get("name"))
        self.assertIs(uploads[0].get("with", {}).get("include-hidden-files"), True)
        job_text = yaml.safe_dump(job)
        self.assertNotIn("makepkg", job_text)
        self.assertNotRegex(job_text, r"(?m)\bsource\s+[^\n]*PKGBUILD")
        validation = next(
            step for step in steps
            if step.get("name") == "Require exactly the generated AUR metadata files"
        )
        validation_script = validation.get("run", "")
        self.assertIn("LC_ALL=C sort", validation_script)
        self.assertIn('[ -L "$RUNNER_TEMP/aur-metadata/.SRCINFO" ]', validation_script)
        self.assertIn('[ -L "$RUNNER_TEMP/aur-metadata/PKGBUILD" ]', validation_script)

    def test_publish_roots_live_under_runner_temp(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        for name in ("publish-homebrew", "publish-aur", "aur-srcinfo"):
            job = jobs[name]
            for step in job.get("steps", []):
                script = step.get("run", "")
                if "mktemp -d" in script:
                    with self.subTest(job=name, step=step.get("name")):
                        for match in re.findall(
                            r'mktemp -d(?:\s+("?[^\s)]+))?', script
                        ):
                            self.assertTrue(
                                match.lstrip('"').startswith("$RUNNER_TEMP/"),
                                f"mktemp directory must be under RUNNER_TEMP: {match!r}",
                            )

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
        script = next(
            step["run"]
            for step in manifest["steps"]
            if step.get("name") == "assemble + push manifest list from frozen digests"
        )
        following_lines = script.replace("\\\n", " ").splitlines()
        create_line = next(
            line for line in following_lines if "podman manifest create" in line
        )
        create_command = create_line
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
        self.assertIn("len(set(expected.values())) != 2", verify_script)
        self.assertIn('item.get("platform", {}).get("architecture")', verify_script)
        self.assertIn('expected.get(item.get("platform", {}).get("architecture"))', verify_script)
        push_script = steps[
            next(
                index
                for index, step in enumerate(steps)
                if step.get("name") == "assemble + push manifest list from frozen digests"
            )
        ]["run"]
        self.assertIn('--authfile "$HOME/.docker/config.json"', push_script)
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
