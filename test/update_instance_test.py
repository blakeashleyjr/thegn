#!/usr/bin/env python3
"""Regression tests for scripts/update_instance.py (`just update`). No network, no real home."""

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("update_instance", ROOT / "scripts/update_instance.py")
upd = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(upd)

TARGET = upd.TARGET


def done(stdout="", code=0):
    return subprocess.CompletedProcess([], code, stdout=stdout)


def script_binary(path: Path, version: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!/bin/sh\necho 'thegn {version}'\n")
    path.chmod(0o755)


def make_release(root: Path, tag: str, version: str, *, bad_sha=False, binary_version=None):
    """Build a fake release dir holding the tarball and .sha256 for `tag`."""
    release = root / "release"
    release.mkdir(exist_ok=True)
    stage = root / "stage"
    script_binary(stage / "thegn", binary_version or version)
    archive = release / f"thegn-{tag}-{TARGET}.tar.gz"
    with tarfile.open(archive, "w:gz") as bundle:
        bundle.add(stage / "thegn", arcname="thegn")
    digest = "0" * 64 if bad_sha else hashlib.sha256(archive.read_bytes()).hexdigest()
    (release / f"thegn-{tag}-{TARGET}.sha256").write_text(f"{digest}  {archive.name}\n")
    return release


class FakeCtx(upd.Ctx):
    def __init__(self, home: Path, tags=("v0.1.0-alpha.101", "v0.1.0-alpha.130", "v0.1.0-alpha.99"),
                 gh=True, release_dir=None, tty=False, answers=()):
        super().__init__(home)
        self.out, self.err = io.StringIO(), io.StringIO()
        self.tags = list(tags)
        self.gh = gh
        self.release_dir = release_dir
        self.root = ROOT
        self.calls = []
        self.answers = list(answers)
        self.asked = []
        self.isatty = lambda: tty
        self.ask = self._ask
        self.which = lambda name: f"/usr/bin/{name}" if name == "git" or (name == "gh" and self.gh) else None
        self.run = self._fake_run
        self.run_inherit = self._fake_inherit
        self.download = self._fake_download
        self.fetch = self._fake_fetch
        self.attest_ok = True
        self.builder_code = 0
        self.fail_list = False

    def _ask(self, prompt):
        self.asked.append(prompt)
        return self.answers.pop(0)

    def _fake_fetch(self, url):
        self.calls.append(("fetch", url))
        return json.dumps([{"tag_name": t, "draft": False} for t in self.tags] + [{"tag_name": "v9.9.9", "draft": True}]).encode()

    def _fake_run(self, argv, **kwargs):
        self.calls.append(("run", list(argv)))
        if argv[:3] == ["gh", "release", "list"]:
            if self.fail_list:
                return done("boom", 1)
            return done(json.dumps([{"tagName": t, "isDraft": False} for t in self.tags] + [{"tagName": "v9.9.9", "isDraft": True}]))
        if argv[:3] == ["gh", "attestation", "verify"]:
            return done("verified" if self.attest_ok else "no attestation", 0 if self.attest_ok else 1)
        if argv[0] == "git":
            sub = argv[3:]
            if sub[:2] == ["rev-parse", "--abbrev-ref"]:
                return done(self.branch + "\n")
            if sub[0] == "status":
                return done(self.porcelain)
        return subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=30,
                              **{k: v for k, v in kwargs.items() if k in ("env",)})

    branch = "feature"
    porcelain = ""

    def _fake_inherit(self, argv, **kwargs):
        self.calls.append(("inherit", list(argv)))
        if str(argv[0]) == str(self.remote_build):
            if self.builder_code == 0:
                script_binary(Path(argv[2]), "0.1.0-alpha.777")
            return done("", self.builder_code)
        return done("", 0)

    def _fake_download(self, tag, names, dest):
        self.calls.append(("download", tag, list(names)))
        for name in names:
            (dest / name).write_bytes((self.release_dir / name).read_bytes())

    def inherits(self):
        return [c[1] for c in self.calls if c[0] == "inherit"]


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="thegn-update-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.home = self.root / "home"
        self.home.mkdir()

    def ctx(self, **kwargs):
        return FakeCtx(self.home, **kwargs)

    def stable(self, version):
        script_binary(self.home / ".local/lib/thegn/thegn", version)


class VersionTests(unittest.TestCase):
    def test_ordering_is_numeric_not_lexical(self):
        self.assertLess(upd.parse_version("0.1.0-alpha.99"), upd.parse_version("v0.1.0-alpha.100"))
        self.assertLess(upd.parse_version("0.1.0-alpha.130"), upd.parse_version("0.1.0"))

    def test_rejects_garbage(self):
        with self.assertRaises(upd.Refusal):
            upd.parse_version("latest-ish")


