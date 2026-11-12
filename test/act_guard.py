#!/usr/bin/env python3
"""Bind credentialed local act runs to a clean HEAD and clean up failed runs.

THE-430: `just act` and `just act-job` pass through this guard so credentials
cannot reach modified workflow or local-action code, and disposable runs use
act's automatic container removal.
"""

from __future__ import annotations

import os
try:
    import pwd
except ImportError:  # pragma: no cover - pwd is Unix-only
    pwd = None
import re
import subprocess
import sys
import tempfile
from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


# act 0.2.89 `act --help`: bool shorthands pflag skips inside a cluster (-h is help),
# and the value-taking shorthands that swallow the rest of the arg.
_SHORT_BOOL = set("bghlnpqrvw")
_SHORT_VALUE = set("aCejPsW")
_REUSE_TRUE = {"1", "t", "T", "true", "TRUE", "True"}


# act long flags that take a value (act --help): the separate next arg is data, not a flag.
_LONG_VALUE = {
    "--container-options",
    "--container-architecture",
    "--env",
    "--env-file",
    "--input",
    "--var",
    "--job",
    "--matrix",
    "--platform",
    "--secret",
    "--secret-file",
    "--workflows",
    "--directory",
    "--local-repository",
    "--action-cache-path",
    "--artifact-server-path",
    "--event",
    "--eventpath",
}


def _scannable(args: list[str]) -> list[str]:
    """`args` minus the separate value token that follows a value-taking flag."""
    out: list[str] = []
    skip = False
    for arg in args:
        if skip:
            skip = False
            continue
        out.append(arg)
        if arg in _LONG_VALUE or (len(arg) == 2 and arg[0] == "-" and arg[1] in _SHORT_VALUE):
            skip = True
    return out


def _dangling_value_flag(args: list[str]) -> str | None:
    """The value-taking flag that ends `args` with no value after it, if any.

    act would consume whatever comes next (the appended `--rm`, or for `.actrc` the event
    name) as that flag's value, so the guard cannot tell what the run receives.
    """
    skip = False
    last = None
    for arg in args:
        if skip:
            skip = False
            last = None
            continue
        if arg in _LONG_VALUE or (len(arg) == 2 and arg[0] == "-" and arg[1] in _SHORT_VALUE):
            skip = True
            last = arg
        else:
            last = None
    return last if skip else None


def _short_cluster(arg: str) -> tuple[bool, str | None, bool]:
    """Scan one single-dash arg the way pflag does: (reuse, value flag letter, ambiguous).

    Bool shorthands are skipped (`r` records reuse), the first value shorthand takes the
    rest of the arg, and an unknown letter makes the arg ambiguous (callers fail closed).
    """
    if len(arg) < 2 or arg[0] != "-" or arg[1] == "-":
        return False, None, False
    reuse = False
    for i in range(1, len(arg)):
        c = arg[i]
        if c in _SHORT_BOOL:
            glued = arg[i + 1 : i + 2] == "="
            if c == "r":
                reuse = reuse or not glued or arg[i + 2 :] in _REUSE_TRUE
            if glued:
                return reuse, None, False
            continue
        if c in _SHORT_VALUE:
            return reuse, c, False
        return reuse, None, True
    return reuse, None, False


def _short_value_flag(arg: str) -> tuple[str, str] | None:
    """(letter, rest) for the first value shorthand reached after only bool letters, else None."""
    if len(arg) < 2 or arg[0] != "-" or arg[1] == "-":
        return None
    for i in range(1, len(arg)):
        c = arg[i]
        if c in _SHORT_BOOL:
            if arg[i + 1 : i + 2] == "=":
                return None
            continue
        if c in _SHORT_VALUE:
            rest = arg[i + 1 :]
            if rest.startswith("="):
                rest = rest[1:]
            return c, rest
        return None
    return None


def has_credentials(args: list[str]) -> bool:
    return any(
        arg.startswith("--secret") or _short_cluster(arg)[1] == "s" for arg in _scannable(args)
    )


def has_reuse(args: list[str]) -> bool:
    return any(
        arg == "--reuse"
        or (arg.startswith("--reuse=") and arg.partition("=")[2] in _REUSE_TRUE)
        or _short_cluster(arg)[0]
        for arg in _scannable(args)
    )


