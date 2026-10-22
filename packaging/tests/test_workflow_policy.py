from __future__ import annotations

import base64
import hashlib
import json
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
        for job_name in ("build", "upload", "completions", "package", "verify-manifest"):
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
        self.assertEqual(
            "refs/tags/${{ needs.admit.outputs.tag }}",
            action.get("with", {}).get("ref"),
        )

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

    def test_package_verifies_attestations_before_validation(self) -> None:
        job = read_workflow("release.yml")["jobs"]["package"]
        steps = job.get("steps", [])
        names = [step.get("name") for step in steps]
        verify_index = names.index("Verify archive provenance")
        download_index = names.index("Download this tag's source assets")
        validation_index = names.index("Validate all active inputs and render metadata")
        self.assertLess(download_index, verify_index)
        self.assertLess(verify_index, validation_index)
        script = steps[verify_index].get("run", "")
        for expected in (
            "gh attestation verify",
            "--signer-workflow",
            "--source-digest",
            "--deny-self-hosted-runners",
            "--format json",
            "runInvocationURI",
            "$GITHUB_RUN_ID",
        ):
            with self.subTest(expected=expected):
                self.assertIn(expected, script)
        self.assertEqual("read", job.get("permissions", {}).get("attestations"))

    def test_package_provenance_rehearsal(self) -> None:
        if shutil.which("bash") is None:
            self.skipTest("bash is required for the package provenance rehearsal")
        job = read_workflow("release.yml")["jobs"]["package"]
        step = next(
            candidate
            for candidate in job.get("steps", [])
            if candidate.get("name") == "Verify archive provenance"
        )
        script = step["run"]
        admitted_sha = "38ea6674e95abc580c473ed708332cd3d14e97ea"
        signing_sha = "e45b37fb590f7aac8d3580ab22be6cd2b450c43f"
        self.assertNotEqual(admitted_sha, signing_sha)
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary_path = Path(temporary_directory)
            assets = temporary_path / "assets"
            assets.mkdir()
            for name in ("thegn-v1.2.3-a.tar.gz", "thegn-v1.2.3-b.tar.gz"):
                (assets / name).write_bytes(b"")
            (assets / "thegn-v1.2.3-a.tar.gz.sha256").write_text(
                "not an archive\n", encoding="utf-8"
            )
            stub_dir = temporary_path / "bin"
            stub_dir.mkdir()
            log_path = temporary_path / "gh.log"
            stub = stub_dir / "gh"
            stub.write_text(
                "#!/bin/sh\n"
                'printf \'%s\\n\' "$*" >> "$GH_LOG"\n'
                'if [ -n "$GH_FAIL" ]; then\n'
                '  case "$*" in *"/$GH_FAIL "*) exit 1;; esac\n'
                "fi\n"
                'if [ "$GH_FOREIGN_RUN" = 1 ]; then\n'
                '  run_uri="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/other/attempts/1"\n'
                "else\n"
                '  run_uri="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/1"\n'
                "fi\n"
                'printf \'[{"verificationResult":{"signature":{"certificate":{"runInvocationURI":"%s"}}}}]\\n\' "$run_uri"\n',
                encoding="utf-8",
            )
            stub.chmod(0o755)

            def run_case(
                *, fail: str = "", foreign_run: bool = False, empty: bool = False
            ) -> subprocess.CompletedProcess[str]:
                log_path.write_text("", encoding="utf-8")
                if empty:
                    for archive in assets.glob("*.tar.gz"):
                        archive.unlink()
                environment = os.environ.copy()
                environment.update(
                    {
                        "PATH": f"{stub_dir}{os.pathsep}{environment.get('PATH', '')}",
                        "PACKAGE_ROOT": str(temporary_path),
                        "TAG": "v1.2.3",
                        "GITHUB_REPOSITORY": "o/r",
                        "ADMITTED_SHA": admitted_sha,
                        "GITHUB_SHA": signing_sha,
                        "GITHUB_SERVER_URL": "https://github.com",
                        "GITHUB_RUN_ID": "12345",
                        "GITHUB_TOKEN": "x",
                        "GH_LOG": str(log_path),
                        "GH_FAIL": fail,
                        "GH_FOREIGN_RUN": "1" if foreign_run else "0",
                    }
                )
                return subprocess.run(
                    ["bash", "-c", script],
                    env=environment,
                    check=False,
                    capture_output=True,
                    text=True,
                )

            result = run_case(fail="")
            self.assertEqual(0, result.returncode, result.stderr)
            calls = log_path.read_text(encoding="utf-8").splitlines()
            self.assertEqual(2, len(calls))
            for call in calls:
                self.assertTrue(call.startswith("attestation verify "))
                self.assertIn(f"--source-digest {signing_sha}", call)
                self.assertIn("--repo o/r", call)
                self.assertIn("--format json", call)
            self.assertTrue(any("thegn-v1.2.3-a.tar.gz" in call for call in calls))
            self.assertTrue(any("thegn-v1.2.3-b.tar.gz" in call for call in calls))
            self.assertFalse(any(".sha256" in call for call in calls))

            result = run_case(fail="thegn-v1.2.3-b.tar.gz")
            self.assertNotEqual(0, result.returncode)

            result = run_case(foreign_run=True)
            self.assertNotEqual(0, result.returncode)
            self.assertIn("not bound to this workflow run", result.stdout)

            result = run_case(fail="", empty=True)
            self.assertNotEqual(0, result.returncode)
            self.assertEqual("", log_path.read_text(encoding="utf-8"))

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

    def test_publishers_push_with_lease(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        for job_name, step_name in (
            ("publish-homebrew", "Publish Homebrew formula"),
            ("publish-aur", "Publish generated AUR metadata"),
        ):
            steps = jobs[job_name]["steps"]
            step = next(item for item in steps if item.get("name") == step_name)
            script = step.get("run", "")
            summary_name = (
                "Summarize Homebrew publication"
                if job_name == "publish-homebrew"
                else "Summarize AUR publication"
            )
            summary = next(item for item in steps if item.get("name") == summary_name)
            summary_script = summary.get("run", "")
            with self.subTest(job=job_name):
                self.assertEqual("publish", step.get("id"))
                self.assertIn("--force-with-lease=", script)
                self.assertIn("ls-remote", script)
                self.assertIn("GITHUB_OUTPUT", script)
                self.assertNotRegex(script, r"(?m)^\s*git .*push origin HEAD\s*$")
                self.assertIn('[ -z "$BRANCH" ]', summary_script)
                self.assertIn(
                    "before the target was cloned; remote state unknown", summary_script
                )

    def test_publish_cas_rehearsal(self) -> None:
        if shutil.which("git") is None or shutil.which("bash") is None:
            self.skipTest("git and bash are required for the local publication rehearsal")
        real_git = shutil.which("git")
        self.assertIsNotNone(real_git)

        jobs = read_workflow("release.yml")["jobs"]
        publishers = (
            ("publish-homebrew", "Publish Homebrew formula", "main"),
            ("publish-aur", "Publish generated AUR metadata", "master"),
        )

        def git(*args: str, env: dict[str, str] | None = None) -> str:
            git_environment = os.environ.copy()
            git_environment.update(
                {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
            )
            if env is not None:
                git_environment.update(env)
            completed = subprocess.run(
                ["git", "-c", "commit.gpgsign=false", *args],
                check=True,
                capture_output=True,
                text=True,
                env=git_environment,
            )
            return completed.stdout.strip()

        def make_remote(root: Path, branch: str) -> tuple[Path, str]:
            bare = root / "target.git"
            git("init", "--bare", f"--initial-branch={branch}", str(bare))
            seed = root / "seed"
            git("clone", str(bare), str(seed))
            git("-C", str(seed), "config", "user.name", "Fixture")
            git("-C", str(seed), "config", "user.email", "fixture@example.invalid")
            (seed / "README").write_text("initial\n", encoding="utf-8")
            git("-C", str(seed), "add", "README")
            git(
                "-C", str(seed), "-c", "user.name=Fixture",
                "-c", "user.email=fixture@example.invalid", "commit", "-m", "initial",
            )
            git("-C", str(seed), "push", "origin", f"HEAD:refs/heads/{branch}")
            return bare, git("-C", str(seed), "rev-parse", "HEAD")

        def run_publisher(
            parent: Path,
            script: str,
            url: str,
            bare: Path,
            *,
            content: str,
            wrapper_path: Path | None = None,
        ) -> tuple[subprocess.CompletedProcess[str], dict[str, str]]:
            root = parent / f"run-{len(list(parent.glob('run-*')))}"
            publish_root = root / "root"
            (publish_root / "metadata").mkdir(parents=True)
            (publish_root / "keys").mkdir()
            runner_temp = root / "runner"
            (runner_temp / "aur-metadata").mkdir(parents=True)
            (publish_root / "metadata" / "thegn-v1.2.3-homebrew.rb").write_text(
                content, encoding="utf-8"
            )
            (runner_temp / "aur-metadata" / "PKGBUILD").write_text(
                content, encoding="utf-8"
            )
            (runner_temp / "aur-metadata" / ".SRCINFO").write_text(
                f"pkgname = thegn-bin\ncontent = {content}", encoding="utf-8"
            )
            output = root / "github-output"
            output.parent.mkdir(parents=True, exist_ok=True)
            git_config = {
                "GIT_CONFIG_COUNT": "2",
                "GIT_CONFIG_KEY_0": f"url.file://{bare}.insteadOf",
                "GIT_CONFIG_VALUE_0": url,
                "GIT_CONFIG_KEY_1": "commit.gpgsign",
                "GIT_CONFIG_VALUE_1": "false",
                "GIT_CONFIG_GLOBAL": "/dev/null",
                "GIT_CONFIG_NOSYSTEM": "1",
                "PUBLISH_ROOT": str(publish_root),
                "RUNNER_TEMP": str(runner_temp),
                "GITHUB_WORKSPACE": str(ROOT),
                "GITHUB_OUTPUT": str(output),
                "TAG": "v1.2.3",
                "TAP_REPOSITORY": "o/homebrew-tap",
            }
            environment = os.environ.copy()
            environment.update(git_config)
            if wrapper_path is not None:
                environment["PATH"] = f"{wrapper_path.parent}:{environment['PATH']}"
                environment["REAL_GIT"] = real_git or "git"
                environment["BARE_REPO"] = str(bare)
                environment["CONCURRENT_CLONE"] = str(parent / "concurrent")
            completed = subprocess.run(
                ["bash", "-c", script],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )
            values = {}
            if output.exists():
                values = dict(
                    line.split("=", 1)
                    for line in output.read_text(encoding="utf-8").splitlines()
                    if "=" in line
                )
            return completed, values

        for job_name, step_name, branch in publishers:
            url = (
                "git@github.com:o/homebrew-tap.git"
                if job_name == "publish-homebrew"
                else "ssh://aur@aur.archlinux.org/thegn-bin.git"
            )
            step = next(
                item for item in jobs[job_name]["steps"]
                if item.get("name") == step_name
            )
            with self.subTest(job=job_name):
                with tempfile.TemporaryDirectory() as temporary:
                    parent = Path(temporary)
                    bare, initial = make_remote(parent, branch)
                    changed, changed_values = run_publisher(
                        parent, step["run"], url, bare, content="changed\n"
                    )
                    self.assertEqual(0, changed.returncode, changed.stderr)
                    self.assertEqual("committed", changed_values.get("outcome"))
                    self.assertEqual(initial, changed_values.get("old"))
                    self.assertEqual(
                        changed_values.get("new"),
                        git("--git-dir", str(bare), "rev-parse", f"refs/heads/{branch}"),
                    )

                    unchanged, unchanged_values = run_publisher(
                        parent, step["run"], url, bare, content="changed\n"
                    )
                    self.assertEqual(0, unchanged.returncode, unchanged.stderr)
                    self.assertEqual("unchanged", unchanged_values.get("outcome"))
                    self.assertEqual(
                        changed_values.get("new"),
                        git("--git-dir", str(bare), "rev-parse", f"refs/heads/{branch}"),
                    )

                    wrapper_dir = parent / "wrapper-bin"
                    wrapper_dir.mkdir()
                    wrapper = wrapper_dir / "git"
                    wrapper.write_text(
                        "#!/usr/bin/env python3\n"
                        "import os, pathlib, subprocess, sys\n"
                        "real = os.environ['REAL_GIT']\n"
                        "args = sys.argv[1:]\n"
                        "if 'push' in args:\n"
                        "    clone = pathlib.Path(os.environ['CONCURRENT_CLONE'])\n"
                        "    subprocess.run([real, '-c', 'commit.gpgsign=false', 'clone', os.environ['BARE_REPO'], str(clone)], check=True)\n"
                        "    subprocess.run([real, '-c', 'commit.gpgsign=false', '-C', str(clone), 'config', 'user.name', 'Maintainer'], check=True)\n"
                        "    subprocess.run([real, '-c', 'commit.gpgsign=false', '-C', str(clone), 'config', 'user.email', 'maintainer@example.invalid'], check=True)\n"
                        "    (clone / 'maintainer-change').write_text('concurrent\\n')\n"
                        "    subprocess.run([real, '-c', 'commit.gpgsign=false', '-C', str(clone), 'add', 'maintainer-change'], check=True)\n"
                        "    subprocess.run([real, '-c', 'commit.gpgsign=false', '-C', str(clone), '-c', 'user.name=Maintainer', '-c', 'user.email=maintainer@example.invalid', 'commit', '-m', 'concurrent'], check=True)\n"
                        "    subprocess.run([real, '-c', 'commit.gpgsign=false', '-C', str(clone), 'push', 'origin', 'HEAD'], check=True)\n"
                        "os.execv(real, [real, *args])\n",
                        encoding="utf-8",
                    )
                    wrapper.chmod(0o755)
                    concurrent, concurrent_values = run_publisher(
                        parent,
                        step["run"],
                        url,
                        bare,
                        content="candidate after race\n",
                        wrapper_path=wrapper,
                    )
                    remote_tip = git(
                        "--git-dir", str(bare), "rev-parse", f"refs/heads/{branch}"
                    )
                    self.assertNotEqual(0, concurrent.returncode)
                    self.assertEqual(changed_values.get("new"), concurrent_values.get("old"))
                    self.assertEqual(concurrent_values.get("observed"), remote_tip)
                    self.assertNotEqual(concurrent_values.get("old"), remote_tip)
                    self.assertNotEqual(concurrent_values.get("new"), remote_tip)

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

    def test_release_build_enables_the_iroh_feature(self) -> None:
        steps = read_workflow("release.yml")["jobs"]["build"]["steps"]
        action = next(
            step for step in steps
            if step.get("uses", "").startswith("taiki-e/upload-rust-binary-action@")
        )
        self.assertIn("iroh", action["with"]["features"].split(","))

    def test_verify_manifest_job_depends_on_every_producer(self) -> None:
        job = read_workflow("release.yml")["jobs"]["verify-manifest"]
        self.assertEqual(["admit", "upload", "completions", "package"], job["needs"])
        self.assertEqual(
            {"contents": "write", "id-token": "write", "attestations": "write"},
            job["permissions"],
        )
        steps = job["steps"]
        names = [step.get("name", "") for step in steps]
        positions = {
            name: names.index(name)
            for name in (
                "Download every draft asset",
                "Verify archive provenance",
                "Build the release manifest",
                "Verify tag still names the admitted commit",
                "Attest release manifest",
                "Upload release manifest",
            )
        }
        self.assertEqual(sorted(positions.values()), list(positions.values()))
        provenance = steps[positions["Verify archive provenance"]]["run"]
        for expected in ("--signer-workflow", "--source-digest", "--deny-self-hosted-runners", "runInvocationURI"):
            self.assertIn(expected, provenance)
        download = steps[positions["Download every draft asset"]]["run"]
        self.assertIn('gh release download "$TAG" --dir "$root/assets"', download)
        self.assertNotIn("--pattern", download)
        attest = steps[positions["Attest release manifest"]]
        self.assertTrue(attest["uses"].startswith("actions/attest-build-provenance@"))
        self.assertIn("manifest.json", attest["with"]["subject-path"])
        upload = steps[positions["Upload release manifest"]]["run"]
        self.assertIn('gh release upload "$TAG" "$manifest" --clobber', upload)
        self.assertNotIn('release upload "$TAG" "$VERIFY_ROOT/assets/', upload)

    def test_publishers_require_the_release_manifest(self) -> None:
        jobs = read_workflow("release.yml")["jobs"]
        for job_name, step_name in (
            ("publish-homebrew", "Download formula from published release"),
            ("publish-aur", "Download release PKGBUILD and compare with generated metadata"),
        ):
            job = jobs[job_name]
            self.assertEqual("read", job["permissions"]["attestations"])
            step = next(item for item in job["steps"] if item.get("name") == step_name)
            key_step = next(
                index
                for index, item in enumerate(job["steps"])
                if item.get("name") == ("Materialize Homebrew deploy key" if job_name == "publish-homebrew" else "Materialize AUR deploy key")
            )
            self.assertLess(job["steps"].index(step), key_step)
            for expected in (
                'thegn-$TAG-manifest.json',
                "gh attestation verify",
                "packaging/release.py verify-assets",
                '--release-manifest "$manifest"',
            ):
                self.assertIn(expected, step["run"])
            self.assertNotIn("jq -r", step["run"])
        self.assertEqual({"contents": "read"}, jobs["aur-srcinfo"]["permissions"])

    def test_publisher_manifest_check_rehearsal(self) -> None:
        if shutil.which("bash") is None:
            self.skipTest("bash is required for the publisher manifest rehearsal")
        publishers = (
            ("publish-homebrew", "Download formula from published release", "thegn-v1.2.3-homebrew.rb"),
            (
                "publish-aur",
                "Download release PKGBUILD and compare with generated metadata",
                "thegn-v1.2.3-PKGBUILD",
            ),
        )
        jobs = read_workflow("release.yml")["jobs"]
        for job_name, step_name, asset_name in publishers:
            with self.subTest(job=job_name), tempfile.TemporaryDirectory() as temporary_directory:
                job = jobs[job_name]
                step = next(
                    candidate for candidate in job["steps"] if candidate.get("name") == step_name
                )
                script = step["run"]
                root = Path(temporary_directory)
                fixtures = root / "fixtures"
                fixtures.mkdir()
                manifest_name = "thegn-v1.2.3-manifest.json"
                asset_bytes = f"fixture for {asset_name}\n".encode()
                (fixtures / asset_name).write_bytes(asset_bytes)
                manifest = {
                    "schema_version": 1,
                    "tag": "v1.2.3",
                    "commit": "fixture-commit",
                    "tag_object": "fixture-tag-object",
                    "workflow_sha": "fixture-workflow-sha",
                    "run_id": "fixture-run-id",
                    "assets": [
                        {
                            "name": asset_name,
                            "sha256": hashlib.sha256(asset_bytes).hexdigest(),
                            "size": len(asset_bytes),
                        }
                    ],
                }
                manifest_path = fixtures / manifest_name
                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
                stub_dir = root / "bin"
                stub_dir.mkdir()
                (stub_dir / "gh").write_text(
                    "#!/bin/sh\n"
                    'if [ "$1" = release ] && [ "$2" = view ]; then printf false; exit 0; fi\n'
                    'if [ "$1" = release ] && [ "$2" = download ]; then\n'
                    '  shift 2; tag="$1"; shift; destination=""; pattern=""\n'
                    '  while [ "$#" -gt 0 ]; do\n'
                    '    case "$1" in --dir) destination="$2"; shift 2;; --pattern) pattern="$2"; shift 2;; *) shift;; esac\n'
                    '  done\n'
                    '  [ -f "$GH_FIXTURES/$pattern" ] || exit 1\n'
                    '  cp "$GH_FIXTURES/$pattern" "$destination/$pattern"; exit 0\n'
                    'fi\n'
                    'if [ "$1" = attestation ] && [ "$2" = verify ]; then exit "$GH_FAIL_ATTESTATION"; fi\n'
                    'exit 2\n',
                    encoding="utf-8",
                )
                (stub_dir / "gh").chmod(0o755)
                env = os.environ.copy()
                env.update(
                    {
                        "PATH": f"{stub_dir}{os.pathsep}{env.get('PATH', '')}",
                        "RUNNER_TEMP": str(root),
                        "GITHUB_ENV": str(root / "github-env"),
                        "GITHUB_TOKEN": "fixture-token",
                        "GITHUB_REPOSITORY": "owner/repo",
                        "TAG": "v1.2.3",
                        "GH_FIXTURES": str(fixtures),
                        "GH_FAIL_ATTESTATION": "0",
                    }
                )
                if job_name == "publish-aur":
                    aur_metadata = root / "aur-metadata"
                    aur_metadata.mkdir()
                    (aur_metadata / "PKGBUILD").write_bytes(asset_bytes)

                def run_case(*, fail_attestation: bool = False) -> subprocess.CompletedProcess[str]:
                    (root / "github-env").write_text("", encoding="utf-8")
                    env["GH_FAIL_ATTESTATION"] = "1" if fail_attestation else "0"
                    return subprocess.run(
                        ["bash", "-c", script], cwd=ROOT, env=env, check=False, capture_output=True, text=True
                    )

                result = run_case()
                self.assertEqual(0, result.returncode, result.stderr)

                manifest_path.unlink()
                result = run_case()
                self.assertNotEqual(0, result.returncode)
                self.assertIn(
                    "release manifest missing: run the build to completion (verify-manifest) before publishing",
                    result.stdout,
                )

                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
                result = run_case(fail_attestation=True)
                self.assertNotEqual(0, result.returncode)
                self.assertIn("release manifest attestation did not verify", result.stdout)

                wrong_tag = dict(manifest, tag="v9.9.9")
                manifest_path.write_text(json.dumps(wrong_tag), encoding="utf-8")
                result = run_case()
                self.assertNotEqual(0, result.returncode)
                self.assertIn("does not match requested tag", result.stderr)

                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
                (fixtures / asset_name).write_bytes(b"different asset bytes\n")
                result = run_case()
                self.assertNotEqual(0, result.returncode)
                self.assertIn(f"{asset_name} does not match the release manifest", result.stdout)

                (fixtures / asset_name).write_bytes(asset_bytes)
                other_asset = {
                    "name": "thegn-v1.2.3-linux-x86_64.tar.gz",
                    "sha256": hashlib.sha256(b"another release asset").hexdigest(),
                    "size": len(b"another release asset"),
                }
                duplicate_asset_manifest = dict(
                    manifest,
                    assets=[manifest["assets"][0], other_asset, other_asset],
                )
                manifest_path.write_text(json.dumps(duplicate_asset_manifest), encoding="utf-8")
                result = run_case()
                self.assertNotEqual(0, result.returncode)
                self.assertIn("duplicate asset name", result.stderr)

                duplicate_published_asset_manifest = dict(
                    manifest,
                    assets=[manifest["assets"][0], manifest["assets"][0]],
                )
                manifest_path.write_text(json.dumps(duplicate_published_asset_manifest), encoding="utf-8")
                result = run_case()
                self.assertNotEqual(0, result.returncode)
                self.assertIn("duplicate asset name", result.stderr)

                wrong_size_asset = dict(manifest["assets"][0], size=len(asset_bytes) + 1)
                wrong_size_manifest = dict(manifest, assets=[wrong_size_asset])
                manifest_path.write_text(json.dumps(wrong_size_manifest), encoding="utf-8")
                result = run_case()
                self.assertNotEqual(0, result.returncode)
                self.assertIn("size does not match", result.stderr)

    def test_upload_publishes_trusted_asset_digests(self) -> None:
        job = read_workflow("release.yml")["jobs"]["upload"]
        self.assertEqual("${{ steps.record.outputs.asset_digests }}", job["outputs"]["asset_digests"])
        step = next(item for item in job["steps"] if item.get("name") == "Write and publish release record")
        self.assertEqual("record", step["id"])
        self.assertIn('echo "asset_digests=$asset_digests" >> "$GITHUB_OUTPUT"', step["run"])
        self.assertIn('sha256sum "$asset"', step["run"])
        self.assertIn('thegn-$TAG-release.json', step["run"])
        self.assertIn("::error::expected release asset file is missing", step["run"])
        self.assertIn('[ -f "$asset" ] || {', step["run"])

    def test_package_checks_producer_digests_first(self) -> None:
        job = read_workflow("release.yml")["jobs"]["package"]
        names = [item.get("name", "") for item in job["steps"]]
        verify_index = names.index("Verify assets against the upload broker's digests")
        self.assertLess(names.index("Download this tag's source assets"), verify_index)
        self.assertLess(verify_index, names.index("Verify archive provenance"))
        self.assertLess(
            names.index("Verify archive provenance"),
            names.index("Validate all active inputs and render metadata"),
        )
        verify = job["steps"][verify_index]
        self.assertEqual(
            "${{ needs.upload.outputs.asset_digests }}", verify["env"]["TRUSTED_DIGESTS"]
        )
        self.assertIn("release.py verify-digests", verify["run"])
        self.assertEqual(
            "${{ steps.packages.outputs.package_digests }}", job["outputs"]["package_digests"]
        )
        packages = next(
            item
            for item in job["steps"]
            if item.get("name") == "Upload packages and rendered metadata"
        )
        self.assertEqual("packages", packages["id"])
        self.assertIn("::error::expected package asset file is missing", packages["run"])
        self.assertIn('[ -f "$asset" ] || {', packages["run"])
        self.assertLess(
            packages["run"].index('echo "package_digests='),
            packages["run"].index('gh release upload'),
        )

    def test_verify_manifest_binds_producer_digests(self) -> None:
        job = read_workflow("release.yml")["jobs"]["verify-manifest"]
        step = next(
            item for item in job["steps"] if item.get("name") == "Build the release manifest"
        )
        self.assertEqual(
            "${{ needs.upload.outputs.asset_digests }}", step["env"]["UPLOAD_DIGESTS"]
        )
        self.assertEqual(
            "${{ needs.package.outputs.package_digests }}", step["env"]["PACKAGE_DIGESTS"]
        )
        self.assertEqual(2, step["run"].count("--trusted-digests"))
        self.assertIn('"$RUNNER_TEMP/manifest-input/', step["run"])
        self.assertNotIn("$VERIFY_ROOT/assets/manifest-input", step["run"])

    def test_trusted_digest_step_rehearsal(self) -> None:
        import tomllib

        if shutil.which("bash") is None:
            self.skipTest("bash is required for the trusted digest rehearsal")
        job = read_workflow("release.yml")["jobs"]["package"]
        step = next(
            candidate
            for candidate in job["steps"]
            if candidate.get("name") == "Verify assets against the upload broker's digests"
        )
        script = step["run"]
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            package_root = root / "package"
            assets = package_root / "assets"
            assets.mkdir(parents=True)
            with (ROOT / "packaging" / "release.json").open(encoding="utf-8") as handle:
                manifest = json.load(handle)
            with (ROOT / "Cargo.toml").open("rb") as handle:
                version = tomllib.load(handle)["workspace"]["package"]["version"]
            tag = f"v{version}"
            names: list[str] = []
            archive = manifest["archive"]
            for target, config in manifest["targets"].items():
                if config["enabled"]:
                    stem = archive["stem"].format(tag=tag, target=target)
                    extension = archive["formats"][config["platform"]]
                    names.extend((f"{stem}.{extension}", f"{stem}{archive['checksum_suffix']}"))
            completions = f"thegn-{tag}-completions.tar.gz"
            names.extend((completions, f"{completions}.sha256"))
            trusted: dict[str, str] = {}
            for name in names:
                content = f"fixture:{name}".encode()
                (assets / name).write_bytes(content)
                trusted[name] = hashlib.sha256(content).hexdigest()

            def run_case(digests: dict[str, str] | None) -> subprocess.CompletedProcess[str]:
                env = os.environ.copy()
                env.update(
                    {
                        "PACKAGE_ROOT": str(package_root),
                        "TAG": tag,
                        "TRUSTED_DIGESTS": "" if digests is None else json.dumps(digests),
                    }
                )
                return subprocess.run(
                    ["bash", "-c", script],
                    cwd=ROOT,
                    env=env,
                    check=False,
                    capture_output=True,
                    text=True,
                )

            self.assertEqual(0, run_case(trusted).returncode)
            wrong = dict(trusted)
            wrong[names[0]] = "0" * 64
            self.assertNotEqual(0, run_case(wrong).returncode)
            self.assertNotEqual(0, run_case(None).returncode)

    def test_package_stages_archive_without_system_tar(self) -> None:
        package_steps = read_workflow("release.yml")["jobs"]["package"]["steps"]
        by_name = {step.get("name"): step for step in package_steps}
        resolve = by_name["Resolve release tag"]["run"]
        self.assertNotIn("package/stage", resolve)
        digest_index = next(
            index for index, step in enumerate(package_steps)
            if step.get("name") == "Verify assets against the upload broker's digests"
        )
        for step in package_steps[digest_index + 1 :]:
            script = step.get("run", "")
            with self.subTest(step=step.get("name")):
                self.assertNotRegex(script, r"\btar\s+-x")
        build = by_name["Build standalone deb and rpm packages"]["run"]
        stage_at = build.index("packaging/release.py stage")
        nfpm_at = build.index("nixpkgs#nfpm")
        self.assertLess(stage_at, nfpm_at)
        self.assertIn('--trusted-digests "$PACKAGE_ROOT/trusted-digests.json"', build)
        self.assertIn('--output-dir "$PACKAGE_ROOT/package/stage"', build)

    def test_package_verifies_payload_before_upload(self) -> None:
        steps = read_workflow("release.yml")["jobs"]["package"]["steps"]
        names = [step.get("name") for step in steps]
        build_at = names.index("Build standalone deb and rpm packages")
        verify_at = names.index("Verify packaged payload against staged bytes")
        metadata_at = names.index("Stage rendered metadata")
        upload_at = names.index("Upload packages and rendered metadata")
        self.assertLess(build_at, verify_at)
        self.assertLess(verify_at, metadata_at)
        self.assertLess(metadata_at, upload_at)
        verify = steps[verify_at]["run"]
        self.assertEqual(2, verify.count("packaging/release.py verify-payload"))
        self.assertIn("--label deb", verify)
        self.assertIn("--label rpm", verify)
        self.assertIn('"$PACKAGE_ROOT/payload/deb"', verify)
        self.assertIn('"$PACKAGE_ROOT/payload/rpm"', verify)
        self.assertIn('cd "$PACKAGE_ROOT/payload/rpm"', verify)
        self.assertNotIn('cd "$PACKAGE_ROOT/release-assets"', verify)
        for command in (
            "nixpkgs#dpkg",
            "nixpkgs#rpm",
            "nixpkgs#cpio",
            '"$dpkg_store/bin/dpkg-deb" -x',
            '"$rpm_store/bin/rpm2cpio"',
            '"$cpio_store/bin/cpio" -idm --no-absolute-filenames --quiet',
        ):
            with self.subTest(command=command):
                self.assertIn(command, verify)


if __name__ == "__main__":
    unittest.main()
