#!/usr/bin/env python3
"""Regression tests for the remote TUI parity probe."""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("remote_parity_tui.py")


def load_probe():
    spec = importlib.util.spec_from_file_location("remote_parity_tui", SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load the TUI parity probe")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def read_env_dump(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        key, _, value = line.partition("=")
        if key and key not in values:
            values[key] = value
    return values


class RemoteParityTuiTests(unittest.TestCase):
    def test_self_test_uses_shell_and_reports_latency_json(self) -> None:
        completed = subprocess.run(
            [
                sys.executable,
                "-I",
                str(SCRIPT),
                "--bin",
                "/bin/false",
                "--repo",
                str(SCRIPT.parent.parent),
                "--samples",
                "1",
                "--timeout",
                "10",
                "--self-test",
            ],
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(len(completed.stdout.splitlines()), 1)
        payload = json.loads(completed.stdout)
        self.assertEqual(payload["status"], "PASS")
        self.assertEqual(set(payload), {"status", "note", "first", "next", "samples_file"})
        self.assertEqual(payload["first"]["n"], 1)
        self.assertEqual(payload["next"]["n"], 5)
        self.assertLess(payload["first"]["p50"], 50)
        self.assertLess(payload["next"]["p50"], 50)

    def test_samples_file_keeps_raw_timings_out_of_stdout(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples_path = Path(directory) / "samples.json"
            environment = os.environ.copy()
            environment["RP_TUI_SAMPLES_OUT"] = str(samples_path)
            completed = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    str(SCRIPT),
                    "--bin",
                    "/bin/false",
                    "--repo",
                    str(SCRIPT.parent.parent),
                    "--samples",
                    "1",
                    "--timeout",
                    "10",
                    "--self-test",
                ],
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
                env=environment,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            payload = json.loads(completed.stdout)
            self.assertEqual(payload["samples_file"], str(samples_path))
            samples = json.loads(samples_path.read_text(encoding="utf-8"))
            self.assertEqual(set(samples), {"first_key_ms", "next_key_ms"})
            self.assertEqual(len(samples["first_key_ms"]), 1)
            self.assertEqual(len(samples["next_key_ms"]), 5)

    def test_the_tui_gets_a_throwaway_config_and_home(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / "fake-tui"
            dump = root / "env.txt"
            fixture.write_text(
                "#!/bin/sh\n"
                'env >"$RP_TUI_TEST_ENV_DUMP"\n'
                "printf 'probe$ '\n"
                "exec /bin/sh -i\n",
                encoding="utf-8",
            )
            fixture.chmod(0o755)
            environment = {
                "HOME": "/home/owner",
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "RP_TUI_TEST_ENV_DUMP": str(dump),
                "SHELL": "/bin/sh",
                "THEGN_DIR": "/home/owner/.thegn",
                "XDG_CONFIG_HOME": "/home/owner/.config",
                "XDG_STATE_HOME": "/home/owner/.local/state",
            }
            completed = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    str(SCRIPT),
                    "--bin",
                    str(fixture),
                    "--repo",
                    str(SCRIPT.parent.parent),
                    "--samples",
                    "1",
                    "--timeout",
                    "10",
                ],
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
                env=environment,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            payload = json.loads(completed.stdout)
            self.assertEqual(payload["status"], "PASS", payload["note"])
            values = read_env_dump(dump)
            self.assertEqual(values["THEGN_SKIP_ONBOARDING"], "1")
            # Nothing the caller set for a real user survives: home, config and
            # state all move, and only unrelated env (the pane shell) passes on.
            throwaway = Path(values["HOME"]).parent
            self.assertNotEqual(values["HOME"], "/home/owner")
            self.assertEqual(values["SHELL"], "/bin/sh")
            config_dir = Path(values["THEGN_CONFIG_DIR"])
            self.assertEqual(config_dir, Path(values["XDG_CONFIG_HOME"]) / "thegn")
            self.assertTrue(config_dir.is_relative_to(throwaway))
            self.assertTrue(Path(values["XDG_STATE_HOME"]).is_relative_to(throwaway))
            self.assertTrue(Path(values["THEGN_DIR"]).is_relative_to(throwaway))
            self.assertEqual(values["THEGN_NO_DAEMON"], "1")

    def test_the_probe_types_nothing_when_no_shell_prompt_appears(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / "no-prompt"
            dump = root / "stdin.txt"
            fixture.write_text(
                '#!/bin/sh\nexec /bin/cat >"$RP_TUI_TEST_STDIN_DUMP"\n',
                encoding="utf-8",
            )
            fixture.chmod(0o755)
            environment = os.environ.copy()
            environment["RP_TUI_TEST_STDIN_DUMP"] = str(dump)
            completed = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    str(SCRIPT),
                    "--bin",
                    str(fixture),
                    "--repo",
                    str(SCRIPT.parent.parent),
                    "--samples",
                    "1",
                    "--timeout",
                    "2",
                ],
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
                env=environment,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            payload = json.loads(completed.stdout)
            self.assertEqual(payload["status"], "SKIP")
            self.assertIn("not typed", payload["note"])
            self.assertEqual(dump.read_text(encoding="utf-8"), "")

    def test_the_isolation_guard_resolves_and_refuses_the_owner_config(self) -> None:
        probe = load_probe()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "throwaway"
            root.mkdir()
            owner = {"HOME": "/home/owner"}
            self.assertEqual(
                probe.config_path_for(owner),
                Path("/home/owner/.config/thegn/config.toml"),
            )
            self.assertEqual(
                probe.config_path_for({"HOME": "/home/o", "XDG_CONFIG_HOME": "/cfg"}),
                Path("/cfg/thegn/config.toml"),
            )
            self.assertEqual(
                probe.config_path_for({"HOME": "/home/o", "THEGN_CONFIG_DIR": "/x/thegn"}),
                Path("/x/thegn/config.toml"),
            )
            self.assertEqual(
                probe.config_path_for({"HOME": "/home/o", "THEGN_CONFIG_DIR": "thegn"}),
                Path("/home/o/.config/thegn/config.toml"),
            )
            self.assertIn(
                "real user config",
                probe.isolation_refusal(owner, root, owner) or "",
            )
            child = probe.isolated_environment(owner, root)
            self.assertIsNone(probe.isolation_refusal(child, root, owner))
            self.assertEqual(child["HOME"], str(root / "home"))
            self.assertEqual(child["XDG_STATE_HOME"], str(root / "state"))
            self.assertEqual(child["THEGN_DIR"], str(root / "thegn"))

    def test_an_isolating_caller_keeps_its_state_dir(self) -> None:
        probe = load_probe()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "throwaway"
            root.mkdir()
            base = {
                "HOME": "/home/owner",
                "XDG_STATE_HOME": "/run/parity/state",
                "THEGN_DIR": "/run/parity/thegn",
            }
            child = probe.isolated_environment(base, root)
            self.assertEqual(child["XDG_STATE_HOME"], "/run/parity/state")
            self.assertEqual(child["THEGN_DIR"], "/run/parity/thegn")
            self.assertEqual(child["XDG_CONFIG_HOME"], str(root / "config"))
            self.assertNotIn("THEGN_NO_DAEMON", child)

    def test_state_inside_the_owner_home_is_replaced(self) -> None:
        probe = load_probe()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "throwaway"
            root.mkdir()
            base = {
                "HOME": "/home/owner",
                "XDG_STATE_HOME": "/home/owner/.local/state",
                "THEGN_DIR": "/home/owner/.thegn-devel/home",
            }
            child = probe.isolated_environment(base, root)
            self.assertEqual(child["XDG_STATE_HOME"], str(root / "state"))
            self.assertEqual(child["THEGN_DIR"], str(root / "thegn"))
            self.assertEqual(child["THEGN_NO_DAEMON"], "1")

    def test_any_caller_set_no_daemon_value_is_left_alone(self) -> None:
        # thegn disables the daemon for ANY set THEGN_NO_DAEMON value, "0"
        # included, so the probe only fills the variable in when it is unset.
        probe = load_probe()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "throwaway"
            root.mkdir()
            child = probe.isolated_environment(
                {"HOME": "/home/owner", "THEGN_NO_DAEMON": "0"}, root
            )
            self.assertEqual(child["THEGN_NO_DAEMON"], "0")
            self.assertEqual(child["PS1"], probe.PROBE_PS1)

    def test_an_unset_home_is_an_error_not_the_cwd(self) -> None:
        probe = load_probe()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "throwaway"
            root.mkdir()
            for base in ({}, {"HOME": ""}):
                with self.assertRaises(ValueError):
                    probe.isolated_environment(base, root)

    def test_only_the_named_env_tables_are_copied(self) -> None:
        probe = load_probe()
        text = (
            "[ui]\ntheme = 'x'\n\n"
            "[env.box]\nprovider = 'sprites'\n\n"
            "[env.box.provider_opts]\nregion = 'a'\n\n"
            "[env.other]\nprovider = 'nope'\n\n"
            '[env."box2"]\nprovider = "nope"\n\n'
            "[host.h]\nx = 1\n"
        )
        table = probe.env_table_text(text, "box")
        self.assertIn("[env.box]", table)
        self.assertIn("[env.box.provider_opts]", table)
        self.assertNotIn("other", table)
        self.assertNotIn("box2", table)
        self.assertNotIn("[ui]", table)
        self.assertNotIn("[host.h]", table)
        self.assertEqual(probe.env_table_text(text, "missing"), "")

    def test_seeding_reads_the_owner_config_and_never_writes_it(self) -> None:
        probe = load_probe()
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            owner_dir = base / "owner" / "thegn"
            owner_dir.mkdir(parents=True)
            owner_file = owner_dir / "config.toml"
            owner_file.write_text("[env.box]\nprovider = 'sprites'\n[ui]\nx = 1\n")
            before = owner_file.read_bytes()
            child_dir = base / "child"
            child_dir.mkdir()
            owner = {"HOME": "/home/o", "XDG_CONFIG_HOME": str(base / "owner")}
            self.assertIsNone(probe.seed_env_config(owner, child_dir, "box"))
            self.assertEqual(
                (child_dir / "config.toml").read_text(), "[env.box]\nprovider = 'sprites'\n"
            )
            self.assertEqual(owner_file.read_bytes(), before)
            self.assertIn("no [env.nope]", probe.seed_env_config(owner, child_dir, "nope") or "")

    def _drive_env_probe(self, local_name: str) -> dict:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_home = root / "owner"
            (config_home / "thegn").mkdir(parents=True)
            (config_home / "thegn" / "config.toml").write_text(
                "[env.box]\nprovider = 'sprites'\n", encoding="utf-8"
            )
            bin_dir = root / "bin"
            bin_dir.mkdir()
            uname = bin_dir / "uname"
            uname.write_text(f"#!/bin/sh\necho {local_name}\n", encoding="utf-8")
            uname.chmod(0o755)
            fixture = root / "fake-tui"
            fixture.write_text(
                "#!/bin/sh\nprintf 'probe$ '\nexec /bin/sh -i\n", encoding="utf-8"
            )
            fixture.chmod(0o755)
            environment = {
                "HOME": str(root),
                "PATH": f"{bin_dir}:{os.environ.get('PATH', '/usr/bin:/bin')}",
                "XDG_CONFIG_HOME": str(config_home),
            }
            completed = subprocess.run(
                [
                    sys.executable, "-I", str(SCRIPT), "--bin", str(fixture),
                    "--repo", str(SCRIPT.parent.parent), "--samples", "1",
                    "--timeout", "10", "--env", "box",
                ],
                capture_output=True, text=True, timeout=30, check=False, env=environment,
            )
            return json.loads(completed.stdout)

    def test_a_pane_on_another_host_passes_the_remote_check(self) -> None:
        payload = self._drive_env_probe("remote-box.internal")
        self.assertEqual(payload["status"], "PASS", payload["note"])

    def test_a_pane_on_this_host_fails_as_a_silent_local_fallback(self) -> None:
        import socket

        payload = self._drive_env_probe(socket.gethostname())
        self.assertEqual(payload["status"], "FAIL")
        self.assertIn("local shell", payload["note"])

    def test_a_missing_env_table_fails_before_driving_the_tui(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "thegn").mkdir()
            (root / "thegn" / "config.toml").write_text("[ui]\nx = 1\n")
            completed = subprocess.run(
                [
                    sys.executable, "-I", str(SCRIPT), "--bin", "/bin/false",
                    "--repo", str(root), "--env", "box", "--timeout", "2",
                ],
                capture_output=True, text=True, timeout=20, check=False,
                env={"HOME": str(root), "XDG_CONFIG_HOME": str(root), "PATH": "/usr/bin:/bin"},
            )
            payload = json.loads(completed.stdout)
            self.assertEqual(payload["status"], "FAIL")
            self.assertIn("[env.box]", payload["note"])

    def test_the_prompt_gate_ignores_repaints_without_a_prompt(self) -> None:
        probe = load_probe()
        self.assertIsNone(probe.shell_prompt(b"\x1b[2J\x1b[H\x1b[1mbuilding\x1b[0m\n"))
        self.assertEqual(
            probe.shell_prompt(b"\x1b[4;1Hbash-5.2$ \x1b[4;12H\x1b[7m P20B \x1b[0m"),
            "bash-5.2$",
        )
        self.assertIsNone(
            probe.shell_prompt(b"printf '%s%s\\n' RP TUIREADY\r\nRPTUIREADY\r\n")
        )

    def test_the_status_bar_is_not_a_shell_prompt(self) -> None:
        probe = load_probe()
        rows = (
            "\x1b[40;1H\U0001f4be 21% \u00b7 \U0001f525 1% \u00b7 "
            "\u2193 476K \u2191 245K \u00b7 79%",
            "\x1b[40;1H\u2193 1.1M \u2191 464K \u00b7 \u26a1 79%",
        )
        for row in rows:
            self.assertIsNone(probe.shell_prompt(row.encode("utf-8")))
        prompt = "\x1b[40;1Hblake@host ~/code %"
        self.assertEqual(probe.shell_prompt(prompt.encode("utf-8")), "blake@host ~/code %")


if __name__ == "__main__":
    unittest.main()