class TagResolutionTests(Base):
    def test_latest_picks_highest_non_draft(self):
        self.assertEqual(upd.resolve_tag(self.ctx(), "latest"), "v0.1.0-alpha.130")
        self.assertEqual(upd.resolve_tag(self.ctx(), None), "v0.1.0-alpha.130")

    def test_explicit_versions_are_normalised(self):
        self.assertEqual(upd.resolve_tag(self.ctx(), "0.1.0-alpha.101"), "v0.1.0-alpha.101")
        self.assertEqual(upd.resolve_tag(self.ctx(), "v0.1.0-alpha.101"), "v0.1.0-alpha.101")

    def test_falls_back_to_the_api_without_gh(self):
        ctx = self.ctx(gh=False)
        self.assertEqual(upd.resolve_tag(ctx, "latest"), "v0.1.0-alpha.130")
        self.assertTrue(any(c[0] == "fetch" for c in ctx.calls))

    def test_falls_back_to_the_api_when_gh_fails(self):
        ctx = self.ctx()
        ctx.fail_list = True
        self.assertEqual(upd.resolve_tag(ctx, "latest"), "v0.1.0-alpha.130")
        self.assertTrue(any(c[0] == "fetch" for c in ctx.calls))


class StableTests(Base):
    def test_installs_keeps_prev_and_never_signals(self):
        self.stable("0.1.0-alpha.100")
        release = make_release(self.root, "v0.1.0-alpha.130", "0.1.0-alpha.130")
        ctx = self.ctx(release_dir=release)
        self.assertEqual(upd.cmd_stable(ctx, "latest", False, False, False, False), 0)
        installed = self.home / ".local/lib/thegn/thegn"
        self.assertEqual(upd.binary_version(ctx, installed), "0.1.0-alpha.130")
        self.assertEqual(upd.binary_version(ctx, installed.with_name("thegn.prev")), "0.1.0-alpha.100")
        self.assertIn("thegn daemon stop", ctx.out.getvalue())
        self.assertFalse([c for c in ctx.calls if c[0] == "run" and "daemon" in c[1]])
        self.assertTrue(any(c[0] == "run" and c[1][:3] == ["gh", "attestation", "verify"] for c in ctx.calls))

    def test_sha_mismatch_refuses_and_leaves_stable_alone(self):
        self.stable("0.1.0-alpha.100")
        release = make_release(self.root, "v0.1.0-alpha.130", "0.1.0-alpha.130", bad_sha=True)
        ctx = self.ctx(release_dir=release)
        with self.assertRaisesRegex(upd.Refusal, "sha256 mismatch"):
            upd.cmd_stable(ctx, "latest", False, False, False, False)
        self.assertEqual(upd.binary_version(ctx, ctx.stable_bin), "0.1.0-alpha.100")
        self.assertFalse(ctx.stable_bin.with_name("thegn.prev").exists())

    def test_version_mismatch_between_tag_and_binary_refuses(self):
        self.stable("0.1.0-alpha.100")
        release = make_release(self.root, "v0.1.0-alpha.130", "0.1.0-alpha.130", binary_version="0.1.0-alpha.129")
        with self.assertRaisesRegex(upd.Refusal, "expected"):
            upd.cmd_stable(self.ctx(release_dir=release), "latest", False, False, False, False)

    def test_downgrade_refused_without_force(self):
        self.stable("0.1.0-alpha.120")
        release = make_release(self.root, "v0.1.0-alpha.101", "0.1.0-alpha.101")
        ctx = self.ctx(release_dir=release)
        with self.assertRaisesRegex(upd.Refusal, "downgrade"):
            upd.cmd_stable(ctx, "0.1.0-alpha.101", False, False, False, False)
        self.assertEqual(upd.cmd_stable(ctx, "0.1.0-alpha.101", False, True, False, False), 0)
        self.assertEqual(upd.binary_version(ctx, ctx.stable_bin), "0.1.0-alpha.101")

    def test_same_version_is_a_noop(self):
        self.stable("0.1.0-alpha.130")
        ctx = self.ctx()
        self.assertEqual(upd.cmd_stable(ctx, "latest", False, False, False, False), 0)
        self.assertIn("already at", ctx.out.getvalue())
        self.assertFalse([c for c in ctx.calls if c[0] == "download"])

    def test_missing_gh_requires_no_attest(self):
        self.stable("0.1.0-alpha.100")
        release = make_release(self.root, "v0.1.0-alpha.130", "0.1.0-alpha.130")
        ctx = self.ctx(gh=False, release_dir=release)
        with self.assertRaisesRegex(upd.Refusal, "--no-attest"):
            upd.cmd_stable(ctx, "0.1.0-alpha.130", False, False, False, False)
        self.assertEqual(upd.cmd_stable(ctx, "0.1.0-alpha.130", False, False, True, False), 0)
        self.assertIn("NOT verified", ctx.err.getvalue())

    def test_failed_attestation_refuses(self):
        self.stable("0.1.0-alpha.100")
        release = make_release(self.root, "v0.1.0-alpha.130", "0.1.0-alpha.130")
        ctx = self.ctx(release_dir=release)
        ctx.attest_ok = False
        with self.assertRaisesRegex(upd.Refusal, "attestation"):
            upd.cmd_stable(ctx, "latest", False, False, False, False)
        self.assertEqual(upd.binary_version(ctx, ctx.stable_bin), "0.1.0-alpha.100")

    def test_rollback_swaps_and_toggles(self):
        self.stable("0.1.0-alpha.100")
        release = make_release(self.root, "v0.1.0-alpha.130", "0.1.0-alpha.130")
        ctx = self.ctx(release_dir=release)
        upd.cmd_stable(ctx, "latest", False, False, False, False)
        self.assertEqual(upd.cmd_stable(ctx, None, False, False, False, True), 0)
        self.assertEqual(upd.binary_version(ctx, ctx.stable_bin), "0.1.0-alpha.100")
        self.assertEqual(upd.binary_version(ctx, ctx.stable_bin.with_name("thegn.prev")), "0.1.0-alpha.130")

    def test_rollback_without_prev_refuses(self):
        self.stable("0.1.0-alpha.100")
        with self.assertRaisesRegex(upd.Refusal, "nothing to roll back"):
            upd.cmd_stable(self.ctx(), None, False, False, False, True)

    def test_dry_run_changes_and_downloads_nothing(self):
        self.stable("0.1.0-alpha.100")
        ctx = self.ctx()
        self.assertEqual(upd.cmd_stable(ctx, "latest", True, False, False, False), 0)
        self.assertIn("plan", ctx.out.getvalue())
        self.assertIn("v0.1.0-alpha.130", ctx.out.getvalue())
        self.assertFalse([c for c in ctx.calls if c[0] == "download"])
        self.assertEqual(upd.binary_version(ctx, ctx.stable_bin), "0.1.0-alpha.100")


