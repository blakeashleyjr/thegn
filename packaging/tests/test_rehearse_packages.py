from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "packaging" / "rehearse_packages.py"
SPEC = importlib.util.spec_from_file_location("thegn_rehearse_packages", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
rehearse = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = rehearse
SPEC.loader.exec_module(rehearse)


class RehearsePackagesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.deb = self.root / "thegn_0.1.0-alpha.22_amd64.deb"
        self.rpm = self.root / "thegn-0.1.0-alpha.22-1.x86_64.rpm"
        self.previous_deb = self.root / "thegn_0.1.0-alpha.21_amd64.deb"
        self.previous_rpm = self.root / "thegn-0.1.0-alpha.21-1.x86_64.rpm"
        for path in (self.deb, self.rpm, self.previous_deb, self.previous_rpm):
            path.write_bytes(f"fixture:{path.name}".encode())

    def args(self, **overrides: object) -> SimpleNamespace:
        values: dict[str, object] = {
            "deb": str(self.deb),
            "rpm": None,
            "previous_deb": None,
            "previous_rpm": None,
            "runtime": "podman",
            "deb_images": ["debian:test", "ubuntu:test"],
            "rpm_images": ["fedora:test"],
            "evidence_dir": self.root / "evidence",
            "step_timeout": 10.0,
            "dry_run": False,
        }
        values.update(overrides)
        return SimpleNamespace(**values)

    def successful_runner(self, calls: list[list[str]] | None = None):
        def run(argv: list[str], timeout: float) -> rehearse.StepResult:
            del timeout
            if calls is not None:
                calls.append(argv)
            output = '["debian@sha256:abc"]' if argv[1:3] == ["image", "inspect"] else "podman version 5\n"
            return rehearse.StepResult(0, output, "", 3)

        return run

    def test_plan_has_one_leg_per_image_with_install_version_and_uninstall_steps(self) -> None:
        args = self.args(rpm=str(self.rpm))
        legs = rehearse.plan(args)
        self.assertEqual(["debian:test", "ubuntu:test", "fedora:test"], [leg.image for leg in legs])
        self.assertEqual(3, len(legs))
        deb_names = [step.name for step in legs[0].steps]
        self.assertIn("install", deb_names)
        self.assertIn("version", deb_names)
        self.assertIn("tg-version", deb_names)
        self.assertIn("uninstall", deb_names)
        self.assertIn("absent-after-uninstall", deb_names)
        self.assertIn("dnf remove -y thegn", legs[-1].steps[-2].argv[-1])

    def test_dry_run_prints_the_plan_and_calls_no_runner(self) -> None:
        args = self.args(dry_run=True)
        with mock.patch.object(rehearse, "_run", side_effect=AssertionError("runner called")):
            with mock.patch("sys.stdout") as stdout:
                result = rehearse.main(["--deb", str(self.deb), "--runtime", "podman", "--dry-run"])
        self.assertEqual(0, result)
        self.assertTrue(any("podman pull docker.io/library/debian:stable-slim" in str(call) for call in stdout.write.call_args_list))
        self.assertFalse(args.evidence_dir.exists())

    def test_rejects_a_misnamed_package_before_any_runtime_call(self) -> None:
        bad = self.root / "package.deb"
        bad.write_bytes(b"bad")
        calls: list[list[str]] = []
        with self.assertRaisesRegex(rehearse.RehearsalError, str(bad)):
            rehearse.run_rehearsal(self.args(deb=str(bad)), self.successful_runner(calls))
        self.assertEqual([], calls)

    def test_rejects_a_missing_package_before_any_runtime_call(self) -> None:
        missing = self.root / "missing" / "thegn_0.1.0-alpha.22_amd64.deb"
        calls: list[list[str]] = []
        with self.assertRaisesRegex(rehearse.RehearsalError, str(missing)):
            rehearse.run_rehearsal(self.args(deb=str(missing)), self.successful_runner(calls))
        self.assertEqual([], calls)

    def test_rejects_a_symlinked_package_before_any_runtime_call(self) -> None:
        links = self.root / "links"
        links.mkdir()
        linked = links / self.deb.name
        linked.symlink_to(self.deb)
        calls: list[list[str]] = []
        with self.assertRaisesRegex(rehearse.RehearsalError, "symlink"):
            rehearse.run_rehearsal(self.args(deb=str(linked)), self.successful_runner(calls))
        self.assertEqual([], calls)

    def test_packages_are_mounted_read_only_one_file_at_a_time(self) -> None:
        legs = rehearse.plan(self.args(previous_deb=str(self.previous_deb)))
        start = legs[0].start_argv
        volumes = [start[index + 1] for index, value in enumerate(start[:-1]) if value == "--volume"]
        self.assertEqual(2, len(volumes))
        self.assertTrue(all(volume.endswith(":ro") for volume in volumes))
        self.assertEqual(
            {
                f"{self.deb}:/rehearsal/{self.deb.name}:ro",
                f"{self.previous_deb}:/rehearsal/{self.previous_deb.name}:ro",
            },
            set(volumes),
        )

    def test_failed_step_fails_the_run_skips_the_rest_and_still_removes_the_container(self) -> None:
        calls: list[list[str]] = []

        def failing(argv: list[str], timeout: float) -> rehearse.StepResult:
            calls.append(argv)
            if argv[1:3] == ["image", "inspect"]:
                return rehearse.StepResult(0, '["debian@sha256:abc"]', "", 1)
            if argv[1:2] == ["exec"] and "apt-get update" in argv[-1]:
                return rehearse.StepResult(42, "", "apt unavailable", 1)
            return rehearse.StepResult(0, "version", "", 1)

        evidence = rehearse.run_rehearsal(self.args(), failing)
        self.assertFalse(evidence.passed)
        self.assertTrue(any(argv[1:3] == ["rm", "-f"] for argv in calls))
        self.assertTrue(any(step.get("skipped") for step in evidence.legs[0].results))
        self.assertEqual("apt unavailable", next(step["stderr_tail"] for step in evidence.legs[0].results if step["name"] == "update"))

    def test_timeout_is_recorded_and_still_removes_the_container(self) -> None:
        calls: list[list[str]] = []

        def timing_out(argv: list[str], timeout: float) -> rehearse.StepResult:
            calls.append(argv)
            if argv[1:3] == ["image", "inspect"]:
                return rehearse.StepResult(0, '["debian@sha256:abc"]', "", 1)
            if argv[1:2] == ["exec"] and "apt-get update" in argv[-1]:
                return rehearse.StepResult(None, "", "deadline", 10, True)
            return rehearse.StepResult(0, "ok", "", 1)

        evidence = rehearse.run_rehearsal(self.args(), timing_out)
        self.assertFalse(evidence.passed)
        self.assertTrue(next(step for step in evidence.legs[0].results if step["name"] == "update")["timed_out"])
        self.assertTrue(any(argv[1:3] == ["rm", "-f"] for argv in calls))

    def test_upgrade_leg_installs_previous_then_new_and_checks_both_versions(self) -> None:
        leg = rehearse.plan(self.args(previous_deb=str(self.previous_deb)))[0]
        names = [step.name for step in leg.steps]
        self.assertLess(names.index("install-previous"), names.index("previous-version"))
        self.assertLess(names.index("previous-version"), names.index("upgrade-from-previous"))
        self.assertIn("alpha.21", leg.steps[names.index("previous-version")].argv[-1])
        self.assertIn("thegn_0.1.0-alpha.21_amd64.deb", leg.steps[names.index("install-previous")].argv[-1])
        self.assertIn("thegn_0.1.0-alpha.22_amd64.deb", leg.steps[names.index("upgrade-from-previous")].argv[-1])
        rpm_leg = rehearse.plan(self.args(rpm=str(self.rpm), deb=None, previous_rpm=str(self.previous_rpm)))[0]
        rpm_commands = [step.argv[-1] for step in rpm_leg.steps]
        self.assertTrue(any("dnf install -y /rehearsal/thegn-0.1.0-alpha.21-1.x86_64.rpm" in command for command in rpm_commands))
        self.assertTrue(any("dnf upgrade -y /rehearsal/thegn-0.1.0-alpha.22-1.x86_64.rpm" in command for command in rpm_commands))

    def test_evidence_records_package_sha256_image_digest_and_step_tails(self) -> None:
        evidence = rehearse.run_rehearsal(self.args(), self.successful_runner())
        rehearse._write_evidence(evidence, self.root / "written", "0.1.0-alpha.22")
        payload = json.loads(evidence.json_path.read_text(encoding="utf-8"))
        package = next(item for item in payload["packages"] if item["basename"] == self.deb.name)
        self.assertEqual(hashlib.sha256(self.deb.read_bytes()).hexdigest(), package["sha256"])
        self.assertEqual(["debian@sha256:abc"], payload["legs"][0]["image_digests"])
        self.assertTrue(payload["legs"][0]["steps"])
        self.assertIn("podman version", payload["runtime_version"])
        self.assertTrue(evidence.markdown_path.is_file())

    def test_evidence_file_is_never_overwritten(self) -> None:
        evidence = rehearse.run_rehearsal(self.args(), self.successful_runner())
        moment = datetime_for_test()
        with mock.patch.object(rehearse, "datetime", moment):
            rehearse._write_evidence(evidence, self.root / "evidence", "0.1.0-alpha.22")
            evidence.json_path.write_text("original", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                rehearse._write_evidence(evidence, self.root / "evidence", "0.1.0-alpha.22")
        self.assertEqual("original", evidence.json_path.read_text(encoding="utf-8"))

    def test_output_tail_keeps_the_last_bytes(self) -> None:
        output = "a" * (rehearse.OUTPUT_TAIL_BYTES + 5) + "tail"
        self.assertEqual("a" * (rehearse.OUTPUT_TAIL_BYTES - 4) + "tail", rehearse._tail(output))


def datetime_for_test():
    from datetime import datetime, timezone

    return type("FixedDateTime", (), {"now": staticmethod(lambda tz: datetime(2026, 1, 2, tzinfo=timezone.utc))})


if __name__ == "__main__":
    unittest.main()