def ambiguous_short_args(args: list[str]) -> list[str]:
    """Single-dash args with a letter outside act's known short flags."""
    return [arg for arg in _scannable(args) if _short_cluster(arg)[2]]


_REDIRECT_LONG = (
    "--workflows",
    "--directory",
    "--local-repository",
    "--action-cache-path",
    "--action-offline-mode",
)


def redirect_flags(args: list[str]) -> list[str]:
    """Flags that point act at workflow/action code outside the checked `.github/` tree (in args order)."""
    found: list[str] = []
    for arg in _scannable(args):
        for flag in _REDIRECT_LONG:
            if arg == flag or arg.startswith(flag + "="):
                found.append(flag)
        letter = _short_cluster(arg)[1]
        if letter in {"W", "C"}:
            found.append("-" + letter)
    return found


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
        else:
            short = _short_value_flag(tok)
            if short and short[0] == "P":
                if short[1]:
                    value = short[1]
                else:
                    value = tokens[i + 1] if i + 1 < len(tokens) else ""
                    i += 1
        if value is not None:
            label, _, image = value.partition("=")
            out.append((label, image))
        i += 1
    return out


def _expand_actrc_vars(line: str, env: dict[str, str]) -> str:
    """Expand variables with the same forms and unset behavior as Go."""
    special = set("*#$@!?-0123456789")

    def is_ascii_alnum(char: str) -> bool:
        return (
            char == "_"
            or "0" <= char <= "9"
            or "a" <= char <= "z"
            or "A" <= char <= "Z"
        )

    expanded: list[str] = []
    i = 0
    while i < len(line):
        if line[i] != "$" or i + 1 == len(line):
            expanded.append(line[i])
            i += 1
            continue
        if line[i + 1] == "{":
            if i + 3 < len(line) and line[i + 2] in special and line[i + 3] == "}":
                name = line[i + 2]
                i += 4
                expanded.append(env.get(name, ""))
                continue
            end = line.find("}", i + 2)
            if end < 0:
                i += 2
                continue
            if end == i + 2:
                i = end + 1
                continue
            name = line[i + 2 : end]
            i = end + 1
        else:
            if line[i + 1] in special:
                name = line[i + 1]
                i += 2
            elif is_ascii_alnum(line[i + 1]):
                end = i + 2
                while end < len(line) and is_ascii_alnum(line[end]):
                    end += 1
                name = line[i + 1 : end]
                i = end
            else:
                expanded.append(line[i])
                i += 1
                continue
        expanded.append(env.get(name, ""))
    return "".join(expanded)


def actrc_tokens(actrc: str, env: dict[str, str] | None = None) -> list[str]:
    """Parse act's readArgsFile lines into one flag and one value per line."""
    tokens: list[str] = []
    for line in actrc.splitlines():
        # Mirrors act cmd/root.go readArgsFile: apply os.ExpandEnv, trim, keep
        # '-' lines, then split each line into at most two fields.
        expanded = _expand_actrc_vars(line, os.environ if env is None else env)
        stripped = expanded.strip()
        if not stripped.startswith("-"):
            continue
        tokens.extend(stripped.split(None, 1))
    return tokens


def user_actrc_paths(env: dict[str, str], platform_name: str = sys.platform) -> list[Path]:
    """Return act's XDG and home config paths, in its token-prepend order."""
    home = env.get("HOME")
    if not home and pwd is not None:
        try:
            home = pwd.getpwuid(os.getuid()).pw_dir
        except (KeyError, OSError):
            pass
    xdg_config = env.get("XDG_CONFIG_HOME")
    if not xdg_config and home:
        if platform_name == "darwin":
            xdg_config = str(Path(home) / "Library" / "Application Support")
        else:
            xdg_config = str(Path(home) / ".config")

    paths: list[Path] = []
    if xdg_config:
        paths.append(Path(xdg_config) / "act" / "actrc")
    if home:
        paths.append(Path(home) / ".actrc")
    return paths


