from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[2]
TAG_REGEX = r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?$"


def read_workflow(name: str) -> dict:
    with (ROOT / ".github" / "workflows" / name).open(encoding="utf-8") as handle:
        workflow = yaml.safe_load(handle)
    if not isinstance(workflow, dict):
        raise AssertionError(f"{name} is not a YAML mapping")
    return workflow


class ReleaseAdmissionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = read_workflow("release.yml")
        self.jobs = self.workflow["jobs"]

    @staticmethod
    def _git(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "GIT_CONFIG_GLOBAL": "/dev/null",
                "GIT_CONFIG_NOSYSTEM": "1",
            }
        )
        command = [
            "git",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgSign=false",
            *args,
        ]
        return subprocess.run(
            command,
            cwd=cwd,
            env=environment,
            check=check,
            capture_output=True,
            text=True,
        )

    def test_admit_runs_in_release_admission_environment(self) -> None:
        self.assertEqual("release-admission", self.jobs["admit"].get("environment"))

    def test_releasing_setup_allows_dispatch_branch_and_release_tags(self) -> None:
        releasing = (ROOT / "RELEASING.md").read_text(encoding="utf-8")
        self.assertIn("deployment rule allowing the protected `main`", releasing)
        self.assertIn("branch and `v*` tags", releasing)
        self.assertIn("`v<workspace version>`", releasing)

    def test_tag_object_name_reads_only_the_header(self) -> None:
        script = self._resolve_script()
        command = "sed -n '/^$/q; s/^tag //p'"
        self.assertIn(command, script)
        self.assertNotIn("| head -n 1", script)
        tag_content = (
            "object " + "a" * 40 + "\n"
            "type commit\n"
            "tag v1.2.3\n"
            "tagger T <t@example.invalid> 0 +0000\n"
            "\n"
            "release notes\n"
            "tag attacker-controlled-body-line\n"
        )
        result = subprocess.run(
            ["sed", "-n", "/^$/q; s/^tag //p"],
            input=tag_content,
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertEqual("v1.2.3\n", result.stdout)

    def test_every_write_job_depends_on_admit(self) -> None:
        for job_name, job in self.jobs.items():
            permissions = job.get("permissions", {})
            has_write = any(str(permission).lower() == "write" for permission in permissions.values())
            if "inputs.operation == 'build'" not in job.get("if", "") or not has_write:
                continue
            pending = list(job.get("needs", []))
            if isinstance(job.get("needs"), str):
                pending = [job["needs"]]
            closure: set[str] = set()
            while pending:
                dependency = pending.pop()
                if dependency in closure:
                    continue
                closure.add(dependency)
                needs = self.jobs[dependency].get("needs", [])
                pending.extend([needs] if isinstance(needs, str) else needs)
            with self.subTest(job=job_name):
                self.assertIn("admit", closure)

    def test_every_write_job_reverifies_the_admitted_tag(self) -> None:
        for job_name in ("create-release", "upload", "package", "verify-manifest"):
            steps = self.jobs[job_name]["steps"]
            checks = [
                (index, step)
                for index, step in enumerate(steps)
                if "git ls-remote" in step.get("run", "")
                and "release tag moved after admission" in step.get("run", "")
            ]
            self.assertTrue(checks, f"{job_name} must reverify the admitted tag")
            first_check_index, check_step = checks[0]
            first_privileged_index = next(
                (
                    index
                    for index, step in enumerate(steps)
                    if "gh release create" in step.get("run", "")
                    or "gh release upload" in step.get("run", "")
                    or str(step.get("uses", "")).startswith("actions/attest-")
                ),
                None,
            )
            self.assertIsNotNone(first_privileged_index, f"{job_name} must expose a write/attest step")
            if job_name == "package" and first_check_index == first_privileged_index:
                script = check_step["run"]
                self.assertLess(script.index("git ls-remote"), script.index("gh release upload"))
            else:
                self.assertLess(first_check_index, first_privileged_index, job_name)

    def test_release_checkouts_do_not_persist_credentials(self) -> None:
        for job_name, job in self.jobs.items():
            for step in job.get("steps", []):
                if step.get("uses", "").startswith("actions/checkout@"):
                    with self.subTest(job=job_name, checkout=step.get("name", step.get("uses"))):
                        self.assertIs(step.get("with", {}).get("persist-credentials"), False)

    def test_tag_syntax_matrix(self) -> None:
        admit_script = next(
            step["run"]
            for step in self.jobs["admit"]["steps"]
            if step.get("name") == "Validate requested tag syntax"
        )
        accepted = ("v0.1.0-alpha.31", "v1.2.3", "v10.0.0-rc.1")
        refused = (
            "",
            "-v1.2.3",
            "v1.2.3-",
            "v1.2.3-.",
            "v1.2.3-a..b",
            "v01.2.3",
            "v1.2",
            "refs/tags/v1.2.3",
            "v1.2.3 x",
            "v1.2.3\n",
            "v1.2.3;id",
            "v1.2.3\x1b[31m",
            "v1.2.3-ä",
        )
        for tag in accepted + refused:
            with self.subTest(tag=tag):
                environment = os.environ.copy()
                environment["REQUESTED_TAG"] = tag
                result = subprocess.run(
                    ["bash", "-c", admit_script],
                    env=environment,
                    check=False,
                    capture_output=True,
                    text=True,
                )
                if tag in accepted:
                    self.assertEqual(0, result.returncode, result.stderr)
                else:
                    self.assertNotEqual(0, result.returncode)
                    self.assertIn("invalid release tag", result.stdout)

        regexes = []
        for job_name in ("admit", "aur-srcinfo", "publish-homebrew", "publish-aur"):
            script = next(
                step["run"]
                for step in self.jobs[job_name]["steps"]
                if step.get("name") == ("Validate requested tag syntax" if job_name == "admit" else "Validate release tag syntax")
            )
            normalized_script = script.replace("\\\n", "")
            match = re.search(r"=~\s*(\^v[^\s]+)", normalized_script)
            self.assertIsNotNone(match, job_name)
            regexes.append(match.group(1))
        self.assertEqual([TAG_REGEX] * 4, regexes)

    def _resolve_script(self) -> str:
        return next(
            step["run"]
            for step in self.jobs["admit"]["steps"]
            if step.get("id") == "resolve"
        )

    def _run_resolve_case(self, root: Path, case: str) -> tuple[subprocess.CompletedProcess[str], str]:
        root.mkdir()
        bare = root / "origin.git"
        seed = root / "seed"
        clone = root / "clone"
        bare.mkdir()
        self._git(root, "init", "--bare", str(bare))
        seed.mkdir()
        self._git(seed, "init", "-b", "main")
        (seed / "Cargo.toml").write_text(
            "[workspace.package]\nversion = \"1.2.3\"\n", encoding="utf-8"
        )
        (seed / "tracked.txt").write_text("base\n", encoding="utf-8")
        self._git(seed, "add", "Cargo.toml", "tracked.txt")
        self._git(seed, "commit", "-m", "base")
        self._git(seed, "remote", "add", "origin", str(bare))
        self._git(seed, "push", "origin", "main")
        self._git(seed, "--git-dir", str(bare), "symbolic-ref", "HEAD", "refs/heads/main")
        self._git(root, "clone", str(bare), str(clone))
        tag = "v1.2.3"
        if case == "lightweight":
            self._git(clone, "tag", tag)
        elif case == "version-mismatch":
            self._git(clone, "tag", "-a", "v1.2.4", "-m", "release")
            tag = "v1.2.4"
        elif case == "off-main":
            self._git(clone, "checkout", "-b", "side")
            (clone / "tracked.txt").write_text("side\n", encoding="utf-8")
            self._git(clone, "add", "tracked.txt")
            self._git(clone, "commit", "-m", "side")
            self._git(clone, "tag", "-a", tag, "-m", "release")
        elif case == "tag-name-mismatch":
            self._git(clone, "tag", "-a", "v1.2.3-alias", "-m", "release")
        else:
            self._git(clone, "tag", "-a", tag, "-m", "release")
        if case == "tag-name-mismatch":
            self._git(clone, "push", "origin", "refs/tags/v1.2.3-alias:refs/tags/v1.2.3")
        else:
            self._git(clone, "push", "origin", tag)
        output_path = root / "github-output"
        environment = os.environ.copy()
        environment.update(
            {
                "TAG": tag,
                "GITHUB_OUTPUT": str(output_path),
                "GIT_CONFIG_GLOBAL": "/dev/null",
                "GIT_CONFIG_NOSYSTEM": "1",
            }
        )
        result = subprocess.run(
            ["bash", "-c", self._resolve_script()],
            cwd=clone,
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )
        return result, output_path.read_text(encoding="utf-8") if output_path.exists() else ""

    def test_admit_resolve_rehearsal(self) -> None:
        if shutil.which("bash") is None or shutil.which("git") is None:
            self.skipTest("bash and git are required for the admission rehearsal")
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            # The annotated tag is fetched by the explicit tag refspec; origin/main exists from the main fetch;
            # the annotated-object and version checks pass, then the output lines are written.
            result, output = self._run_resolve_case(root / "annotated", "annotated")
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("tag=v1.2.3", output)
            fields = dict(line.split("=", 1) for line in output.splitlines())
            self.assertNotEqual(fields["sha"], fields["tag_object"])
            self.assertIn("workflow_sha=", output)

            # The cat-file object-type check refuses the lightweight tag before ancestry or output.
            result, _ = self._run_resolve_case(root / "lightweight", "lightweight")
            self.assertNotEqual(0, result.returncode)
            self.assertIn("annotated", result.stdout)

            # The tag-object header check refuses v1.2.3 when the object names v1.2.3-alias.
            result, _ = self._run_resolve_case(root / "tag-name", "tag-name-mismatch")
            self.assertNotEqual(0, result.returncode)
            self.assertIn("names a different tag", result.stdout)

            # The manifest comparison refuses v1.2.4 against Cargo.toml version 1.2.3.
            result, _ = self._run_resolve_case(root / "version", "version-mismatch")
            self.assertNotEqual(0, result.returncode)
            self.assertIn("workspace version", result.stdout)

            # The merge-base check refuses the side-branch commit before version/output checks.
            result, _ = self._run_resolve_case(root / "off-main", "off-main")
            self.assertNotEqual(0, result.returncode)
            self.assertIn("not descended from main", result.stdout)

    def test_create_release_reverify_rehearsal(self) -> None:
        if shutil.which("bash") is None or shutil.which("awk") is None:
            self.skipTest("bash and awk are required for the create-release rehearsal")
        step = self.jobs["create-release"]["steps"][0]
        self.assertEqual("Verify tag still names the admitted commit", step.get("name"))
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            stub_dir = root / "bin"
            stub_dir.mkdir()
            log_path = root / "git.log"
            stub = stub_dir / "git"
            stub.write_text(
                "#!/bin/sh\n"
                'printf \'%s\\n\' "$*" >> "$GIT_STUB_LOG"\n'
                'printf \'%s\' "$GIT_STUB_OUTPUT"\n',
                encoding="utf-8",
            )
            stub.chmod(0o755)
            admitted_object = "a" * 40
            admitted_sha = "b" * 40
            cases = (
                ("matching", f"{admitted_object}\trefs/tags/v1.2.3\n{admitted_sha}\trefs/tags/v1.2.3^{{}}\n", 0, None),
                ("moved", f"{'c' * 40}\trefs/tags/v1.2.3\n{admitted_sha}\trefs/tags/v1.2.3^{{}}\n", 1, "moved after admission"),
                ("disappeared", "", 1, "disappeared"),
            )
            for case, output, expected_status, message in cases:
                with self.subTest(case=case):
                    log_path.write_text("", encoding="utf-8")
                    environment = os.environ.copy()
                    environment.update(
                        {
                            "PATH": f"{stub_dir}{os.pathsep}{environment.get('PATH', '')}",
                            "GIT_STUB_LOG": str(log_path),
                            "GIT_STUB_OUTPUT": output,
                            "TAG": "v1.2.3",
                            "ADMITTED_SHA": admitted_sha,
                            "ADMITTED_TAG_OBJECT": admitted_object,
                            "GH_REPO": "o/r",
                        }
                    )
                    result = subprocess.run(
                        ["bash", "-c", step["run"]],
                        env=environment,
                        check=False,
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(expected_status, result.returncode, result.stderr)
                    if message is not None:
                        self.assertIn(message, result.stdout)
                    self.assertIn(
                        "ls-remote https://github.com/o/r refs/tags/v1.2.3 refs/tags/v1.2.3^{}",
                        log_path.read_text(encoding="utf-8"),
                    )
