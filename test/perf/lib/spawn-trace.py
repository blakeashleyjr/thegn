#!/usr/bin/env python3
"""Parse strace execve/process-lifecycle traces into a bounded idle-window report.

Input is what cpu-sample.sh records:

    strace -f -qq --seccomp-bpf -ttt -s 4096 -e signal=none \\
           -e trace=execve,clone,clone3,fork,vfork -o FILE ...

but the parser is deliberately tolerant of everything real strace emits even
when those flags drift: truncated strings (`"..."...`), signal and exit lines
(`--- SIGCHLD ... ---`, `+++ exited with 0 +++`), and `<unfinished ...>` /
`<... NAME resumed>` pairs (matched by pid), which `-f` produces whenever two
tasks are inside a traced syscall at once. A line that cannot be classified as
one of those is fatal (fail closed) -- an unparsable trace must never read as
"no spawns".

What counts as ONE spawn
  * A successful execve is one spawn.
  * A FAILED execve (`= -1 ENOENT` ...) is what a PATH search produces once per
    PATH entry for a single logical spawn. Failures from one pid with the same
    argv as a later success on that pid collapse into that success. Failures
    that never succeed (a command not found anywhere) count ONCE per
    (pid, argv). A failed attempt still cost a fork, so it is not ignored.

What is NOT thegn's spawn
  * `-f` also follows pane shells (the daemon's panes, or in-process panes under
    THEGN_NO_DAEMON) and whatever their rc files and prompt hooks exec. thegn
    spawns a pane shell from `$SHELL` verbatim (panes.rs `pane_shell_argv`), and
    the harness points `$SHELL` at a uniquely named symlink (`--pane-shell`).
    Using the clone/fork/vfork results we build a pid->parent map; an exec whose
    argv[0] is EXACTLY that marker path marks the root of a pane subtree, and
    every exec in the subtree is reported separately as `excluded` (informational,
    never counted). Nothing else is ever excluded -- argv shape (`bash --version`,
    `sh -s`, `sh script.sh`) proves nothing -- and with no `--pane-shell` given
    nothing is excluded at all. Residual limitation: a pane started as a
    non-shell command is counted (it is still a thegn spawn, and idle thegn does
    not start panes).
  * Per-pid exec state is reset when clone/fork/vfork hands out that pid, so pid
    reuse cannot inherit an earlier process's history.
"""

import json
import re
import sys
from collections import Counter
from pathlib import Path


# The pid prefix is MANDATORY (`-f` always prints it). Were it optional, a line
# `1790000000.5 execve(...)` would backtrack into pid `179000000`, time `0.5`.
LINE = re.compile(r"^(?:\[pid\s+(?P<bpid>\d+)\]|(?P<pid>\d+))\s+(?P<time>\d+\.\d+)\s+(?P<body>.*)$")
RESUMED = re.compile(r"^<\.\.\. (?P<name>\w+|\?\?\?) resumed>(?P<rest>.*)$")
CALL = re.compile(r"^(?P<name>\w+|\?\?\?)\((?P<args>.*)$")
RESULT = re.compile(r"\)\s*=\s*(-?\d+|\?)")
UNFINISHED = " <unfinished ...>"
SPAWN_CALLS = {"clone", "clone3", "fork", "vfork"}
UNREADABLE = "<unreadable execve"


class TraceError(ValueError):
    pass