class DevTests(Base):
    def test_arg_splitting_keeps_flag_values_out_of_the_target(self):
        self.assertEqual(upd.split_dev_args(["--level", "debug", "--here"]),
                         ("release", False, False, ["--level", "debug", "--here"]))
        self.assertEqual(upd.split_dev_args(["main", "--restart-daemon", "--dry-run"]),
                         ("main", True, False, ["--restart-daemon"]))

    def test_release_target_hands_the_verified_binary_to_dev_live(self):
        release = make_release(self.root, "v0.1.0-alpha.130", "0.1.0-alpha.130")
        ctx = self.ctx(release_dir=release)
        self.assertEqual(upd.cmd_dev(ctx, ["release", "--here"]), 0)
        (argv,) = ctx.inherits()
        self.assertIn("--prebuilt", argv)
        self.assertTrue(argv[0].endswith("python3") or "python" in os.path.basename(argv[0]))
        self.assertTrue(argv[2].endswith("scripts/dev_live.py"))
        self.assertEqual(argv[-1], "--here")

    def test_release_dry_run_does_not_download(self):
        ctx = self.ctx()
        self.assertEqual(upd.cmd_dev(ctx, ["release=0.1.0-alpha.101", "--dry-run"]), 0)
        self.assertIn("v0.1.0-alpha.101", ctx.out.getvalue())
        self.assertFalse([c for c in ctx.calls if c[0] in ("download", "inherit")])

    def make_builder(self, ctx):
        builder = self.root / "rbuild.sh"
        builder.write_text("#!/bin/sh\n")
        builder.chmod(0o755)
        ctx.environ["THEGN_REMOTE_BUILD"] = str(builder)
        return builder

    def test_mac_build_success_installs_via_prebuilt(self):
        ctx = self.ctx()
        self.make_builder(ctx)
        self.assertEqual(upd.cmd_dev(ctx, ["main"]), 0)
        builder_call, live_call = ctx.inherits()
        self.assertEqual(builder_call[1], "main")
        self.assertIn("--prebuilt", live_call)
        self.assertEqual(ctx.asked, [])

    def test_checkout_refuses_a_dirty_worktree(self):
        ctx = self.ctx()
        self.make_builder(ctx)
        FakeCtx.porcelain = " M src.rs\n"
        self.addCleanup(setattr, FakeCtx, "porcelain", "")
        with self.assertRaisesRegex(upd.Refusal, "commit first"):
            upd.cmd_dev(ctx, ["checkout"])
        self.assertEqual(ctx.inherits(), [])

    def test_checkout_builds_the_current_branch(self):
        ctx = self.ctx()
        self.make_builder(ctx)
        self.assertEqual(upd.cmd_dev(ctx, ["checkout"]), 0)
        self.assertEqual(ctx.inherits()[0][1], "feature")

    def test_mac_failure_prompts_and_yes_builds_locally(self):
        ctx = self.ctx(tty=True, answers=["y"])
        self.make_builder(ctx)
        ctx.builder_code = 3
        self.assertEqual(upd.cmd_dev(ctx, ["checkout"]), 0)
        self.assertEqual(ctx.asked, ["Mac build failed (remote builder exited 3). Build locally instead? "
                                     "This compiles on this machine. [y/N] "])
        local = ctx.inherits()[-1]
        self.assertNotIn("--prebuilt", local)
        self.assertTrue(local[2].endswith("scripts/dev_live.py"))

    def test_mac_failure_prompt_no_means_no_local_build(self):
        for answer in ("n", ""):
            ctx = self.ctx(tty=True, answers=[answer])
            self.make_builder(ctx)
            ctx.builder_code = 1
            self.assertEqual(upd.cmd_dev(ctx, ["checkout"]), 1)
            self.assertEqual(len(ctx.inherits()), 1, "only the failed Mac build ran")

    def test_non_interactive_failure_refuses_without_asking(self):
        ctx = self.ctx(tty=False)
        self.make_builder(ctx)
        ctx.builder_code = 1
        self.assertEqual(upd.cmd_dev(ctx, ["checkout"]), 1)
        self.assertEqual(ctx.asked, [])
        self.assertIn("Mac build failed (remote builder exited 1). Build locally instead?", ctx.err.getvalue())
        self.assertEqual(len(ctx.inherits()), 1)

    def test_missing_builder_is_a_fallback_reason(self):
        ctx = self.ctx(tty=True, answers=["n"])
        ctx.environ["THEGN_REMOTE_BUILD"] = str(self.root / "absent.sh")
        self.assertEqual(upd.cmd_dev(ctx, ["checkout"]), 1)
        self.assertIn("remote builder not found", ctx.asked[0])

    def test_local_fallback_refuses_for_a_branch_that_is_not_checked_out(self):
        ctx = self.ctx(tty=True, answers=["y"])
        self.make_builder(ctx)
        ctx.builder_code = 1
        with self.assertRaisesRegex(upd.Refusal, "check out main first"):
            upd.cmd_dev(ctx, ["main"])
        self.assertEqual(len(ctx.inherits()), 1)


