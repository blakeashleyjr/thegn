#!/usr/bin/env python3
"""Bind credentialed local act runs to a clean HEAD and clean up failed runs.

THE-430: `just act` and `just act-job` pass through this guard so credentials
cannot reach modified workflow or local-action code, and disposable runs use
act's automatic container removal.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def has_credentials(args: list[str]) -> bool:
    return any(
        arg.startswith("--secret") or arg == "-s" or arg.startswith("-s=")
        for arg in args
    )


def has_reuse(args: list[str]) -> bool:
    return any(arg in {"--reuse", "-r", "--reuse=true"} for arg in args)


_DIGEST = re.compile(r"@sha256:[0-9a-f]{64}$")


def platform_mappings(tokens: list[str]) -> list[tuple[str, str]]:
    """`label=image` pairs from `-P x=y`, `-Px=y`, `--platform x=y`, `--platform=x=y` (token list order)."""
    out: list[tuple[str, str]] = []
    i = 0
    while i < len(tokens):
        tok = tokens[i]
        value: str | None = None
        if tok in {"-P", "--platform"}:
            value = tokens[i + 1] if i + 1 < len(tokens) else ""
            i += 1
        elif tok.startswith("--platform="):
            value = tok[len("--platform="):]
        elif tok.startswith("-P") and len(tok) > 2:
            value = tok[2:]
        if value is not None:
            label, _, image = value.partition("=")
            out.append((label, image))
        i += 1
    return out


def actrc_tokens(actrc: str) -> list[str]:
    """Whitespace-split tokens of every non-blank, non-comment .actrc line."""
    tokens: list[str] = []
    for line in actrc.splitlines():
        stripped = line.strip()
        if stripped and not stripped.startswith("#"):
            tokens.extend(stripped.split())
    return tokens


def image_refusal(actrc: str | None, args: list[str]) -> str | None:
    """A refusal message when the effective runner image is not digest-pinned or an override changes the pinned one."""
    if actrc is None:
        return "act: .actrc is missing; the runner image must be pinned by digest there"
    pinned = dict(platform_mappings(actrc_tokens(actrc)))
    if not pinned:
        return "act: .actrc maps no runner image (-P label=image@sha256:...); not running"
    for label, image in pinned.items():
        if not _DIGEST.search(image):
            return f"act: .actrc runner image for {label} is not pinned by digest: {image}"
    for label, image in platform_mappings(args):
        if not _DIGEST.search(image):
            return f"act: the {label} runner image override is not pinned by digest: {image}"
        if label in pinned and pinned[label] != image:
            return (
                f"act: the {label} runner image override differs from the pin in .actrc "
                f"({pinned[label]}); change .actrc in a reviewed commit instead"
            )
    return None


def plan(
    event: str,
    args: list[str],
    dirty: list[str] | None,
    head: str | None,
    implicit_secrets: bool = False,
) -> tuple[list[str] | None, str]:
    """Build guarded act argv, or return a refusal message."""
    credentialed = has_credentials(args) or implicit_secrets
    credential_source = " (including default .secrets)" if implicit_secrets else ""
    reuse = has_reuse(args)

    if credentialed and reuse:
        return None, (
            f"act: --reuse cannot be combined with credentials{credential_source} "
            "(the retained container would carry them into later runs)"
        )
    if credentialed and (dirty is None or head is None):
        return None, (
            "act: credentialed runs need git to bind the run to HEAD; not running"
        )
    if credentialed and dirty:
        paths = ", ".join(dirty[:10])
        if len(dirty) > 10:
            paths += ", ..."
        return None, (
            f"act: credentialed runs require .github/ and .actrc to match HEAD {head}"
            f"{credential_source}; "
            f"uncommitted: {paths}. Commit or stash them, or run without credentials."
        )

    command = ["act", event, *args]
    if not reuse and "--rm" not in args:
        command.append("--rm")
    message = (
        f"act: credentialed run{credential_source} bound to HEAD {head}"
        if credentialed
        else ""
    )
    return command, message


def dirty_paths() -> list[str] | None:
    """Return changed or untracked workflow/act config paths; None on git failure."""
    try:
        result = subprocess.run(
            [
                "git",
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--",
                ".github",
                ".actrc",
            ],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, UnicodeError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return result.stdout.splitlines()


def head_sha() -> str | None:
    """Return the checked-out commit, or None when git cannot verify HEAD."""
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--verify", "HEAD"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, UnicodeError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    sha = result.stdout.strip()
    return sha or None


def self_test() -> int:
    """Check plan behavior without git, act, or a container engine."""

    def check(label: str, condition: bool) -> bool:
        if condition:
            return True
        print(f"SELF-TEST FAIL: {label}", file=sys.stderr)
        return False

    cases: list[tuple[str, bool]] = []

    argv, _ = plan("push", [], None, None)
    cases.append(("anonymous-adds-rm", argv == ["act", "push", "--rm"]))

    argv, _ = plan("push", ["--rm"], None, None)
    cases.append(("anonymous-keeps-existing-rm", argv == ["act", "push", "--rm"]))

    argv, _ = plan("push", ["--reuse"], None, None)
    cases.append(("anonymous-reuse-skips-rm", argv == ["act", "push", "--reuse"]))

    argv, message = plan("push", ["--secret-file", ".secrets"], [], "abc123")
    cases.append(
        (
            "credential-clean-binds-head",
            argv == ["act", "push", "--secret-file", ".secrets", "--rm"]
            and "abc123" in message,
        )
    )

    argv, message = plan("push", ["--secret-file=.secrets"], [], "abc123")
    cases.append(("credential-equals-form", argv is not None and "abc123" in message))

    argv, message = plan("push", ["-s", "GITHUB_TOKEN"], [], "abc123")
    cases.append(("credential-short-flag", argv is not None and "abc123" in message))

    argv, message = plan("push", ["--secret-file", ".secrets"], [" M .github/workflows/ci.yml"], "abc123")
    cases.append(
        (
            "credential-dirty-refused",
            argv is None
            and ".github/workflows/ci.yml" in message
            and "abc123" in message,
        )
    )

    argv, message = plan("push", ["--secret", "TOKEN=x"], ["?? .github/actions/evil/action.yml"], "abc123")
    cases.append(("credential-untracked-action-refused", argv is None and ".github/actions/evil/action.yml" in message))

    argv, message = plan("push", ["--secret", "TOKEN=x"], None, "abc123")
    cases.append(("credential-without-git-refused", argv is None and "need git" in message))

    argv, message = plan("push", ["--secret", "TOKEN=x", "-r"], [], "abc123")
    cases.append(("credential-with-reuse-refused", argv is None and "--reuse cannot" in message))

    argv, message = plan("push", ["--secret-file="], [], "abc123")
    cases.append(("blank-secret-file-value-still-credential", argv is not None and "abc123" in message))

    dirty_argv, dirty_message = plan(
        "push", [], [" M .github/workflows/ci.yml"], "abc123", True
    )
    reuse_argv, reuse_message = plan("push", ["--reuse"], [], "abc123", True)
    clean_argv, clean_message = plan("push", [], [], "abc123", True)
    cases.append(
        (
            "implicit-default-secrets-is-credential",
            dirty_argv is None
            and ".secrets" in dirty_message
            and ".github/workflows/ci.yml" in dirty_message
            and reuse_argv is None
            and ".secrets" in reuse_message
            and "--reuse cannot" in reuse_message
            and clean_argv == ["act", "push", "--rm"]
            and ".secrets" in clean_message
            and "abc123" in clean_message,
        )
    )

    argv, _ = plan("push", ["-j", "lint"], None, None)
    cases.append(("job-flag-passthrough", argv == ["act", "push", "-j", "lint", "--rm"]))

    good = "-P ubuntu-latest=img/x:t@sha256:" + "a" * 64 + "\n"
    cases.append(("image-pin-accepts-pinned-actrc", image_refusal(good, []) is None))
    cases.append(
        (
            "image-pin-ignores-comments-and-blank-lines",
            image_refusal("# c\n\n" + good, []) is None,
        )
    )
    cases.append(
        (
            "image-pin-refuses-tag-only-actrc",
            "not pinned by digest"
            in (image_refusal("-P ubuntu-latest=img/x:latest\n", []) or ""),
        )
    )
    cases.append(
        (
            "image-pin-refuses-truncated-digest",
            "not pinned by digest"
            in (
                image_refusal(
                    "-P ubuntu-latest=img/x:t@sha256:" + "a" * 63 + "\n", []
                )
                or ""
            ),
        )
    )
    cases.append(
        (
            "image-pin-refuses-missing-or-empty-actrc",
            "missing" in (image_refusal(None, []) or "")
            and "maps no runner image"
            in (image_refusal("--container-architecture linux/amd64\n", []) or ""),
        )
    )
    override_forms = [
        ["-P", "ubuntu-latest=img/x:latest"],
        ["-Pubuntu-latest=img/x:latest"],
        ["--platform", "ubuntu-latest=img/x:latest"],
        ["--platform=ubuntu-latest=img/x:latest"],
    ]
    cases.append(
        (
            "image-pin-refuses-tag-override-in-all-forms",
            all(
                "override is not pinned by digest" in (image_refusal(good, form) or "")
                for form in override_forms
            ),
        )
    )
    cases.append(
        (
            "image-pin-refuses-a-different-digest-for-a-pinned-label",
            "differs from the pin"
            in (
                image_refusal(
                    good, ["-P", "ubuntu-latest=img/y:t@sha256:" + "b" * 64]
                )
                or ""
            ),
        )
    )
    cases.append(
        (
            "image-pin-allows-the-identical-override-and-a-new-pinned-label",
            image_refusal(good, ["-P", "ubuntu-latest=img/x:t@sha256:" + "a" * 64])
            is None
            and image_refusal(good, ["-P", "ubuntu-22.04=img/z:t@sha256:" + "c" * 64])
            is None,
        )
    )
    cases.append(
        (
            "image-pin-ignores-non-platform-flags",
            image_refusal(good, ["-j", "lint", "--rm", "-W", "x"]) is None,
        )
    )

    for label, passed in cases:
        if not check(label, passed):
            return 1
    print(f"OK: {len(cases)} act guard self-tests passed")
    return 0


def main(argv: list[str]) -> int:
    if argv and argv[0] == "--self-test":
        return self_test()
    if not argv:
        print("usage: act_guard.py EVENT [ACT_ARGS...]", file=sys.stderr)
        return 2

    event, *args = argv
    try:
        actrc_text: str | None = (ROOT / ".actrc").read_text()
    except OSError:
        actrc_text = None
    refusal = image_refusal(actrc_text, args)
    if refusal:
        print(refusal, file=sys.stderr)
        return 1
    implicit_secrets = (ROOT / ".secrets").exists()
    if has_credentials(args) or implicit_secrets:
        dirty = dirty_paths()
        head = head_sha()
    else:
        dirty = None
        head = None
    command, message = plan(event, args, dirty, head, implicit_secrets)
    if message:
        print(message, file=sys.stderr)
    if command is None:
        return 1
    os.execvp("act", command)
    return 127


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