def c_string(source: str, start: int) -> tuple[str, int]:
    """Parse one strace-quoted string; a trailing `...` marks truncation (accepted)."""
    if start >= len(source) or source[start] != '"':
        raise TraceError("expected quoted string")
    result = bytearray()
    i = start + 1
    while i < len(source):
        char = source[i]
        i += 1
        if char == '"':
            if source.startswith("...", i):
                i += 3
            return result.decode("utf-8", errors="replace"), i
        if char != "\\":
            result += char.encode("utf-8")
            continue
        if i >= len(source):
            break
        escaped = source[i]
        i += 1
        simple = {"n": 10, "r": 13, "t": 9, "b": 8, "f": 12, "v": 11, "\\": 92, '"': 34}
        if escaped in simple:
            result.append(simple[escaped])
        elif escaped == "x":
            digits = source[i : i + 2]
            if len(digits) != 2 or not re.fullmatch(r"[0-9a-fA-F]{2}", digits):
                raise TraceError("malformed hex escape")
            result.append(int(digits, 16))
            i += 2
        elif escaped in "01234567":
            digits = escaped
            while i < len(source) and len(digits) < 3 and source[i] in "01234567":
                digits += source[i]
                i += 1
            result.append(int(digits, 8) & 0xFF)
        else:
            # strace quotes unrecognized bytes as backslash plus byte.
            result += escaped.encode("utf-8")
    raise TraceError("unterminated quoted string")


def argv_from_args(call: str) -> list[str]:
    """argv from the text after `execve(` (may or may not include the result)."""
    if not call.startswith('"'):
        # strace could not read the tracee's memory (`execve(0x40c148, 0x7ff...,
        # 0x7ff...) = 0`, seen from a child racing its vfork parent). The exec
        # happened; its argv is unknowable. Count it, labelled, never drop it.
        return [UNREADABLE + f" {call.split(',')[0].strip()}>"]
    _, after_path = c_string(call, 0)
    args_start = call.find("[", after_path)
    if args_start < 0:
        raise TraceError("execve argv array is missing")
    values = []
    i = args_start + 1
    closed = False
    while i < len(call):
        while i < len(call) and (call[i].isspace() or call[i] == ","):
            i += 1
        if i >= len(call):
            break
        if call[i] == "]":
            closed = True
            break
        if call.startswith("...", i):
            # strace elides argv elements past its array limit with a bare `...`.
            i += 3
            continue
        value, i = c_string(call, i)
        values.append(value)
    if not closed:
        raise TraceError("unterminated execve argv array")
    if not values:
        raise TraceError("execve argv array is empty")
    return values


def result_of(text: str) -> str:
    found = RESULT.findall(text)
    if not found:
        raise TraceError("syscall result is missing")
    return found[-1]


def read_events(path: Path) -> list[tuple[float, int, str, str, str]]:
    """-> [(timestamp, pid, syscall, args_text, result)] with unfinished/resumed joined."""
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        raise TraceError(f"cannot read trace {path}: {error}") from error
    if not any(line.strip() for line in lines):
        raise TraceError(f"trace is empty: {path}")
    events = []
    pending: dict[int, tuple[float, str, str]] = {}
    for number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        match = LINE.match(line)
        try:
            if not match:
                if line.startswith("strace:"):
                    continue  # tracer diagnostics ("Process N attached")
                raise TraceError("unrecognised line")
            pid = int(match.group("pid") or match.group("bpid"))
            stamp = float(match.group("time"))
            text = match.group("body")
            if text.startswith(("---", "+++")) or text.endswith("<detached ...>"):
                # signal delivery, exit notices, and `???( <detached ...>` (tracer
                # let go of a task at shutdown) -- none is a syscall.
                continue
            resumed = RESUMED.match(text)
            if resumed:
                if pid not in pending or pending[pid][1] != resumed.group("name"):
                    raise TraceError("resumed syscall without matching unfinished")
                started, name, args = pending.pop(pid)
                events.append((started, pid, name, args, result_of(resumed.group("rest"))))
                continue
            call = CALL.match(text)
            if not call:
                raise TraceError("unrecognised line")
            if text.endswith(UNFINISHED):
                pending[pid] = (stamp, call.group("name"), call.group("args")[: -len(UNFINISHED)])
                continue
            events.append((stamp, pid, call.group("name"), call.group("args"), result_of(text)))
        except TraceError as error:
            raise TraceError(f"{path} line {number}: {error}") from error
    for pid, (started, name, args) in pending.items():
        # The tracer was stopped mid-syscall. An exec that had started is still an
        # exec attempt; count it (result unknown) rather than lose it.
        events.append((started, pid, name, args, "?"))
    return events