class StatusTests(Base):
    def test_status_reports_behind_and_latest(self):
        self.stable("0.1.0-alpha.100")
        script_binary(self.home / ".thegn-devel/bin/thegn", "0.1.0-alpha.130")
        ctx = self.ctx()
        self.assertEqual(upd.main([], ctx), 0)
        text = ctx.out.getvalue()
        self.assertIn("latest release: v0.1.0-alpha.130", text)
        self.assertRegex(text, r"stable\s+thegn 0.1.0-alpha.100\s+BEHIND")
        self.assertRegex(text, r"dev\s+thegn 0.1.0-alpha.130\s+up to date")

    def test_empty_arguments_from_just_mean_status(self):
        ctx = self.ctx()
        self.assertEqual(upd.main(["", ""], ctx), 0)
        self.assertIn("latest release", ctx.out.getvalue())

    def test_unknown_target_refuses(self):
        with self.assertRaises(upd.Refusal):
            upd.main(["bogus"], self.ctx())


class ArchiveTests(Base):
    def test_extract_ignores_traversal_members(self):
        archive = self.root / "evil.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            data = b"x"
            info = tarfile.TarInfo("../escape")
            info.size = 1
            bundle.addfile(info, io.BytesIO(data))
        with self.assertRaises(upd.Refusal):
            upd.extract_binary(archive, self.root)
        self.assertFalse((self.root.parent / "escape").exists())


if __name__ == "__main__":
    unittest.main()
