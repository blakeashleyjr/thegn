#!/usr/bin/env python3
"""`just update`: move the STABLE or DEV thegn instance to a release or a source build.

  just update                                  status: stable, dev and latest release versions
  just update stable [VERSION|latest]          install a verified GitHub release as the stable binary
  just update stable --rollback                swap the previous stable binary back in
  just update dev [release[=VERSION]|main|checkout|<branch>] [dev_live args]

Stable is the binary at ~/.local/lib/thegn/thegn. It is replaced by an atomic rename,
so a running stable daemon and open panes keep the old inode until the user restarts
thegn; this script never stops or signals any process. The dev instance is installed
by scripts/dev_live.py (`--prebuilt`), which validates the dev config first.

Source builds prefer the remote Mac builder (THEGN_REMOTE_BUILD, default
~/.local/share/thegn-devbox/rbuild.sh) and only compile locally after an explicit yes.
Standard library only; every side effect goes through `Ctx` so tests inject fakes."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

REPO = "blakeashleyjr/thegn"
WORKFLOW = "release.yml"
TARGET = "x86_64-unknown-linux-gnu"
DEFAULT_REMOTE_BUILD = "/home/user/.local/share/thegn-devbox/rbuild.sh"
ROOT = Path(__file__).resolve().parent.parent
DEV_LIVE_VALUE_FLAGS = ("--level", "--size-mb", "--files", "--prebuilt")
RESTART_COMMAND = "thegn daemon stop"


class Refusal(Exception):
    """A visible refusal; the message says what to do instead."""


# ---------------------------------------------------------------- versions
def parse_version(text: str):
    """Semver-ish sort key for `0.1.0-alpha.130` (a leading `v` is accepted)."""
    match = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?", text.strip())
    if not match:
        raise Refusal(f"not a version: {text!r}")
    core = tuple(int(part) for part in match.group(1, 2, 3))
    pre = match.group(4)
    if pre is None:
        return core, 1, ()
    parts = tuple((0, int(p), "") if p.isdigit() else (1, 0, p) for p in pre.split("."))
    return core, 0, parts


def normalize_tag(text: str) -> str:
    text = text.strip()
    parse_version(text)
    return text if text.startswith("v") else f"v{text}"


def version_of_tag(tag: str) -> str:
    return tag[1:] if tag.startswith("v") else tag


# ---------------------------------------------------------------- side effects
class Ctx:
    """Every side effect lives here; unit tests replace the attributes."""

    def __init__(self, home: Path | None = None):
        # The account home, as scripts/dev_live.py does: $HOME can be a per-profile dir.
        self.home = home or Path(pwd.getpwuid(os.getuid()).pw_dir)
        self.environ = dict(os.environ)
        self.out = sys.stdout
        self.err = sys.stderr
        self.fetch = self._fetch
        self.run = self._run
        self.run_inherit = self._run_inherit
        self.download = self._download
        self.which = shutil.which
        self.isatty = lambda: sys.stdin.isatty() and sys.stdout.isatty()
        self.ask = input
        self.root = ROOT

    # -- paths
    @property
    def stable_bin(self) -> Path:
        return self.home / ".local" / "lib" / "thegn" / "thegn"

    @property
    def dev_bin(self) -> Path:
        return self.home / ".thegn-devel" / "bin" / "thegn"

    @property
    def dev_record(self) -> Path:
        return self.home / ".thegn-devel" / "bin" / "thegn.build.json"

    @property
    def remote_build(self) -> Path:
        return Path(self.environ.get("THEGN_REMOTE_BUILD") or DEFAULT_REMOTE_BUILD)

    def say(self, message: str = "") -> None:
        print(message, file=self.out)

    def warn(self, message: str) -> None:
        print(message, file=self.err)

    # -- defaults
    def _fetch(self, url: str) -> bytes:
        request = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json",
                                                       "User-Agent": "thegn-update"})
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read()

    def _run(self, argv, **kwargs):
        kwargs.setdefault("stdout", subprocess.PIPE)
        kwargs.setdefault("stderr", subprocess.STDOUT)
        kwargs.setdefault("timeout", 600)
        return subprocess.run(argv, **kwargs)

    def _run_inherit(self, argv, **kwargs):
        return subprocess.run(argv, check=False, **kwargs)

    def _download(self, tag: str, names: list[str], dest: Path) -> None:
        if self.which("gh"):
            args = ["gh", "release", "download", tag, "-R", REPO, "-D", str(dest), "--clobber"]
            for name in names:
                args += ["-p", name]
            result = self.run(args, text=True)
            if result.returncode == 0 and all((dest / n).is_file() for n in names):
                return
            self.warn(f"gh release download failed; falling back to https:\n{(result.stdout or '')[-400:]}")
        for name in names:
            data = self.fetch(f"https://github.com/{REPO}/releases/download/{tag}/{name}")
            (dest / name).write_bytes(data)


# ---------------------------------------------------------------- releases
def list_tags(ctx: Ctx) -> list[str]:
    """Published (non-draft) release tags. Releases are all pre-releases, so GitHub's
    `/releases/latest` 404s; the newest is chosen by version instead."""
    if ctx.which("gh"):
        result = ctx.run(["gh", "release", "list", "-R", REPO, "-L", "100", "--json", "tagName,isDraft"], text=True)
        if result.returncode == 0:
            try:
                return [r["tagName"] for r in json.loads(result.stdout) if not r.get("isDraft")]
            except (ValueError, KeyError, TypeError):
                pass
        ctx.warn("gh release list failed; falling back to the GitHub API")
    try:
        data = json.loads(ctx.fetch(f"https://api.github.com/repos/{REPO}/releases?per_page=100"))
    except (OSError, ValueError) as error:
        raise Refusal(f"could not list releases ({error}); check the network or pass an explicit VERSION") from error
    return [r["tag_name"] for r in data if not r.get("draft")]


def resolve_tag(ctx: Ctx, want: str | None) -> str:
    if want in (None, "", "latest"):
        tags = []
        for tag in list_tags(ctx):
            try:
                parse_version(tag)
                tags.append(tag)
            except Refusal:
                continue
        if not tags:
            raise Refusal(f"no published releases found for {REPO}")
        return max(tags, key=parse_version)
    return normalize_tag(want)


def asset_names(tag: str) -> tuple[str, str]:
    stem = f"thegn-{tag}-{TARGET}"
    return f"{stem}.tar.gz", f"{stem}.sha256"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_sha256(archive: Path, sha_file: Path) -> None:
    fields = sha_file.read_text(errors="replace").split()
    if not fields or not re.fullmatch(r"[0-9a-fA-F]{64}", fields[0]):
        raise Refusal(f"{sha_file.name} does not hold a sha256 digest; refusing to install")
    if len(fields) > 1 and Path(fields[1].lstrip("*")).name != archive.name:
        raise Refusal(f"{sha_file.name} is for {fields[1]}, not {archive.name}; refusing to install")
    actual = sha256_file(archive)
    if actual != fields[0].lower():
        raise Refusal(f"sha256 mismatch for {archive.name}: expected {fields[0].lower()}, got {actual}; nothing was installed")


def verify_attestation(ctx: Ctx, archive: Path, no_attest: bool) -> None:
    if no_attest:
        ctx.warn("WARNING: --no-attest: the build-provenance attestation was NOT verified")
        return
    if not ctx.which("gh"):
        raise Refusal("gh is not installed, so the build-provenance attestation cannot be verified; "
                      "install gh, or pass --no-attest to accept the sha256 check alone")
    result = ctx.run(["gh", "attestation", "verify", str(archive), "--repo", REPO,
                      "--signer-workflow", f"{REPO}/.github/workflows/{WORKFLOW}"], text=True)
    if result.returncode != 0:
        raise Refusal(f"attestation verification failed; nothing was installed:\n{(result.stdout or '')[-800:]}")
    ctx.say("attestation: verified (signer workflow release.yml)")


def extract_binary(archive: Path, dest_dir: Path) -> Path:
    """Write only the root `thegn` member (never extractall: archive paths are untrusted)."""
    target = dest_dir / "thegn"
    with tarfile.open(archive) as bundle:
        try:
            member = bundle.getmember("thegn")
        except KeyError as error:
            raise Refusal(f"{archive.name} has no root `thegn` binary") from error
        if not member.isfile():
            raise Refusal(f"`thegn` in {archive.name} is not a regular file")
        source = bundle.extractfile(member)
        if source is None:
            raise Refusal(f"cannot read `thegn` from {archive.name}")
        with source, target.open("wb") as handle:
            shutil.copyfileobj(source, handle)
    target.chmod(0o755)
    return target


def binary_version(ctx: Ctx, binary: Path) -> str | None:
    """`thegn 0.1.0-alpha.100` -> `0.1.0-alpha.100`; None when absent or not runnable."""
    if not binary.is_file():
        return None
    env = dict(ctx.environ, THEGN_NO_MIGRATE="1")
    try:
        result = ctx.run([str(binary), "--version"], text=True, env=env, timeout=30)
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    match = re.search(r"(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)", result.stdout or "")
    return match.group(1) if match else None


def fetch_release_binary(ctx: Ctx, tag: str, workdir: Path, no_attest: bool) -> Path:
    """Download, verify and sanity-run a release binary inside `workdir`; return its path."""
    archive_name, sha_name = asset_names(tag)
    ctx.say(f"downloading {archive_name} + .sha256 into {workdir}")
    ctx.download(tag, [archive_name, sha_name], workdir)
    archive = workdir / archive_name
    verify_sha256(archive, workdir / sha_name)
    ctx.say("sha256: ok")
    verify_attestation(ctx, archive, no_attest)
    extracted = workdir / "extracted"
    extracted.mkdir()
    binary = extract_binary(archive, extracted)
    found = binary_version(ctx, binary)
    want = version_of_tag(tag)
    if found != want:
        raise Refusal(f"the extracted binary reports version {found!r}, expected {want!r}; refusing to install")
    ctx.say(f"binary: reports thegn {found}")
    return binary


# ---------------------------------------------------------------- stable
def _atomic_copy(source: Path, destination: Path) -> None:
    temporary = destination.with_name(f".{destination.name}.new.{os.getpid()}")
    try:
        shutil.copyfile(source, temporary)
        temporary.chmod(0o755)
        with temporary.open("rb") as handle:
            os.fsync(handle.fileno())
        os.replace(temporary, destination)
    finally:
        if temporary.exists():
            temporary.unlink()


def _keep_previous(current: Path, prev: Path) -> None:
    """Hardlink (same dir, atomic, no 139 MB copy) the current binary to thegn.prev."""
    temporary = prev.with_name(f".{prev.name}.keep.{os.getpid()}")
    try:
        try:
            os.link(current, temporary)
        except OSError:
            shutil.copyfile(current, temporary)
            temporary.chmod(0o755)
        os.replace(temporary, prev)
    finally:
        if temporary.exists():
            temporary.unlink()


def restart_notice(ctx: Ctx, what: str) -> None:
    ctx.say("")
    ctx.say(f"The running {what} daemon and any open panes keep the OLD binary until you restart thegn.")
    ctx.say(f"To switch: quit thegn, run `{RESTART_COMMAND}` (the repo-documented way to stop the pane daemon,")
    ctx.say("see docs/help/upgrading-and-uninstalling.md), then launch thegn again. This command did not stop anything.")
    ctx.say("The state database schema only moves forward; `thegn doctor` shows build vs on-disk versions.")


def cmd_stable(ctx: Ctx, version: str | None, dry_run: bool, force: bool, no_attest: bool, rollback: bool) -> int:
    current = ctx.stable_bin
    prev = current.with_name("thegn.prev")
    current_version = binary_version(ctx, current)
    ctx.say(f"stable binary: {current} ({'thegn ' + current_version if current_version else 'not installed'})")

    if rollback:
        previous_version = binary_version(ctx, prev)
        if previous_version is None:
            raise Refusal(f"nothing to roll back to: {prev} is missing or does not run")
        ctx.say(f"rollback: {current_version or '(none)'} -> {previous_version} (the current binary becomes thegn.prev)")
        if dry_run:
            ctx.say("dry-run: nothing changed")
            return 0
        swap = current.with_name(f".thegn.rollback.{os.getpid()}")
        try:
            if current.exists():
                os.link(current, swap)
            _atomic_copy(prev, current)
            if swap.exists():
                os.replace(swap, prev)
        finally:
            if swap.exists():
                swap.unlink()
        ctx.say(f"stable is now thegn {previous_version}")
        restart_notice(ctx, "stable")
        return 0

    tag = resolve_tag(ctx, version)
    target_version = version_of_tag(tag)
    if current_version:
        order = (parse_version(target_version) > parse_version(current_version)) - (parse_version(target_version) < parse_version(current_version))
        if order < 0 and not force:
            raise Refusal(f"{tag} is older than the installed {current_version}; refusing to downgrade (use --force, or --rollback for the previous binary)")
        if order == 0 and not force:
            ctx.say(f"stable is already at {current_version}; nothing to do (--force reinstalls)")
            return 0
    archive_name, _ = asset_names(tag)
    plan = [
        f"resolve:  {tag}",
        f"download: {archive_name} (+ .sha256) from github.com/{REPO}, into a fresh temp dir",
        "verify:   sha256" + ("" if no_attest else " + gh attestation"),
        f"install:  {current}  (previous kept as {prev.name})",
        "never:    stop, signal or restart the running stable daemon",
    ]
    for line in plan:
        ctx.say(("plan  " if dry_run else "step  ") + line)
    if dry_run:
        ctx.say("dry-run: nothing downloaded or changed")
        return 0
    with tempfile.TemporaryDirectory(prefix="thegn-update-") as scratch:
        binary = fetch_release_binary(ctx, tag, Path(scratch), no_attest)
        current.parent.mkdir(parents=True, exist_ok=True)
        if current.exists():
            _keep_previous(current, prev)
        _atomic_copy(binary, current)
    ctx.say(f"stable is now thegn {target_version} at {current}")
    restart_notice(ctx, "stable")
    return 0


# ---------------------------------------------------------------- dev
def dev_live_argv(ctx: Ctx, prebuilt: Path | None, passthrough: list[str]) -> list[str]:
    argv = [sys.executable, "-B", str(ctx.root / "scripts" / "dev_live.py")]
    if prebuilt is not None:
        argv += ["--prebuilt", str(prebuilt)]
    return argv + passthrough


def split_dev_args(args: list[str]):
    """Return (target, dry_run, no_attest, passthrough); flag values never become the target."""
    target, dry_run, no_attest, passthrough = None, False, False, []
    index = 0
    while index < len(args):
        arg = args[index]
        if arg == "--dry-run":
            dry_run = True
        elif arg == "--no-attest":
            no_attest = True
        elif arg in DEV_LIVE_VALUE_FLAGS and index + 1 < len(args):
            passthrough += [arg, args[index + 1]]
            index += 1
        elif arg.startswith("-"):
            passthrough.append(arg)
        elif target is None:
            target = arg
        else:
            passthrough.append(arg)
        index += 1
    return target or "release", dry_run, no_attest, passthrough


def git_out(ctx: Ctx, *args: str) -> str:
    result = ctx.run(["git", "-C", str(ctx.root), *args], text=True)
    if result.returncode != 0:
        raise Refusal(f"git {' '.join(args)} failed: {(result.stdout or '').strip()}")
    return (result.stdout or "").strip()


def mac_build(ctx: Ctx, branch: str, out: Path) -> str | None:
    """Run the remote builder; return None on success or the failure reason."""
    builder = ctx.remote_build
    if not (builder.is_file() and os.access(builder, os.X_OK)):
        return f"remote builder not found at {builder} (set THEGN_REMOTE_BUILD)"
    ctx.say(f"building {branch} on the remote Mac builder: {builder}")
    try:
        result = ctx.run_inherit([str(builder), branch, str(out)])
    except OSError as error:
        return f"could not start the remote builder: {error}"
    if result.returncode != 0:
        return f"remote builder exited {result.returncode}"
    if not out.is_file():
        return "remote builder reported success but produced no binary"
    return None


def confirm_local_build(ctx: Ctx, reason: str) -> bool:
    prompt = f"Mac build failed ({reason}). Build locally instead? This compiles on this machine. [y/N] "
    if not ctx.isatty():
        ctx.warn(prompt.strip())
        ctx.warn("non-interactive: refusing to compile locally. Fix the Mac builder, or rerun interactively.")
        return False
    return ctx.ask(prompt).strip().lower() in ("y", "yes")


def cmd_dev(ctx: Ctx, args: list[str]) -> int:
    target, dry_run, no_attest, passthrough = split_dev_args(args)
    if target == "release" or target.startswith("release="):
        want = target.partition("=")[2] or None
        tag = resolve_tag(ctx, want)
        ctx.say(f"dev <- release {tag}: download+verify, then scripts/dev_live.py --prebuilt")
        if dry_run:
            ctx.say("dry-run: " + " ".join(dev_live_argv(ctx, Path("<verified-binary>"), passthrough)))
            return 0
        with tempfile.TemporaryDirectory(prefix="thegn-update-") as scratch:
            binary = fetch_release_binary(ctx, tag, Path(scratch), no_attest)
            return ctx.run_inherit(dev_live_argv(ctx, binary, passthrough), cwd=str(ctx.root)).returncode

    current_branch = git_out(ctx, "rev-parse", "--abbrev-ref", "HEAD")
    if target == "checkout":
        if current_branch == "HEAD":
            raise Refusal("detached HEAD: check out a branch (or pass the branch name) first")
        if git_out(ctx, "status", "--porcelain", "--untracked-files=normal"):
            raise Refusal("the worktree has uncommitted changes; commit first so the build is of a real revision")
        branch = current_branch
    else:
        branch = "main" if target == "main" else target
    ctx.say(f"dev <- source build of branch {branch}: remote Mac builder first")
    if dry_run:
        ctx.say(f"dry-run: {ctx.remote_build} {branch} <tmp>/thegn, then "
                + " ".join(dev_live_argv(ctx, Path("<tmp>/thegn"), passthrough))
                + " (local `dev_live.py` build only after an explicit yes)")
        return 0
    with tempfile.TemporaryDirectory(prefix="thegn-update-") as scratch:
        out = Path(scratch) / "thegn"
        reason = mac_build(ctx, branch, out)
        if reason is None:
            version = binary_version(ctx, out)
            ctx.say(f"built binary reports thegn {version or '(unknown)'}")
            return ctx.run_inherit(dev_live_argv(ctx, out, passthrough), cwd=str(ctx.root)).returncode
        if not confirm_local_build(ctx, reason):
            return 1
    if branch != current_branch:
        raise Refusal(f"a local build compiles THIS checkout (branch {current_branch}), not {branch}; check out {branch} first")
    ctx.say("building locally via scripts/dev_live.py (release build, private target/live-cache)")
    return ctx.run_inherit(dev_live_argv(ctx, None, passthrough), cwd=str(ctx.root)).returncode


# ---------------------------------------------------------------- status
def cmd_status(ctx: Ctx) -> int:
    try:
        latest = resolve_tag(ctx, "latest")
    except Refusal as error:
        ctx.warn(f"latest release: unknown ({error})")
        latest = None
    latest_version = version_of_tag(latest) if latest else None
    ctx.say(f"latest release: {latest or 'unknown'}")
    rows = [("stable", ctx.stable_bin, None), ("dev", ctx.dev_bin, ctx.dev_record)]
    for name, binary, record in rows:
        version = binary_version(ctx, binary)
        if version is None:
            ctx.say(f"{name:7} not installed ({binary})")
            continue
        note = ""
        if latest_version:
            behind = parse_version(version) < parse_version(latest_version)
            note = "  BEHIND latest" if behind else "  up to date"
        extra = ""
        if record is not None and record.is_file() and record.stat().st_size < 65536:
            try:
                info = json.loads(record.read_text())
                extra = f"  (revision {str(info.get('revision') or '?')[:12]}, " \
                        f"{'prebuilt' if info.get('prebuilt') else 'local build'}, built {info.get('built_at', '?')})"
            except ValueError:
                pass
        ctx.say(f"{name:7} thegn {version}{note}{extra}")
    ctx.say("update with: just update stable | just update dev [release|main|checkout|<branch>]")
    return 0


# ---------------------------------------------------------------- entry
def main(argv: list[str] | None = None, ctx: Ctx | None = None) -> int:
    ctx = ctx or Ctx()
    args = [a for a in (sys.argv[1:] if argv is None else list(argv)) if a != ""]  # just passes "" for an unset target
    if not args or args == ["status"]:
        return cmd_status(ctx)
    command, rest = args[0], args[1:]
    if command == "stable":
        parser = argparse.ArgumentParser(prog="just update stable", description="Install a verified release as the stable binary.")
        parser.add_argument("version", nargs="?", default="latest", help="tag/version or 'latest' (default)")
        parser.add_argument("--dry-run", action="store_true")
        parser.add_argument("--force", action="store_true", help="allow downgrade / reinstall of the same version")
        parser.add_argument("--no-attest", action="store_true", help="skip gh attestation verification")
        parser.add_argument("--rollback", action="store_true", help="swap thegn.prev back in")
        options = parser.parse_args(rest)
        return cmd_stable(ctx, options.version, options.dry_run, options.force, options.no_attest, options.rollback)
    if command == "dev":
        return cmd_dev(ctx, rest)
    raise Refusal(f"unknown target {command!r}: use `just update` (status), `just update stable` or `just update dev`")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
    except (Refusal, OSError, subprocess.SubprocessError) as error:
        print(f"just update refused/failed: {error}", file=sys.stderr)
        sys.exit(1)