def collapse_execs(events, start: float = float("-inf"), end: float = float("inf")) -> list[tuple[float, int, list[str]]]:
    """One entry per logical spawn: see the module docstring.

    Per-pid exec state is cleared whenever a clone/fork/vfork returns that pid
    as a child, so a reused pid never inherits its predecessor's history."""
    spawns = []
    failed: dict[int, dict[tuple, float]] = {}
    execed: dict[int, float] = {}  # pid -> stamp of its latest READABLE successful exec
    for stamp, pid, name, args, result in sorted(events, key=lambda event: event[0]):
        if name in SPAWN_CALLS:
            if result.isdigit() and int(result) > 0:
                child = int(result)
                execed.pop(child, None)
                spawns.extend((first, child, list(key)) for key, first in failed.pop(child, {}).items())
            continue
        if name != "execve":
            continue
        try:
            argv = argv_from_args(args)
        except TraceError as error:
            raise TraceError(f"pid {pid} execve at {stamp}: {error}") from error
        key = tuple(argv)
        if result.startswith("-"):
            failed.setdefault(pid, {}).setdefault(key, stamp)
            continue
        unreadable = argv[0].startswith(UNREADABLE)
        if unreadable and pid in execed and start <= execed[pid] <= end:
            # A pid's second exec with unreadable args is the setuid/wrapper
            # hand-off of the SAME spawn (`/run/wrappers/bin/sudo` re-exec'ing the
            # real binary); that spawn was already counted by its first exec --
            # but only when that first exec is itself inside the window. Anything
            # else (earlier exec out of window, reused pid) is a new spawn.
            continue
        if not unreadable:
            execed[pid] = stamp
        pending = failed.pop(pid, {})
        pending.pop(key, None)  # PATH-search misses of this very spawn
        spawns.extend((first, pid, list(other)) for other, first in pending.items())
        spawns.append((stamp, pid, argv))
    for pid, pending in failed.items():
        spawns.extend((first, pid, list(key)) for key, first in pending.items())
    spawns.sort(key=lambda spawn: spawn[0])
    return spawns


def label_of(argv: list[str]) -> str:
    command = Path(argv[0]).name or argv[0]
    return " ".join([command, *argv[1:4]])


# Per-run fixture paths -> stable tokens, so the same command run against
# different worktrees groups together under --full-argv. Longest/most specific
# first. `wt-N` also names the fixture's branches, hence the bare-name rule.
NORMALISERS = [
    (re.compile(r"/[^\s\"']*?/tg-perf\.\w+/worktrees/wt-\d+"), "<wt>"),
    (re.compile(r"/[^\s\"']*?/tg-perf\.\w+/repo\b"), "<repo>"),
    (re.compile(r"/[^\s\"']*?/tg-perf\.\w+/origin\.git\b"), "<origin>"),
    (re.compile(r"/[^\s\"']*?/tg-perf\.\w+"), "<tmp>"),
    (re.compile(r"\bwt-\d+\b"), "wt-N"),
]


def normalise(text: str) -> str:
    for pattern, token in NORMALISERS:
        text = pattern.sub(token, text)
    return text


def full_label(argv: list[str]) -> str:
    return normalise(" ".join([Path(argv[0]).name or argv[0], *argv[1:]]))