def user_actrc_tokens(env: dict[str, str]) -> list[str]:
    """Read existing user actrc files, failing closed for non-ENOENT errors."""
    tokens: list[str] = []
    for path in user_actrc_paths(env):
        try:
            text = path.read_text()
        except FileNotFoundError:
            continue
        except (OSError, UnicodeError) as error:
            raise OSError(
                getattr(error, "errno", None),
                getattr(error, "strerror", None) or "cannot decode file",
                str(path),
            ) from error
        tokens.extend(actrc_tokens(text, env))
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
    actrc_args: list[str] | None = None,
) -> tuple[list[str] | None, str]:
    """Build guarded act argv, or return a refusal message.

    act prepends user and repository `.actrc` tokens to argv, so `actrc_args` count for
    the credential, reuse and redirect checks but are not added to the built command.
    The checks scan exactly what act parses, `[*actrc_args, event, *args]`: a dangling
    value flag at the end of the combined config consumes the next token.
    """
    effective = [*(actrc_args or []), event, *args]
    credentialed = has_credentials(effective) or implicit_secrets
    credential_source = " (including default .secrets)" if implicit_secrets else ""
    reuse = has_reuse(effective)

    if credentialed and reuse:
        return None, (
            f"act: --reuse cannot be combined with credentials{credential_source} "
            "(the retained container would carry them into later runs)"
        )
    dangling = _dangling_value_flag(effective)
    if credentialed and dangling:
        return None, (
            f"act: credentialed runs cannot end on {dangling} without a value{credential_source}: "
            "act would take the next token as its value"
        )
    ambiguous = ambiguous_short_args(effective)
    if credentialed and ambiguous:
        return None, (
            f"act: credentialed runs cannot use unrecognised short flags "
            f"{', '.join(ambiguous)}{credential_source}: cannot tell what they enable"
        )
    redirects = redirect_flags(effective)
    if credentialed and redirects:
        flags = ", ".join(sorted(set(redirects)))
        return None, (
            f"act: credentialed runs cannot use {flags}{credential_source}: "
            "the clean-HEAD check only covers .github/ and .actrc of this checkout"
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
    if not reuse and "--rm" not in _scannable(args):
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

    argv, message = plan("push", ["-sGITHUB_TOKEN=x"], [], "abc123")
    cases.append(
        (
            "credential-glued-short-flag",
            argv is not None and "abc123" in message,
        )
    )

    argv, message = plan(
        "push", ["-sGITHUB_TOKEN=x"], [" M .github/workflows/ci.yml"], "abc123"
    )
    cases.append(
        (
            "credential-glued-short-flag-dirty-refused",
            argv is None and ".github/workflows/ci.yml" in message,
        )
    )

    redirect_forms = [
        (["-W", "/tmp/w"], "-W"),
        (["-W/tmp/w"], "-W"),
        (["--workflows", "/tmp/w"], "--workflows"),
        (["--workflows=/tmp/w"], "--workflows"),
        (["-C", "/tmp/c"], "-C"),
        (["-C/tmp/c"], "-C"),
        (["--directory=/tmp/c"], "--directory"),
        (["--local-repository", "o/r@main=/tmp/r"], "--local-repository"),
    ]
    redirect_ok = True
    for form, flag in redirect_forms:
        argv, message = plan("push", ["--secret", "T=x", *form], [], "abc123")
        redirect_ok = redirect_ok and argv is None and flag in message
    cases.append(("credential-redirect-refused-in-all-forms", redirect_ok))

    argv, message = plan("push", ["-W", "/tmp/w"], [], "abc123", True)
    cases.append(
        (
            "implicit-secrets-redirect-refused",
            argv is None and ".secrets" in message,
        )
    )

    # positive control: also passes before this lane's fix
    cases.append(
        (
            "anonymous-redirect-allowed",
            plan("push", ["-W", "/tmp/w"], None, None)
            == (["act", "push", "-W", "/tmp/w", "--rm"], ""),
        )
    )

    reuse_ok = True
    for form in (["--reuse=1"], ["--reuse=TRUE"], ["-r=true"], ["-r=t"]):
        argv, _ = plan("push", form, None, None)
        reuse_ok = reuse_ok and argv is not None and "--rm" not in argv
        reuse_ok = reuse_ok and form[0] in argv
        argv, message = plan("push", ["--secret", "T=x", form[0]], [], "abc123")
        reuse_ok = reuse_ok and argv is None and "--reuse cannot" in message
    cases.append(("reuse-truthy-equals-forms", reuse_ok))

    # positive control: also passes before this lane's fix
    argv, _ = plan("push", ["--reuse=false"], None, None)
    cases.append(
        (
            "reuse-false-equals-keeps-rm",
            argv is not None and argv[-1] == "--rm",
        )
    )

    cases.append(
        (
            "credential-clustered-short-flag",
            all(has_credentials([f]) for f in ("-vsT=x", "-bsX=y", "-qsX"))
            and not has_credentials(["-vq", "-vr"]),
        )
    )

    argv, _ = plan("push", ["-vr"], None, None)
    argv2, message = plan("push", ["--secret", "T=x", "-vr"], [], "abc123")
    cases.append(
        (
            "reuse-clustered-short-flag",
            argv is not None and "--rm" not in argv
            and argv2 is None and "--reuse cannot" in message,
        )
    )

    clustered_ok = True
    for form, flag in ((["-vW/tmp/w"], "-W"), (["-bC/tmp/c"], "-C")):
        argv, message = plan("push", ["--secret", "T=x", *form], [], "abc123")
        clustered_ok = clustered_ok and argv is None and flag in message
    cases.append(("redirect-clustered-short-flag", clustered_ok))

    argv, message = plan("push", ["--secret", "T=x", "-vZ"], [], "abc123")
    argv2, _ = plan("push", ["-vZ"], None, None)
    cases.append(
        (
            "credentialed-unknown-cluster-letter-refused",
            argv is None and "unrecognised" in message and argv2 is not None,
        )
    )

    actrc_ok = True
    for tokens, needle in (
        (["-s", "GITHUB_TOKEN"], ""),
        (["--secret-file", "x"], ""),
    ):
        argv, message = plan("push", [], ["?? .actrc"], "abc123", False, tokens)
        actrc_ok = actrc_ok and argv is None and "uncommitted" in message
    argv, message = plan("push", [], [], "abc123", False, ["--reuse", "-s", "T=x"])
    actrc_ok = actrc_ok and argv is None and "--reuse cannot" in message
    argv, message = plan("push", ["-W", "/tmp/w"], [], "abc123", False, ["-s", "T=x"])
    actrc_ok = actrc_ok and argv is None and "-W" in message
    argv, _ = plan("push", [], None, None, False, ["-P", "ubuntu-latest=img@sha256:" + "0" * 64])
    actrc_ok = actrc_ok and argv == ["act", "push", "--rm"]
    cases.append(("actrc-tokens-feed-guard-checks", actrc_ok))

    with tempfile.TemporaryDirectory() as temp_dir:
        temp_root = Path(temp_dir)
        home = temp_root / "home"
        xdg = temp_root / "xdg"
        (xdg / "act").mkdir(parents=True)
        home.mkdir()
        user_env = {"HOME": str(home), "XDG_CONFIG_HOME": str(xdg)}
        baseline = plan("push", [], None, None)
        passwd_home = None
        if pwd is not None:
            try:
                passwd_home = Path(pwd.getpwuid(os.getuid()).pw_dir)
            except (KeyError, OSError):
                pass
        cases.append(
            (
                "user-actrc-absent-is-unchanged",
                user_actrc_paths(user_env)
                == [xdg / "act" / "actrc", home / ".actrc"]
                and user_actrc_paths({"HOME": str(home)})
                == [home / ".config" / "act" / "actrc", home / ".actrc"]
                and user_actrc_paths({})
                == (
                    [passwd_home / ".config" / "act" / "actrc", passwd_home / ".actrc"]
                    if passwd_home is not None
                    else []
                )
                and user_actrc_tokens(user_env) == []
                and plan("push", [], None, None) == baseline,
            )
        )
        darwin_home = temp_root / "darwin-home"
        cases.append(
            (
                "user-actrc-darwin-config-path",
                user_actrc_paths({"HOME": str(darwin_home)}, "darwin")
                == [
                    darwin_home / "Library" / "Application Support" / "act" / "actrc",
                    darwin_home / ".actrc",
                ],
            )
        )

        user_actrc = xdg / "act" / "actrc"
        user_actrc.write_text("--secret T=x\n")
        user_tokens = user_actrc_tokens(user_env)
        argv, message = plan(
            "push", [], [" M .github/workflows/ci.yml"], "abc123", False, user_tokens
        )
        cases.append(
            (
                "user-actrc-credential-dirty-refused",
                user_tokens == ["--secret", "T=x"]
                and argv is None
                and ".github/workflows/ci.yml" in message,
            )
        )

        user_actrc.write_text("--env A=1 --env\n-s T=x\n")
        two_line_tokens = user_actrc_tokens(user_env)
        argv, message = plan(
            "push", [], [" M .github/workflows/ci.yml"], "abc123", False, two_line_tokens
        )
        cases.append(
            (
                "user-actrc-matches-act-two-field-tokenizer",
                two_line_tokens == ["--env", "A=1 --env", "-s", "T=x"]
                and argv is None
                and ".github/workflows/ci.yml" in message,
            )
        )

        expanded_env = {**user_env, "ACT_SECRET_VALUE": "T=x"}
        user_actrc.write_text("--secret $ACT_SECRET_VALUE\n")
        cases.append(
            (
                "user-actrc-expands-environment-values",
                user_actrc_tokens(expanded_env) == ["--secret", "T=x"],
            )
        )

        flag_env = {**user_env, "ACT_SECRET_FLAG": "-s"}
        unset_tokens = actrc_tokens("$NOPE-s T=x\n", user_env)
        direct_tokens = actrc_tokens("-s T=x\n", user_env)
        set_tokens = actrc_tokens("$ACT_SECRET_FLAG T=x\n", flag_env)
        unset_argv, unset_message = plan(
            "push", [], [" M .github/workflows/ci.yml"], "abc123", False, unset_tokens
        )
        direct_argv, direct_message = plan(
            "push", [], [" M .github/workflows/ci.yml"], "abc123", False, direct_tokens
        )
        cases.append(
            (
                "actrc-expands-before-flag-check-with-unset-and-set-vars",
                unset_tokens == ["-s", "T=x"]
                and direct_tokens == ["-s", "T=x"]
                and set_tokens == ["-s", "T=x"]
                and unset_argv is None
                and direct_argv is None
                and ".github/workflows/ci.yml" in unset_message
                and ".github/workflows/ci.yml" in direct_message,
            )
        )

        user_actrc.write_text("-P ubuntu-latest=img/x:latest\n")
        executed: list[tuple[str, list[str]]] = []
        saved_env = os.environ.copy()
        saved_execvp = os.execvp
        try:
            os.environ.clear()
            os.environ.update(user_env)
            os.execvp = lambda program, command: executed.append((program, command))
            image_result = main(["push"])
        finally:
            os.execvp = saved_execvp
            os.environ.clear()
            os.environ.update(saved_env)
        cases.append(
            (
                "user-actrc-image-pin-is-not-checked",
                user_actrc_tokens(user_env) == ["-P", "ubuntu-latest=img/x:latest"]
                and image_result == 127
                and executed == [("act", ["act", "push", "--rm"])],
            )
        )

        user_actrc.unlink()
        home_actrc = home / ".actrc"
        home_actrc.mkdir()
        saved_env = os.environ.copy()
        unreadable_stderr = StringIO()
        try:
            os.environ.clear()
            os.environ.update(user_env)
            with redirect_stderr(unreadable_stderr):
                unreadable_result = main(["push"])
        finally:
            os.environ.clear()
            os.environ.update(saved_env)
        unreadable_message = unreadable_stderr.getvalue().strip()
        cases.append(
            (
                "unreadable-user-actrc-refused-by-path",
                unreadable_result == 1
                and unreadable_message
                == f"act: cannot read user actrc {home_actrc}; not running"
                and "TOKEN" not in unreadable_message,
            )
        )
        home_actrc.rmdir()

        user_actrc.write_text("--env\n")
        repo_tokens = ["--secret", "T=x"]
        combined_tokens = [*user_actrc_tokens(user_env), *repo_tokens]
        cases.append(
            (
                "user-actrc-tokens-precede-repo-actrc",
                combined_tokens == ["--env", "--secret", "T=x"]
                and not has_credentials([*combined_tokens, "push"]),
            )
        )

    argv, message = plan(
        "push", ["--secret", "T=x", "--container-options", "-v /a:/b"], [], "abc123"
    )
    argv2, _ = plan("push", ["--secret", "T=x", "--env", "-r"], [], "abc123")
    cases.append(
        (
            "long-flag-value-is-not-scanned-as-a-cluster",
            argv is not None and "abc123" in message and argv2 is not None,
        )
    )
    argv, _ = plan("push", ["--secret", "T=x", "-e", "-vr"], [], "abc123")
    cases.append(("separate-short-value-is-not-scanned", argv is not None))
    cases.append(
        (
            "credential-flag-still-detected-with-separate-value",
            has_credentials(["--secret", "T=x"])
            and has_credentials(["-s", "T"])
            and not has_credentials(["--env", "--secret-file"])
            and not has_credentials(["--container-options", "-s"]),
        )
    )
    argv, message = plan("push", ["--secret", "T=x"], [" M .actrc"], "abc123", False, ["--env"])
    cases.append(
        (
            "actrc-trailing-value-flag-does-not-hide-cli-credentials",
            argv is None and ".actrc" in message,
        )
    )
    argv, message = plan("push", ["--secret", "T=x", "--env"], [], "abc123")
    cases.append(
        (
            "credentialed-trailing-value-flag-refused",
            argv is None and "--env without a value" in message,
        )
    )
    argv, _ = plan("push", ["--env", "--rm"], None, None)
    cases.append(("value-rm-is-not-the-rm-flag", argv == ["act", "push", "--env", "--rm", "--rm"]))
    argv, message = plan("push", ["--secret", "T=x", "--env", "A=b", "-r"], [], "abc123")
    cases.append(
        (
            "real-flag-after-a-value-is-still-scanned",
            argv is None and "--reuse cannot" in message,
        )
    )
    pinned = "img/x@sha256:" + "a" * 64
    clustered_ok = all(
        "override is not pinned by digest" in (image_refusal(good, form) or "")
        for form in (
            ["-vPubuntu-latest=img/x:latest"],
            ["-vP", "ubuntu-latest=img/x:latest"],
            ["-P=ubuntu-latest=img/x:latest"],
        )
    )
    cases.append(
        (
            "platform-override-clustered-forms-refused",
            clustered_ok
            and platform_mappings(["-vPubuntu-latest=" + pinned]) == [("ubuntu-latest", pinned)],
        )
    )
    cache_ok = True
    for flag, form in (
        ("--action-cache-path", ["--action-cache-path", "/tmp/c"]),
        ("--action-cache-path", ["--action-cache-path=/tmp/c"]),
        ("--action-offline-mode", ["--action-offline-mode"]),
    ):
        argv, message = plan("push", ["--secret", "T=x", *form], [], "abc123")
        cache_ok = cache_ok and argv is None and flag in message
    anon = plan("push", ["--action-offline-mode"], None, None)
    cache_ok = cache_ok and anon == (["act", "push", "--action-offline-mode", "--rm"], "")
    cases.append(("action-cache-flags-refused-when-credentialed", cache_ok))

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
        user_actrc_args = user_actrc_tokens(dict(os.environ))
    except OSError as error:
        print(
            f"act: cannot read user actrc {error.filename}; not running",
            file=sys.stderr,
        )
        return 1
    try:
        actrc_text: str | None = (ROOT / ".actrc").read_text()
    except OSError:
        actrc_text = None
    refusal = image_refusal(actrc_text, args)
    if refusal:
        print(refusal, file=sys.stderr)
        return 1
    implicit_secrets = (ROOT / ".secrets").exists()
    actrc_args = [*user_actrc_args, *(actrc_tokens(actrc_text) if actrc_text else [])]
    if has_credentials([*actrc_args, event, *args]) or implicit_secrets:
        dirty = dirty_paths()
        head = head_sha()
    else:
        dirty = None
        head = None
    command, message = plan(event, args, dirty, head, implicit_secrets, actrc_args)
    if message:
        print(message, file=sys.stderr)
    if command is None:
        return 1
    os.execvp("act", command)
    return 127


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
