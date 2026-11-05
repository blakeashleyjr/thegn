#!/usr/bin/env python3
"""Offline preview conformance; live preview covers the normal-mode fixture and fault modes are unit-tested."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/thegn"))
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    root = Path(tempfile.mkdtemp(prefix="tg-preview-check-"))
    root.chmod(0o700)
    env = dict(os.environ)
    env.pop("THEGN_PROFILE", None)
    for key, name in (("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"),
                      ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime"),
                      ("XDG_DATA_HOME", "data"), ("THEGN_DIR", "thegn")):
        directory = root / name
        directory.mkdir(mode=0o700)
        env[key] = str(directory)
    binary = str(args.binary.resolve())
    steps = []

    def run(*command, success=True):
        result = subprocess.run([binary, "plugin", "preview", *map(str, command)],
                                cwd=repo, env=env, text=True, capture_output=True, timeout=12)
        if (result.returncode == 0) != success:
            raise AssertionError(f"{command}: exit {result.returncode}\n{result.stderr}\n{result.stdout}")
        return result

    def snapshot(*command):
        value = json.loads(run(*command).stdout)
        assert len(value["lines"]) == value["rows"]
        return value

    report = {"status": "failed", "artifacts": str(root), "steps": steps}
    try:
        fixture = json.loads((repo / "crates/thegn-host/src/cmd/plugin/gallery.json").read_text())
        fixture["inputs"] = [{"key": "tab"}, {"paste": "漢 and 👩🏽‍💻"},
                             {"resize": [45, 12]}, {"key": "left"}, {"resize": [120, 36]}]
        fixture_path = root / "fixture.json"
        fixture_path.write_text(json.dumps(fixture))
        trace = root / "fixture.jsonl"
        original = snapshot("--fixture", fixture_path, "--headless", "--size", "120x36",
                            "--record", trace, "--include-content")
        replay = snapshot("--replay", trace, "--headless", "--size", "120x36")
        assert original == replay, "record/replay screen or cursor diverged"
        if os.name == "posix":
            assert trace.stat().st_mode & 0o777 == 0o600
        before = trace.read_bytes()
        run("--fixture", "gallery", "--headless", "--record", trace, success=False)
        assert trace.read_bytes() == before, "existing recording was modified"
        steps.append("synthetic input/resize record/replay parity and private create-new trace")
        for size in ("24x8", "1x1"):
            snapshot("--fixture", "gallery", "--headless", "--size", size)
        run("--fixture", "gallery", "--headless", "--size", "0x24", success=False)
        steps.append("narrow rendering and invalid geometry refusal")
        fixture["inputs"] = [{"key": "enter", "paste": "ambiguous"}]
        fixture_path.write_text(json.dumps(fixture))
        run("--fixture", fixture_path, "--headless", success=False)
        steps.append("ambiguous scripted input refusal")
        if shutil.which("python3") is None:
            steps.append("live preview skipped: python3 missing")
        else:
            value = json.loads(run("--plugin-dir", repo / "examples/plugins/fixtures", "--headless",
                                   "--size", "80x10").stdout)
            assert value["live"]["surface"] == "fixture.workspace"
            assert value["live"]["revision"] == 1
            assert any("Ready" in line for line in value["lines"])
            steps.append("live headless preview of the fault fixture")
        report["status"] = "passed"
    except (AssertionError, OSError, ValueError, subprocess.SubprocessError) as error:
        report["reason"] = str(error)
    encoded = json.dumps(report, indent=2)
    print(encoded)
    if args.output:
        args.output.write_text(encoded + "\n")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