def parse_traces(
    paths: list[Path], start: float, end: float, pane_shell: str | None = None, full_argv: bool = False
) -> dict:
    counted = Counter()
    excluded = Counter()
    groups: dict[str, dict] = {}
    total_execs = 0
    for path in paths:
        events = read_events(path)
        parent = {}
        tgid = {}  # tid -> thread-group leader (strace -f shows TIDs, not TGIDs)
        for _, pid, name, call_args, result in sorted(events, key=lambda event: event[0]):
            if name in SPAWN_CALLS and result.isdigit() and int(result) > 0:
                parent[int(result)] = pid
                tgid[int(result)] = tgid.get(pid, pid) if "CLONE_THREAD" in call_args else int(result)
        proc_argv: dict[int, str] = {}  # process -> argv[0] basename of its latest exec
        spawns = collapse_execs(events, start, end)
        if not spawns:
            raise TraceError(f"trace contains no execve events: {path}")
        total_execs += len(spawns)
        panes: set[int] = set()

        def in_pane(pid: int) -> bool:
            seen = set()
            while pid and pid not in seen:
                if pid in panes:
                    return True
                seen.add(pid)
                pid = parent.get(pid, 0)
            return False

        for stamp, pid, argv in spawns:
            skip = in_pane(pid)
            if not skip and pane_shell is not None and argv[0] == pane_shell and parent.get(pid):
                panes.add(pid)
                skip = True
            if start <= stamp <= end:
                (excluded if skip else counted)[label_of(argv)] += 1
                if full_argv:
                    forker = parent.get(pid, 0)
                    owner = tgid.get(forker, forker)
                    group = groups.setdefault(
                        ("pane: " if skip else "") + full_label(argv),
                        {"count": 0, "parents": Counter(), "threads": Counter()},
                    )
                    group["count"] += 1
                    group["parents"][proc_argv.get(owner, "?")] += 1
                    group["threads"][forker] += 1
            if not argv[0].startswith(UNREADABLE):
                proc_argv[tgid.get(pid, pid)] = Path(argv[0]).name or argv[0]
    if total_execs == 0:
        raise TraceError("trace contains no execve events")

    def listing(counter: Counter) -> list[dict]:
        return [
            {"command": command, "count": count}
            for command, count in sorted(counter.items(), key=lambda item: (-item[1], item[0]))
        ]

    if full_argv:
        return {
            "status": "measured",
            "method": "strace",
            "count": sum(counted.values()),
            "groups": [
                {
                    "count": group["count"],
                    "argv": label,
                    "parents": dict(group["parents"].most_common()),
                    "forking_tids": dict(group["threads"].most_common()),
                }
                for label, group in sorted(groups.items(), key=lambda item: (-item[1]["count"], item[0]))
            ],
        }
    return {
        "status": "measured",
        "method": "strace",
        "count": sum(counted.values()),
        "ceiling": 0,
        "offenders": listing(counted),
        "excluded": listing(excluded),
    }


FIXTURES = Path(__file__).resolve().parent / "fixtures"


def self_test() -> None:
    import tempfile

    cases = 0

    def expect_fatal(directory: Path, text: str) -> None:
        trace = directory / "bad"
        trace.write_text(text, encoding="utf-8")
        try:
            parse_traces([trace], 0.0, 1e12)
        except TraceError:
            return
        raise AssertionError(f"malformed trace accepted: {text!r}")

    # --- REAL strace captures (committed under fixtures/) -------------------
    # real-noflags.execve: strace WITHOUT -s / signal=none, i.e. what the first
    # draft of the gate recorded: truncated strings, SIGCHLD lines, a never-found
    # binary (failed execve), and the long-path bash trampoline.
    noflags = FIXTURES / "real-noflags.execve"
    result = parse_traces([noflags], 0.0, 1e12)
    labels = {o["command"]: o["count"] for o in result["offenders"]}
    assert "SIGCHLD" in noflags.read_text() and '"...' in noflags.read_text()
    assert result["count"] == 5, result
    assert labels["sleep 0.2"] == 1 and labels["sleep 0.1"] == 1, labels
    assert labels["binary"] == 1, labels  # /no/such/binary: failed execve, counted once
    assert result["excluded"] == []
    cases += 1

    # real-concurrent.execve: -f -s 4096 -e signal=none -e trace=execve,clone,
    # clone3,fork,vfork on a threaded python parent running concurrent subprocess
    # calls (unfinished/resumed pairs, vfork, PATH-search ENOENT bursts, quoted /
    # escaped / utf-8 argv) plus an interactive-style `bash --norc` "pane" that
    # execs `sleep`, plus `sh -c "podman ps"` (a thegn-style helper, must count).
    concurrent = FIXTURES / "real-concurrent.execve"
    text = concurrent.read_text()
    assert "<unfinished ...>" in text and "resumed>" in text and "ENOENT" in text
    first = float(text.split()[1])
    result = parse_traces([concurrent], first + 0.001, 1e12, pane_shell="bash")  # skip the root's own exec
    counted = {o["command"]: o["count"] for o in result["offenders"]}
    argv4 = 'a "quoted" \\ value\nnewline é'
    assert argv_from_args(
        '"/x/podman", ["podman", "ps", "--format", "a \\"quoted\\" \\\\ value\\nnewline \\303\\251"], 0x1) = 0'
    )[3] == 'a "quoted" \\ value\nnewline é'
    assert counted["podman ps --format " + argv4] == 4, counted  # 4 threads x 3 PATH entries -> 4 spawns
    assert counted["no-such-binary-anywhere x"] == 1, counted  # failed 5x, counted once
    assert counted["sh -c podman ps"] == 1 and counted["podman ps"] == 1, counted
    assert result["count"] == 7, result
    pane = {o["command"]: o["count"] for o in result["excluded"]}
    assert pane == {"bash --norc": 1, "sleep 0.01": 1}, pane  # pane subtree: informational only
    cases += 1

    with tempfile.TemporaryDirectory() as tmp:
        directory = Path(tmp)
        # --- synthetic edge cases ------------------------------------------
        trace = directory / "t"
        trace.write_text(
            '100 10.000000 execve("/usr/bin/true", ["true"], 0x1) = 0\n'
            '100 20.000000 execve("/usr/bin/podman", ["podman", "ps", "--format", "json"], 0x1) = 0\n'
            '101 20.500000 execve("/bin/podman", ["podman", "ps", "--format", "json"], 0x1) = 0\n'
            '102 20.750000 execve("/bin/docker", ["docker", "ps"], 0x1) = 0\n'
            '103 31.000000 execve("/usr/bin/late", ["late"], 0x1) = 0\n',
            encoding="utf-8",
        )
        other = directory / "o"
        other.write_text(
            '201 20.600000 execve("/usr/bin/podman", ["podman", "ps", "--format", "json"], 0x1) = 0\n',
            encoding="utf-8",
        )
        result = parse_traces([trace, other], 20.0, 30.0)
        assert result["count"] == 4 and result["offenders"][0] == {"command": "podman ps --format json", "count": 3}
        cases += 1
        # array elision and a tracer left mid-exec (counted, not lost)
        trace.write_text(
            '5 1.000000 execve("/bin/a", ["a", "b", ...], 0x1) = 0\n'
            '6 2.000000 execve("/bin/b", ["b"], 0x1 <unfinished ...>\n',
            encoding="utf-8",
        )
        assert parse_traces([trace], 0.0, 9.0)["count"] == 2
        cases += 1
        # signal/exit/diagnostic lines are ignored
        trace.write_text(
            '5 1.000000 --- SIGCHLD {si_signo=SIGCHLD} ---\n'
            '5 1.100000 execve("/bin/a", ["a"], 0x1) = 0\n'
            '5 1.200000 +++ exited with 0 +++\nstrace: Process 7 attached\n'
            '5 1.300000 ???( <detached ...>\n'
            '5 1.400000 ???( <unfinished ...>\n'
            '5 1.500000 <... ??? resumed>) = ?\n',
            encoding="utf-8",
        )
        assert parse_traces([trace], 0.0, 9.0)["count"] == 1
        cases += 1
        # unreadable args (real line from a soak trace) still count
        trace.write_text('7 3.0 execve(0x40c148, 0x7ffffffe2518, 0x7ffffffe2540) = 0\n', encoding="utf-8")
        assert parse_traces([trace], 0.0, 9.0)["offenders"][0]["command"] == "<unreadable execve 0x40c148>"
        cases += 1
        # ...unless it is the same pid's wrapper hand-off after a readable exec
        trace.write_text(
            '7 3.0 execve("/run/wrappers/bin/sudo", ["sudo", "-n", "podman"], 0x1) = 0\n'
            '7 3.1 execve(0x40c148, 0x7ffffffe2518, 0x7ffffffe2540) = 0\n',
            encoding="utf-8",
        )
        assert parse_traces([trace], 0.0, 9.0)["count"] == 1
        cases += 1
        # readable exec BEFORE the window + unreadable exec inside it: counted
        trace.write_text(
            '7 1.0 execve("/bin/helper", ["helper"], 0x1) = 0\n'
            '7 25.0 execve(0x40c148, 0x7ffe, 0x7ffe) = 0\n',
            encoding="utf-8",
        )
        assert parse_traces([trace], 20.0, 30.0)["count"] == 1
        cases += 1
        # pid reuse: first exec of the reused pid is unreadable -> counted
        trace.write_text(
            '100 1.0 clone(child_stack=NULL) = 7\n'
            '7 21.0 execve("/bin/helper", ["helper"], 0x1) = 0\n'
            '100 25.0 vfork() = 7\n'
            '7 25.5 execve(0x40c148, 0x7ffe, 0x7ffe) = 0\n',
            encoding="utf-8",
        )
        assert parse_traces([trace], 20.0, 30.0)["count"] == 2
        cases += 1
        # unprefixed line (no pid) is fatal, not mis-timed
        expect_fatal(directory, '1790000000.500000 execve("/bin/git", ["git", "status"], 0x1) = 0\n')
        cases += 1
        # shell-shaped execs are NOT pane roots; only the marker path is
        marker = "/tmp/x/bin/thegn-perf-pane-shell"
        thegn = '100 1.0 execve("/bin/thegn", ["thegn"], 0x1) = 0\n'
        for name, body in (
            ("bash --version", ["bash", "--version"]),
            ("sh script.sh", ["sh", "/path/script.sh"]),
        ):
            trace.write_text(
                thegn + '100 5.0 clone(child_stack=NULL) = 101\n'
                f'101 5.1 execve("/bin/{body[0]}", {json.dumps(body)}, 0x1) = 0\n',
                encoding="utf-8",
            )
            result = parse_traces([trace], 2.0, 9.0, pane_shell=marker)
            assert result["count"] == 1 and result["excluded"] == [], (name, result)
            cases += 1
        trace.write_text(
            thegn + '100 5.0 clone(child_stack=NULL) = 101\n'
            '101 5.1 execve("/bin/sh", ["sh", "-s"], 0x1) = 0\n'
            '101 5.2 clone(child_stack=NULL) = 102\n'
            '102 5.3 execve("/bin/podman", ["podman", "ps"], 0x1) = 0\n',
            encoding="utf-8",
        )
        result = parse_traces([trace], 2.0, 9.0, pane_shell=marker)
        assert result["count"] == 2 and result["excluded"] == [], result
        cases += 1
        trace.write_text(
            thegn + '100 5.0 clone(child_stack=NULL) = 101\n'
            f'101 5.1 execve("{marker}", ["{marker}"], 0x1) = 0\n'
            '101 5.2 clone(child_stack=NULL) = 102\n'
            '102 5.3 execve("/bin/podman", ["podman", "ps"], 0x1) = 0\n'
            '100 6.0 clone(child_stack=NULL) = 103\n'
            '103 6.1 execve("/bin/git", ["git", "status"], 0x1) = 0\n',
            encoding="utf-8",
        )
        result = parse_traces([trace], 2.0, 9.0, pane_shell=marker)
        assert result["count"] == 1 and result["offenders"][0]["command"] == "git status", result
        assert sum(o["count"] for o in result["excluded"]) == 2, result
        # with no marker given, nothing is ever excluded
        assert parse_traces([trace], 2.0, 9.0)["count"] == 3
        cases += 1
        # window boundary
        trace.write_text('90 19.999999 execve("/bin/true", ["true"], 0x1) = 0\n', encoding="utf-8")
        assert parse_traces([trace], 20.0, 30.0)["count"] == 0
        cases += 1
        # unclassifiable input fails closed
        for bad in ("", "not a strace event\n", '5 1.0 execve("/bin/a", ["a"\n',
                    '5 1.0 <... execve resumed>) = 0\n', '5 1.0 execve("/bin/a", ["a"], 0x1) =\n',
                    '5 1.0 --- SIGCHLD ---\n'):
            expect_fatal(directory, bad)
            cases += 1
    # --full-argv: complete argv, fixture paths normalised, parent = the exec'ing
    # PROCESS (threads resolved to their group leader via CLONE_THREAD).
    with tempfile.TemporaryDirectory() as tmp:
        trace = Path(tmp) / "f"
        trace.write_text(
            '100 1.0 execve("/bin/thegn", ["thegn"], 0x1) = 0\n'
            '100 2.0 clone(child_stack=NULL, flags=CLONE_VM|CLONE_THREAD|SIGCHLD) = 110\n'
            '110 3.0 clone(child_stack=NULL, flags=CLONE_VM|SIGCHLD) = 120\n'
            '120 3.1 execve("/usr/bin/git", ["git", "-C", "/t/tg-perf.AbC1/worktrees/wt-7", "rev-parse", "wt-7"], 0x1) = 0\n'
            '110 4.0 clone(child_stack=NULL, flags=CLONE_VM|SIGCHLD) = 121\n'
            '121 4.1 execve("/usr/bin/git", ["git", "-C", "/t/tg-perf.AbC1/worktrees/wt-9", "rev-parse", "wt-9"], 0x1) = 0\n'
            '120 5.0 clone(child_stack=NULL, flags=SIGCHLD) = 130\n'
            '130 5.1 execve("/usr/bin/git", ["git", "maintenance", "run", "--auto"], 0x1) = 0\n',
            encoding="utf-8",
        )
        full = parse_traces([trace], 2.5, 9.0, full_argv=True)
        assert full["count"] == 3, full
        top = full["groups"][0]
        assert top["argv"] == "git -C <wt> rev-parse wt-N" and top["count"] == 2, top
        assert top["parents"] == {"thegn": 2} and top["forking_tids"] == {110: 2}, top
        assert full["groups"][1]["parents"] == {"git": 1}, full
        cases += 1

    print(f"spawn-trace self-test: {cases} cases passed")


def main() -> int:
    args = sys.argv[1:]
    if args == ["--self-test"]:
        self_test()
        return 0
    pane_shell = None
    full_argv = False
    if args[:1] == ["--full-argv"]:
        full_argv, args = True, args[1:]
    if len(args) >= 2 and args[0] == "--pane-shell":
        pane_shell, args = args[1], args[2:]
    if len(args) < 3:
        print("usage: spawn-trace.py [--full-argv] [--pane-shell PATH] START_EPOCH END_EPOCH TRACE [TRACE ...]", file=sys.stderr)
        return 1
    try:
        result = parse_traces([Path(path) for path in args[2:]], float(args[0]), float(args[1]), pane_shell, full_argv)
    except (TraceError, ValueError) as error:
        print(f"spawn-rate trace invalid: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
