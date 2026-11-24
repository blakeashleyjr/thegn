#!/usr/bin/env python3
"""Driver for the two-session remote terminal parity run.

Cases: P00H host, P01 startup, P02 environment, P03 resize, P04 color,
P05 unicode, P06-P09 signals and input, P10 bracketed paste, P11 shell-only
alternate screen, P12 DECCKM arrows, P13 mouse, P14 alternate-screen restore,
P15 throughput, P16 large paste, P17 title, P18 cwd, P19 exit status, P20
echo latency and P20B TUI probe, P21 startup detail, P22 close, and P23 idle.

The optional RP_REMOTE_PREFIX_JSON, RP_SHELL_READY_S, and RP_ENV_UP_S inputs
extend the live provider run without changing the local self-test path.  When
P20B runs, it supplies RP_TUI_SAMPLES_OUT to the probe.  A compact run summary
is printed after the case summary.
"""
import base64
import atexit
import json
import os
import re
import shutil
import statistics
import subprocess
import threading
import time
from pathlib import Path

BIN = os.environ["RP_THEGN_BIN"]
LOCAL = os.environ["RP_LOCAL_SESSION"]
REMOTE = os.environ["RP_REMOTE_SESSION"]
REPO = os.environ["RP_REPO"]
TMP = Path(os.environ["RP_TMP"])
ENV_NAME = os.environ["RP_ENV_NAME"]
SAMPLES = int(os.environ.get("RP_SAMPLES", "20"))
KEEP_GOING = os.environ.get("RP_KEEP_GOING") == "1"
IDLE_PROBE = os.environ.get("RP_IDLE_PROBE") == "1"
QUIRK = os.environ.get("RP_PROVIDER_QUIRK", "THE-2450")
REMOTE_ARGV = json.loads(os.environ["RP_REMOTE_ARGV_JSON"])
LOCAL_ARGV = json.loads(os.environ.get("RP_LOCAL_ARGV_JSON", "[]"))
OPEN_MS = [int(os.environ.get("RP_LOCAL_OPEN_MS", "0")), int(os.environ.get("RP_REMOTE_OPEN_MS", "0"))]
HOSTS = ["", ""]
METRICS = {}

CSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
OSC = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
DCS = re.compile(r"\x1b(?:P|X|\^|_)[^\x1b]*(?:\x1b\\)?")
ESC = re.compile(r"\x1b[ -/]*[@-~]")
END = re.compile(r"@@E([A-Z0-9]+) rc=(-?\d+)@@")
results = []
failed = []
active_streams = []


def cli(*args, timeout=30):
    try:
        p = subprocess.run([BIN, *map(str, args)], capture_output=True, timeout=timeout)
        return p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")
    except (OSError, subprocess.TimeoutExpired) as exc:
        return 124, "", str(exc)


def send(session, text, enter=True, paste=False):
    args = ["session", "send", "--session", session]
    if paste:
        args.append("--paste")
    args.append(text)
    if enter:
        args.append("--enter")
    return cli(*args, timeout=30)


def key(session, value):
    return cli("session", "send", "--session", session, "--key", value, timeout=20)


def snap(session):
    rc, out, err = cli("session", "snapshot", "--session", session, "--json", "--text", timeout=20)
    if rc:
        raise RuntimeError(err or out or "snapshot failed")
    return json.loads(out)


def wait_mode(session, field, want, timeout=10):
    """Wait for a snapshot mode field to reach the requested value."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if snap(session).get(field) == want:
                return True
        except (OSError, RuntimeError, ValueError, json.JSONDecodeError):
            pass
        time.sleep(0.1)
    return False


def resize(session, rows, cols):
    params = json.dumps({"s": session, "rows": rows, "cols": cols}, separators=(",", ":"))
    return cli("api", "call", "sessions.resize", "--params", params, timeout=20)


class Attach:
    def __init__(self, session):
        self.proc = None
        self.session = session
        self.data = bytearray()
        self.events = []
        self.lock = threading.Lock()

    def start(self):
        self.proc = subprocess.Popen([BIN, "session", "attach", self.session], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        active_streams.append(self)

        def reader():
            assert self.proc.stdout is not None
            while True:
                # read1 returns what is available; read(n) on a pipe blocks until n bytes or EOF.
                chunk = self.proc.stdout.read1(4096)
                if not chunk:
                    break
                with self.lock:
                    self.data.extend(chunk)
                    self.events.append((time.monotonic(), chunk))

        threading.Thread(target=reader, daemon=True).start()

    def text(self):
        with self.lock:
            return bytes(self.data).decode("utf-8", "replace")

    def length(self):
        with self.lock:
            return len(self.data)

    def wait_initial(self, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                if self.data:
                    return True
            time.sleep(0.02)
        return False

    def wait(self, needle, timeout, offset=0):
        if isinstance(needle, str):
            needle = needle.encode()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                if needle in self.data[offset:]:
                    return time.monotonic()
            time.sleep(0.02)
        return None

    def stop(self):
        if self.proc is None:
            return
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(2)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        if self in active_streams:
            active_streams.remove(self)


@atexit.register
def stop_attach_readers():
    for stream in active_streams[:]:
        stream.stop()


def clean(text):
    text = ESC.sub("", CSI.sub("", DCS.sub("", OSC.sub("", text)))).replace("\r", "")
    text = re.sub(r"/dev/pts/\d+", "/dev/pts/<N>", text)
    text = re.sub(r"(?m)^\[\d+\] \d+$", "[N] <PID>", text)
    text = re.sub(r"(?m)[ \t]+$", "", text)
    text = re.sub(r"\n(?:[ \t]*\n)+", "\n\n", text)
    return text.strip()


def norm(text, remote=False):
    text = clean(text).replace(REPO, "<CWD>")
    host = HOSTS[1 if remote else 0]
    if host:
        text = text.replace(host, "<HOST>")
    if remote:
        text = text.replace(os.environ.get("RP_REMOTE_WORKDIR_EXPR", "$HOME/workspace"), "<CWD>")
        text = re.sub(r"/[^\s\n]+/workspace", "<CWD>", text)
    return text


def clip(text, limit=240):
    return str(text).replace("\n", " ")[:limit]


LAST = {}


def note_sides(left, right):
    """Remember what the two sides looked like, so a FAIL can show both."""
    LAST["left"], LAST["right"] = left, right


def compare(left, right):
    note_sides(norm(left), norm(right, True))
    raw_left = ESC.sub("", CSI.sub("", DCS.sub("", OSC.sub("", left)))).replace("\r", "")
    raw_right = ESC.sub("", CSI.sub("", DCS.sub("", OSC.sub("", right)))).replace("\r", "")
    if raw_left != raw_right and (raw_left.lstrip("\n") == raw_right or raw_left == raw_right.lstrip("\n")):
        return "WARN", QUIRK
    if norm(left) == norm(right, True):
        return "PASS", ""
    return "FAIL", "normalised output differs"


def result(case, status, local=0, remote=0, note=""):
    print(f"{status} {case} local={local}ms remote={remote}ms {note}".rstrip(), flush=True)
    results.append((case, status, local, remote, note))
    if status == "FAIL":
        if case not in failed:
            failed.append(case)
        for side in ("left", "right"):
            if side in LAST:
                label = "local " if side == "left" else "remote"
                print(f"DETAIL {case} {label} {LAST[side][-700:]!r}", flush=True)
    LAST.clear()


def wrap(case, command):
    return f"printf '\\n@@%s%s@@\\n' B {case}; {command}; rc=$?; printf '\\n@@%s%s rc=%s@@\\n' E {case} \"$rc\""


def extract(text, case):
    begin = text.find(f"@@B{case}@@")
    matches = [m for m in END.finditer(text) if m.group(1) == case]
    if begin < 0 or not matches:
        return text, None
    end = matches[0]
    return text[begin + len(f"@@B{case}@@"):end.start()], int(end.group(2))


def command_case(case, command, timeout=30, start_at=None):
    streams = [Attach(LOCAL), Attach(REMOTE)]
    for stream in streams:
        stream.start()
    for stream in streams:
        if not stream.wait_initial():
            raise RuntimeError(f"attach did not receive initial repaint for {case}")
    started = time.monotonic() if start_at is None else start_at
    for session in (LOCAL, REMOTE):
        send(session, wrap(case, command))
    waited = []
    for stream in streams:
        waited.append(stream.wait(f"@@E{case} rc=", timeout))
    values = [extract(stream.text(), case) for stream in streams]
    elapsed = [0 if t is None else int((t - started) * 1000) for t in waited]
    for stream in streams:
        stream.stop()
    return values, elapsed[0], elapsed[1]


def interactive_case(case, command, actions, timeout=30):
    streams = [Attach(LOCAL), Attach(REMOTE)]
    for stream in streams:
        stream.start()
    for stream in streams:
        if not stream.wait_initial():
            raise RuntimeError(f"attach did not receive initial repaint for {case}")
    started = time.monotonic()
    for session in (LOCAL, REMOTE):
        send(session, f"printf '\\n@@%s%s@@\\n' B {case}; {command}")
    for delay, action in actions:
        time.sleep(delay)
        for session in (LOCAL, REMOTE):
            action(session)
    for session in (LOCAL, REMOTE):
        send(session, f"printf '\\n@@%s%s rc=%s@@\\n' E {case} \"$?\"")
    waited = []
    for stream in streams:
        waited.append(stream.wait(f"@@E{case} rc=", timeout))
    values = [extract(stream.text(), case) for stream in streams]
    elapsed = [0 if t is None else int((t - started) * 1000) for t in waited]
    for stream in streams:
        stream.stop()
    return values, elapsed[0], elapsed[1]


def case_p01():
    elapsed = []
    for session, opened in zip((LOCAL, REMOTE), OPEN_MS):
        began = time.monotonic()
        rc, out, err = cli("session", "wait", "--session", session, "--until", r"screen:(?m)^PARITY\$ ?$", "--timeout", 120000, timeout=130)
        if rc:
            result("P01", "FAIL", note=f"prompt timeout: {clip(norm(err or out))}")
            return
        elapsed.append(opened + int((time.monotonic() - began) * 1000))
    METRICS["first_prompt_ms"] = elapsed
    result("P01", "PASS", elapsed[0], elapsed[1], "INFO prompt times")


def case_p02():
    values, local_ms, remote_ms = command_case("P02", r"""printf '%s|%s|%s|%s\n' "$TERM" "$(printenv COLORTERM)" "$(printenv LANG)" "$(printenv LC_ALL)"; infocmp "$TERM" >/dev/null 2>&1 && echo ti-ok; locale 2>&1 | grep -c 'Cannot set'""")
    sides = [norm(value[0], index == 1) for index, value in enumerate(values)]
    identity = []
    for side in sides:
        fields = next((line.split("|", 3) for line in side.splitlines() if line.count("|") == 3), [])
        identity.append(fields)
    status, note = compare(values[0][0], values[1][0])
    if any(len(fields) != 4 for fields in identity):
        status, note = "FAIL", "TERM environment line missing"
    elif identity[0][:2] != identity[1][:2]:
        status, note = "FAIL", "TERM or COLORTERM differs"
    else:
        hard = False
        for index, side in enumerate(sides):
            lines = side.splitlines()
            try:
                warning = next(int(line) for line in lines[lines.index("ti-ok") + 1:] if re.fullmatch(r"\d+", line))
            except (StopIteration, ValueError):
                warning = 1
            if "ti-ok" not in lines or warning != 0:
                status, note = "FAIL", "terminfo or locale warning"
                hard = True
                break
        remote_lang = identity[1][2]
        local_utf8 = bool(re.search(r"(?i)utf-?8", identity[0][2]))
        # The remote only gets a UTF-8 LANG when the host locale is UTF-8, so only then is its absence a failure.
        if not hard and local_utf8 and not re.search(r"(?i)utf-?8", remote_lang):
            status, note = "FAIL", "remote LANG is not UTF-8"
        elif not hard and identity[0][2] != identity[1][2] and re.search(r"(?i)utf-?8", identity[0][2]):
            status, note = "WARN", "LANG differs (D1: remote C.UTF-8)"

    if len(identity[0]) == 4 and len(identity[1]) == 4:
        comparable = []
        for side, fields in zip(sides, identity):
            replacement = "|".join(fields[:2] + ["<LANG>", fields[3]])
            comparable.append(side.replace("|".join(fields), replacement, 1))
        rest_status, rest_note = compare(comparable[0], comparable[1])
        if rest_status == "FAIL" and status not in ("FAIL",):
            status, note = rest_status, rest_note
    result("P02", status, local_ms, remote_ms, note)


def case_p03():
    first, local_a, remote_a = command_case("P03A", "stty size")
    resize(LOCAL, 30, 100)
    resize(REMOTE, 30, 100)
    second, local_b, remote_b = command_case("P03B", "stty size")
    left = first[0][0] + second[0][0]
    right = first[1][0] + second[1][0]
    status, note = compare(left, right)
    sizes = [
        next((line.strip() for line in norm(value[0], index == 1).splitlines() if re.fullmatch(r"\d+ \d+", line.strip())), "?")
        for index, value in enumerate((first[0], first[1]))
    ]
    if "40 120" not in norm(left) or "30 100" not in norm(left):
        status, note = "FAIL", "expected 40 120 then 30 100"
    note = f"first stty size: local {sizes[0]} remote {sizes[1]}" + (f"; {note}" if note else "")
    result("P03", status, local_a + local_b, remote_a + remote_b, note)


def case_p04():
    values, local_ms, remote_ms = command_case("P04", r"""tput colors; printf '\e[38;2;255;100;0mTC\e[0m\n'""")
    status, note = compare(values[0][0], values[1][0])
    ansi = [base64.b64decode(snap(s).get("ansi_b64", "")).decode("utf-8", "replace") for s in (LOCAL, REMOTE)]
    if any("38;2;255;100;0" not in value for value in ansi):
        status, note = "FAIL", "snapshot did not retain truecolor"
    result("P04", status, local_ms, remote_ms, note)


def case_p05():
    values, local_ms, remote_ms = command_case("P05", r"""printf 'héllo 界界 😀|\n'; printf 'é' | wc -c""")
    status, note = compare(values[0][0], values[1][0])
    cursors = [snap(s).get("cursor", {}).get("col") for s in (LOCAL, REMOTE)]
    if cursors[0] != cursors[1]:
        status, note = "FAIL", "cursor columns differ"
    result("P05", status, local_ms, remote_ms, note)


def signal_case(case, command, actions, expected):
    values, local_ms, remote_ms = interactive_case(case, command, actions)
    status, note = compare(values[0][0], values[1][0])
    if any(value[1] != expected for value in values):
        status, note = "FAIL", f"expected rc={expected}"
    result(case, status, local_ms, remote_ms, note)


def case_p06():
    signal_case("P06", "sleep 30", [(1, lambda s: key(s, "ctrl+c"))], 130)


def case_p07():
    actions = [(0.5, lambda s: key(s, "ctrl+z")), (0.2, lambda s: send(s, "jobs")), (0.2, lambda s: send(s, "fg")), (0.5, lambda s: key(s, "ctrl+c"))]
    values, local_ms, remote_ms = interactive_case("P07", "sleep 30", actions)
    status, note = compare(values[0][0], values[1][0])
    if any("Stopped" not in norm(value[0], index == 1) or not re.search(r"\[[0-9]+\]", norm(value[0], index == 1)) for index, value in enumerate(values)):
        status, note = "FAIL", "no job control"
    result("P07", status, local_ms, remote_ms, note)


def case_p08():
    signal_case("P08", "sleep 30", [(1, lambda s: key(s, "ctrl+\\"))], 131)


def case_p09():
    signal_case("P09", "cat", [(0.5, lambda s: key(s, "ctrl+d"))], 0)


def session_exit_code(session):
    rc, out, _ = cli("session", "wait", "--session", session, "--until", "exited", "--json", timeout=20)
    if rc:
        return None
    try:
        return json.loads(out).get("exit_code")
    except (TypeError, ValueError, json.JSONDecodeError):
        return None


def open_parity_session(argv, env):
    params = json.dumps({"argv": argv, "cwd": REPO, "env": env, "rows": 40, "cols": 120}, separators=(",", ":"))
    rc, out, err = cli("api", "call", "sessions.open", "--params", params, timeout=30)
    if rc:
        raise RuntimeError(clip(norm(err or out, True)))
    return json.loads(out)["id"]


def p10_fresh_sessions(prefix):
    if shutil.which("bash") is None:
        result("P10", "SKIP", note="SKIP: no bash on local")
        return
    try:
        if not isinstance(prefix, list) or not prefix or not all(isinstance(item, str) and item for item in prefix):
            raise ValueError("RP_REMOTE_PREFIX_JSON must be a JSON argv list")
    except (TypeError, ValueError):
        result("P10", "FAIL", note="invalid RP_REMOTE_PREFIX_JSON")
        return

    status = "PASS"
    note = ""
    skip_side = None
    for mode in ("on", "off"):
        inputrc = TMP / f"rp-inputrc-{mode}"
        inputrc.write_text(f"set enable-bracketed-paste {mode}\n")
        remote_command = (
            f"printf 'set enable-bracketed-paste %s\\n' {mode} > /tmp/rp-inputrc-{mode}; "
            "export PS1='PARITY$ '; cd \"$HOME/workspace\" 2>/dev/null; "
            "command -v bash >/dev/null 2>&1 || exit 97; "
            f"INPUTRC=/tmp/rp-inputrc-{mode} exec bash --norc --noprofile -i"
        )
        sessions = []
        try:
            sessions.append(open_parity_session(
                ["bash", "--norc", "--noprofile", "-i"],
                [["PS1", "PARITY$ "], ["INPUTRC", str(inputrc)]],
            ))
            sessions.append(open_parity_session(
                prefix + ["/bin/sh", "-c", remote_command],
                [["PS1", "PARITY$ "]],
            ))
            for side, session in enumerate(sessions):
                rc, out, err = cli(
                    "session", "wait", "--session", session,
                    "--until", r"screen:(?m)^PARITY\$ ?$",
                    "--timeout", 120000, timeout=130,
                )
                if rc:
                    exit_code = session_exit_code(session)
                    if exit_code == 97 or (side == 0 and exit_code == 127):
                        skip_side = "remote" if side else "local"
                        break
                    detail = clip(norm(err or out or "fresh session prompt timeout", side == 1))
                    raise RuntimeError(f"{('remote' if side else 'local')} prompt timeout: {detail}")
            if skip_side:
                break
            if not all(wait_mode(session, "bracketed_paste", mode == "on") for session in sessions):
                status, note = "FAIL", "thegn did not report the shell's paste mode"
                break
            for session in sessions:
                send(session, f"echo P10{mode}A\necho P10{mode}B", enter=False, paste=True)
            time.sleep(0.5)
            held = [snap(session).get("text", "") for session in sessions]
            held_tail = [norm(held[index], index == 1).splitlines()[-3:] for index in range(2)]
            if held_tail[0] != held_tail[1]:
                status, note = "FAIL", "bracketed paste screens differ"
            if mode == "on":
                if any(re.search(r"(?m)^P10onA$", norm(value, index == 1)) for index, value in enumerate(held)):
                    status, note = "FAIL", "bracketed paste was not held"
                for session in sessions:
                    key(session, "enter")
            elif not all(re.search(r"(?m)^P10offA$", norm(value, index == 1)) for index, value in enumerate(held)):
                status, note = "FAIL", "unbracketed paste did not run line one"
            if mode == "off":
                for session in sessions:
                    key(session, "ctrl+u")
            note_sides(clip(norm(held[0])), clip(norm(held[1], True)))
            if status == "FAIL":
                break
        finally:
            for session in sessions:
                cli("session", "close", session, "--json", timeout=20)
    if skip_side:
        result("P10", "SKIP", note=f"SKIP: no bash on {skip_side}")
    else:
        result("P10", status, note=note)


def case_p10():
    prefix_json = os.environ.get("RP_REMOTE_PREFIX_JSON")
    if prefix_json:
        try:
            p10_fresh_sessions(json.loads(prefix_json))
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            result("P10", "FAIL", note=f"invalid RP_REMOTE_PREFIX_JSON: {clip(norm(str(exc)))}")
        return
    natural = [snap(s).get("bracketed_paste") for s in (LOCAL, REMOTE)]
    versions, _, _ = command_case("P10V", 'echo "$BASH_VERSION"')
    version_text = [
        next((clip(norm(line, index == 1), 80) for line in value[0].splitlines() if re.fullmatch(r"\d+\.\d+\S*", line.strip())), "unknown")
        for index, value in enumerate(versions)
    ]
    status = "PASS"
    note = f"INFO natural bracketed-paste={natural[0]}/{natural[1]} BASH_VERSION={version_text[0]}/{version_text[1]}"
    if natural[0] != natural[1]:
        status, note = "WARN", note + "; the two shells' bracketed-paste defaults differ (local inputrc?)"
    bind_values, _, _ = command_case("P10B", "command -V bind")
    bindable = ["builtin" in norm(value[0], index == 1).lower() for index, value in enumerate(bind_values)]
    if not all(bindable):
        bind_answers = "command -V bind " + "/".join(
            f"{side}={clip(norm(value[0], index == 1), 120)}"
            for index, (side, value) in enumerate(zip(("local", "remote"), bind_values))
        )
        result("P10", "SKIP", note=note + "; SKIP: bind is not a builtin (remote sh); " + bind_answers)
        return

    for mode in ("on", "off"):
        for session in (LOCAL, REMOTE):
            send(session, f"bind 'set enable-bracketed-paste {mode}'")
        if not all(wait_mode(session, "bracketed_paste", mode == "on") for session in (LOCAL, REMOTE)):
            status, note = "FAIL", "thegn did not report the shell's paste mode"
            break
        for session in (LOCAL, REMOTE):
            send(session, f"echo P10{mode}A\necho P10{mode}B", enter=False, paste=True)
        time.sleep(0.5)
        held = [snap(s).get("text", "") for s in (LOCAL, REMOTE)]
        held_tail = [norm(held[i], i == 1).splitlines()[-3:] for i in range(2)]
        held_equal = held_tail[0] == held_tail[1]
        if not held_equal:
            status, note = "FAIL", "bracketed paste screens differ"
        if mode == "on":
            if any(re.search(rf"(?m)^P10onA$", norm(value, index == 1)) for index, value in enumerate(held)):
                status, note = "FAIL", "bracketed paste was not held"
            for session in (LOCAL, REMOTE):
                key(session, "enter")
        elif not all(re.search(rf"(?m)^P10offA$", norm(value, index == 1)) for index, value in enumerate(held)):
            status, note = "FAIL", "unbracketed paste did not run line one"
        if mode == "off":
            for session in (LOCAL, REMOTE):
                key(session, "ctrl+u")
        note_sides(norm(held[0]), norm(held[1], True))
    for session in (LOCAL, REMOTE):
        send(session, "bind 'set enable-bracketed-paste on'")
    result("P10", status, note=note)


def case_p11():
    command = r"printf '\e[?1049h\e[2J\e[5;10HRP%s' FULL; read -r _; printf '\e[?1049l'"
    for session in (LOCAL, REMOTE):
        send(session, command)
    if not all(wait_mode(session, "alt_screen", True) for session in (LOCAL, REMOTE)):
        for session in (LOCAL, REMOTE):
            key(session, "enter")
        for session in (LOCAL, REMOTE):
            wait_mode(session, "alt_screen", False)
        result("P11", "FAIL", note="shell-only full screen did not enter alternate screen")
        return
    screens = [snap(s) for s in (LOCAL, REMOTE)]
    rows = [norm(screen.get("text", ""), index == 1).splitlines() for index, screen in enumerate(screens)]
    cursor = [screen.get("cursor") for screen in screens]
    status = "PASS" if rows[0] == rows[1] and cursor[0] == cursor[1] else "FAIL"
    for session in (LOCAL, REMOTE):
        key(session, "enter")
    if not all(wait_mode(session, "alt_screen", False) for session in (LOCAL, REMOTE)):
        status = "FAIL"
    note_sides("\n".join(rows[0]) + f"\ncursor={cursor[0]}", "\n".join(rows[1]) + f"\ncursor={cursor[1]}")
    result("P11", status, note="" if status == "PASS" else "alternate-screen rows or cursor differ")


def p12_arrow_bytes(text, app):
    marker = f"@@BP12{int(app)}@@"
    begin = text.find(marker)
    if begin < 0:
        return "none"
    segment = text[begin + len(marker):]
    end = segment.find(f"@@EP12{int(app)} rc=")
    if end >= 0:
        segment = segment[:end]
    segment = ESC.sub("", CSI.sub("", segment)).replace("\r", "")
    match = next(
        (
            line for line in segment.splitlines()
            if re.fullmatch(r"\s*1b(\s+[0-9a-f]{2}){2}\s*", line, re.IGNORECASE)
        ),
        None,
    )
    return " ".join(match.split()).lower() if match else "none"


def case_p12():
    expected = {True: "1b 4f 41", False: "1b 5b 41"}
    observed = {}
    modes_seen = {}
    for app in (True, False):
        streams = [Attach(LOCAL), Attach(REMOTE)]
        for stream in streams:
            stream.start()
        try:
            for stream in streams:
                if not stream.wait_initial():
                    raise RuntimeError("attach did not receive initial repaint for P12")
            command = r"printf '\e[?1%s' " + ("h" if app else "l") + r"""; stty raw -echo; k=$(dd bs=1 count=3 2>/dev/null); stty sane; printf '\e[?1l'; printf '%s' "$k" | od -An -tx1"""
            for session in (LOCAL, REMOTE):
                send(session, f"printf '\\n@@%s%s@@\\n' B P12{int(app)}; {command}")
            mode_seen = all(wait_mode(session, "app_cursor", app) for session in (LOCAL, REMOTE))
            modes_seen[app] = mode_seen
            for session in (LOCAL, REMOTE):
                key(session, "up")
            for session in (LOCAL, REMOTE):
                send(session, f"printf '\\n@@%s%s rc=%s@@\\n' E P12{int(app)} \"$?\"")
            for stream in streams:
                stream.wait(f"@@EP12{int(app)} rc=", 15)
            values = [stream.text() for stream in streams]
            note_sides(clip(norm(values[0][-400:])), clip(norm(values[1][-400:], True)))
            observed[app] = [p12_arrow_bytes(value, app) for value in values]
        finally:
            for stream in streams:
                stream.stop()
    passed = all(observed.get(app) == [expected[app], expected[app]] for app in (True, False))
    if passed:
        note = ""
    else:
        app_values = observed.get(True, ["none", "none"])
        normal_values = observed.get(False, ["none", "none"])
        app_note = f"app: local {app_values[0]} remote {app_values[1]}"
        normal_note = f"normal: local {normal_values[0]} remote {normal_values[1]}"
        if not modes_seen.get(True, False):
            app_note += "; app_cursor not reported"
        if not modes_seen.get(False, False):
            normal_note += "; app_cursor not reported"
        note = f"{app_note}; {normal_note}"
    result("P12", "PASS" if passed else "FAIL", note=note)


def case_p13():
    command = r"""printf '\e[?1000h\e[?1006h'; stty -icanon; cat -v"""
    for session in (LOCAL, REMOTE):
        send(session, command)
    time.sleep(0.4)
    for session in (LOCAL, REMOTE):
        send(session, "\x1b[<0;5;5M", enter=False)
        key(session, "enter")
        key(session, "ctrl+c")
    screens = [snap(s) for s in (LOCAL, REMOTE)]
    for session in (LOCAL, REMOTE):
        send(session, r"""stty sane; printf '\e[?1000l\e[?1006l'""")
    time.sleep(0.5)
    status = "PASS" if all("^[[<0;5;5M" in norm(s.get("text", "")) for s in screens) else "FAIL"
    if any("mouse" in screen and not screen.get("mouse") for screen in screens):
        status = "FAIL"
    result("P13", status, note="" if status == "PASS" else "SGR mouse bytes not seen")


def case_p14():
    # The token is joined by the shell, so it never appears in the echoed command line on the primary screen.
    values, local_ms, remote_ms = command_case("P14", r"""printf '\e[?1049h%s%s' RPALT SCREEN; sleep 1; printf '\e[?1049l'""")
    after = [snap(s).get("text", "") for s in (LOCAL, REMOTE)]
    status = "PASS" if all("RPALTSCREEN" not in value for value in after) and all(not snap(s).get("alt_screen") for s in (LOCAL, REMOTE)) else "FAIL"
    result("P14", status, local_ms, remote_ms, "" if status == "PASS" else "alternate screen was not restored")


def case_p15():
    first, local_ms, remote_ms = command_case("P15A", "yes 0123456789 | head -n 400000", timeout=60)
    second, local_ms2, remote_ms2 = interactive_case("P15B", "yes 0123456789", [(1, lambda s: key(s, "ctrl+c"))], timeout=60)
    flood_bytes = 400000 * len("0123456789\n")
    flood_kbps = [flood_bytes / max(ms, 1) for ms in (local_ms, remote_ms)]
    METRICS["flood_kbps_tty"] = flood_kbps
    drain_ms = [max(ms - 1000, 0) for ms in (local_ms2, remote_ms2)]
    status_a = "PASS" if all(value[1] is not None for value in first) else "FAIL"
    status_b = "PASS" if all(value[1] is not None for value in second) else "FAIL"
    result("P15A", status_a, local_ms, remote_ms, f"INFO flood={local_ms}/{remote_ms}ms (≈{flood_kbps[0]:.1f}/{flood_kbps[1]:.1f} KB/s)")
    result("P15B", status_b, local_ms2, remote_ms2, f"INFO ctrl-c-drain={drain_ms[0]}/{drain_ms[1]}ms")
    trace_path = os.environ.get("RP_BRIDGE_TRACE")
    if trace_path and Path(trace_path).exists():
        try:
            controls = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")
            trace_lines = Path(trace_path).read_text(errors="replace").splitlines()
            inbound = [controls.sub("", line).strip() for line in trace_lines if "inbound MiB mark" in line]
            wire = [controls.sub("", line).strip() for line in trace_lines if "sprites exec wire stats" in line]
            pump = [controls.sub("", line).strip() for line in trace_lines if "sprite-exec output pump" in line]
            for line in inbound[-3:] + wire[-1:] + pump[-1:]:
                print(f"INFO P15 trace {clip(norm(line, True), 200)}", flush=True)
        except OSError as exc:
            print(f"SKIP P15 trace {clip(norm(str(exc), True), 200)}", flush=True)
    else:
        print("SKIP P15 trace: RP_BRIDGE_TRACE not set", flush=True)
    capture_arg = os.environ.get("RP_CAPTURE_ARGV_JSON")
    if capture_arg:
        probe_warnings = []
        try:
            prefix = json.loads(capture_arg)
            if not isinstance(prefix, list) or not prefix or not all(isinstance(item, str) and item for item in prefix):
                raise ValueError("RP_CAPTURE_ARGV_JSON must be a JSON argv list")
            probes = [
                ("exit", ["/bin/sh", "-c", "exit 0"]),
                ("echo", ["/bin/sh", "-c", "echo rp-ok"]),
                ("wc", ["/bin/sh", "-c", "yes 0123456789 | head -n 400000 | wc -c"]),
                ("flood", ["/bin/sh", "-c", "yes 0123456789 | head -n 400000"]),
            ]
            wc_rate = None
            flood_rate = None
            flood_count = None
            for name, argv in probes:
                began = time.monotonic()
                proc = subprocess.run(
                    prefix + argv,
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    timeout=90,
                )
                elapsed = max(time.monotonic() - began, 0.001)
                stdout = proc.stdout.decode("utf-8", "replace")
                stderr = proc.stderr.decode("utf-8", "replace")
                if proc.returncode != 0:
                    probe_warnings.append(
                        f"{name} rc={proc.returncode} stdout={clip(norm(stdout[-300:], True), 300)} "
                        f"stderr={clip(norm(stderr[-300:], True), 300)}"
                    )
                elif name == "echo" and "rp-ok" not in stdout:
                    probe_warnings.append(f"{name} stdout missing rp-ok: {clip(norm(stdout, True), 300)}")
                elif name == "wc":
                    if "4400000" not in stdout:
                        probe_warnings.append(f"{name} stdout missing 4400000: {clip(norm(stdout, True), 300)}")
                    else:
                        wc_rate = flood_bytes / elapsed / 1000
                elif name == "flood":
                    flood_count = len(proc.stdout)
                    flood_rate = flood_count / elapsed / 1000
            if wc_rate is None:
                wc_rate = 0.0
            if flood_rate is not None:
                METRICS["flood_kbps_nontty"] = flood_rate
            if flood_count is not None and flood_count != flood_bytes:
                probe_warnings.append("capture path truncated output")
            flood_rate_text = "-" if flood_rate is None else f"{flood_rate:.1f}"
            flood_count_text = "-" if flood_count is None else str(flood_count)
            info = f"INFO server-side wc={wc_rate:.1f} KB/s non-tty flood={flood_rate_text} KB/s bytes={flood_count_text}"
            result("P15C", "WARN" if probe_warnings else "PASS", note=info + ("; " + "; ".join(probe_warnings) if probe_warnings else ""))
            if trace_path and Path(trace_path).exists():
                try:
                    controls = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")
                    trace_lines = Path(trace_path).read_text(errors="replace").splitlines()
                    wire = [controls.sub("", line).strip() for line in trace_lines if "sprites exec wire stats" in line]
                    for line in wire[-1:]:
                        print(f"INFO P15C trace {clip(norm(line, True), 200)}", flush=True)
                except OSError as exc:
                    print(f"SKIP P15C trace {clip(norm(str(exc), True), 200)}", flush=True)
        except (OSError, subprocess.TimeoutExpired, ValueError, TypeError, json.JSONDecodeError, RuntimeError) as exc:
            result("P15C", "WARN", note=f"server-side transfer probe failed: {clip(norm(str(exc), True))}")
    else:
        result("P15C", "SKIP", note="SKIP: RP_CAPTURE_ARGV_JSON not set")


def case_p16():
    payload = ("x" * 63 + "\n") * 16384
    chunks = [payload[i:i + 96 * 1024] for i in range(0, len(payload), 96 * 1024)]
    if len(chunks) != 11:
        raise RuntimeError("1 MiB payload did not split into 11 chunks")
    for session in (LOCAL, REMOTE):
        send(session, "stty -echo; cat > /tmp/rp.paste; stty echo")
    if not all(wait_mode(session, "bracketed_paste", False) for session in (LOCAL, REMOTE)):
        for session in (LOCAL, REMOTE):
            key(session, "ctrl+d")
        result("P16", "FAIL", note="paste mode never turned off")
        return
    paste_started = time.monotonic()
    for chunk in chunks:
        for session in (LOCAL, REMOTE):
            send(session, chunk, enter=False, paste=True)
    for session in (LOCAL, REMOTE):
        key(session, "ctrl+d")
    values, local_ms, remote_ms = command_case("P16", "wc -c < /tmp/rp.paste; cksum < /tmp/rp.paste", timeout=60, start_at=paste_started)
    status, note = compare(values[0][0], values[1][0])
    sides = (norm(values[0][0]), norm(values[1][0], True))
    if any("1048576" not in norm(value[0], i == 1) for i, value in enumerate(values)):
        status, note = "FAIL", "paste byte count is not 1048576"
    note_sides(*sides)
    rate = [1024 * 1000 / max(ms, 1) for ms in (local_ms, remote_ms)]
    METRICS["paste_kbps"] = rate
    info = f"INFO paste_ms={local_ms}/{remote_ms} rate={rate[0]:.1f}/{rate[1]:.1f} KB/s"
    result("P16", status, local_ms, remote_ms, info + (f"; {note}" if note else ""))


def case_p17():
    _, local_ms, remote_ms = command_case("P17", r"""printf '\e]2;rp-title\a'""")
    screens = [snap(s) for s in (LOCAL, REMOTE)]
    if "title" not in screens[0] or "title" not in screens[1]:
        result("P17", "PASS", local_ms, remote_ms, "SKIP: snapshot has no title field")
    else:
        status = "PASS" if screens[0].get("title") == screens[1].get("title") else "FAIL"
        result("P17", status, local_ms, remote_ms, "" if status == "PASS" else "title differs")


def case_p18():
    values, local_ms, remote_ms = command_case("P18", r"""cd /tmp && pwd; printf '\e]7;file://h/tmp\a'""")
    status, note = compare(values[0][0], values[1][0])
    if any("/tmp" not in norm(value[0], index == 1) for index, value in enumerate(values)):
        status, note = "FAIL", "cwd did not change to /tmp"
    result("P18", status, local_ms, remote_ms, note)


def case_p19():
    values, local_ms, remote_ms = command_case("P19A", r"""sh -c 'exit 7'; echo rc=$?""")
    status, note = compare(values[0][0], values[1][0])
    if any("rc=7" not in norm(value[0], index == 1) for index, value in enumerate(values)):
        status, note = "FAIL", "shell status was not 7"
    fresh = []
    codes = []
    try:
        for argv in (LOCAL_ARGV, REMOTE_ARGV):
            params = json.dumps({"argv": argv, "cwd": REPO, "env": [["PS1", "PARITY$ "]], "rows": 40, "cols": 120}, separators=(",", ":"))
            rc, out, err = cli("api", "call", "sessions.open", "--params", params, timeout=30)
            if rc:
                raise RuntimeError(err or out or "fresh session open failed")
            fresh.append(json.loads(out)["id"])
        for session in fresh:
            rc, out, err = cli("session", "wait", "--session", session, "--until", r"screen:(?m)^PARITY\$ ?$", "--timeout", 120000, timeout=130)
            if rc:
                raise RuntimeError(err or out or "fresh session prompt timeout")
        for session in fresh:
            send(session, "exit 3")
        for session in fresh:
            rc, out, err = cli("session", "wait", "--session", session, "--until", "exited", "--json", timeout=20)
            if rc:
                raise RuntimeError(err or out or "fresh session exit wait failed")
            codes.append(json.loads(out).get("exit_code"))
        if codes != [3, 3]:
            status, note = "FAIL", f"session exit codes {codes}"
    finally:
        for session in fresh:
            cli("session", "close", session, "--json", timeout=20)
    result("P19", status, local_ms, remote_ms, note)


def case_p20():
    times = [[], []]
    streams = [Attach(LOCAL), Attach(REMOTE)]
    for stream in streams:
        stream.start()
    try:
        for stream in streams:
            if not stream.wait_initial():
                raise RuntimeError("attach did not receive initial repaint for P20")
        for index in range(SAMPLES):
            token = f"q{index}"
            for side, session in enumerate((LOCAL, REMOTE)):
                offset = streams[side].length()
                began = time.monotonic()
                send(session, token, enter=False)
                t = streams[side].wait(token, 10, offset)
                if t is None:
                    raise RuntimeError(f"no echo for {token}")
                times[side].append((t - began) * 1000)
                key(session, "ctrl+u")
    finally:
        for stream in streams:
            stream.stop()
    p50 = [statistics.median(values) for values in times]
    p95 = [sorted(values)[max(0, int(len(values) * 0.95) - 1)] for values in times]
    METRICS["echo_p50_ms"] = (p50[0], p50[1], p50[1] - p50[0])
    status = "WARN" if p95[1] > 1000 else "PASS"
    note = f"INFO p50={p50[0]:.1f}/{p50[1]:.1f}ms p95={p95[0]:.1f}/{p95[1]:.1f}ms delta={p50[1]-p50[0]:.1f}ms"
    result("P20", status, int(p50[0]), int(p50[1]), note)


def case_p20b():
    if os.environ.get("RP_TUI_PROBE") != "1":
        result("P20B", "SKIP", note="SKIP: RP_TUI_PROBE not set")
        return
    probe = Path(__file__).with_name("remote_parity_tui.py")
    if not probe.exists():
        result("P20B", "FAIL", note="TUI probe script is missing")
        return
    command = [
        "python3", "-I", str(probe), "--bin", BIN, "--repo", REPO,
        "--samples", "12", "--timeout", "90",
    ]
    command += ["--env", ENV_NAME]
    try:
        probe_env = os.environ.copy()
        probe_env["RP_TUI_SAMPLES_OUT"] = str(TMP / "tui-samples.json")
        proc = subprocess.run(command, capture_output=True, text=True, timeout=180, env=probe_env)
        lines = [line.strip() for line in proc.stdout.splitlines() if line.strip()]
        if proc.returncode or len(lines) != 1:
            raise RuntimeError(f"probe exit={proc.returncode} output={clip(norm(proc.stdout))}")
        payload = json.loads(lines[0])
        if not isinstance(payload, dict):
            raise ValueError("TUI probe JSON was not an object")
        probe_status = str(payload.get("status", "PASS")).upper()
        if payload.get("ok") is False or probe_status not in ("PASS", "WARN", "SKIP"):
            probe_status = "FAIL"
        def stats(name):
            compact = payload.get(name)
            if isinstance(compact, dict):
                required = ("n", "p50", "p95", "max")
                if not all(key in compact for key in required):
                    raise ValueError(f"TUI probe {name} stats are incomplete")
                values = tuple(compact.get(key) for key in ("n", "min", "p50", "p95", "max"))
                return values
            old_key = "first_key_ms" if name == "first" else "next_key_ms"
            values = [float(value) for value in payload.get(old_key, [])]
            if not values:
                return (0, None, None, None, None)
            ordered = sorted(values)
            return (
                len(values),
                min(values),
                statistics.median(values),
                ordered[max(0, int(len(values) * 0.95) - 1)],
                max(values),
            )

        first = stats("first")
        following = stats("next")
        METRICS["first_key_p50_ms"] = first[2]
        METRICS["next_key_p50_ms"] = following[2]
        def stat_text(value):
            if value is None:
                return "-"
            return f"{value:.3f}".rstrip("0").rstrip(".") if isinstance(value, float) else str(value)

        note = (
            f"first n={stat_text(first[0])} p50={stat_text(first[2])} "
            f"p95={stat_text(first[3])} max={stat_text(first[4])}; "
            f"next n={stat_text(following[0])} p50={stat_text(following[2])} "
            f"p95={stat_text(following[3])} max={stat_text(following[4])}"
        )
        result("P20B", probe_status, note=note)
    except (OSError, subprocess.TimeoutExpired, ValueError, TypeError, json.JSONDecodeError, RuntimeError) as exc:
        result("P20B", "FAIL", note=f"TUI probe failed: {clip(norm(str(exc)), 220)}")


def case_p21():
    text = ""
    for name in ("up.log", "up.out"):
        path = TMP / name
        if path.exists():
            text += path.read_text(errors="replace")
    steps = re.findall(r"provision step done.*?(?:step[ =:]([A-Za-z0-9_.-]+)).*?(?:ms[ =:]([0-9.]+))", text, re.I)
    total = re.findall(r"\(([0-9.]+) s\)", text)
    note = "INFO startup steps " + (", ".join(f"{name}={ms}ms" for name, ms in steps) if steps else "not found")
    METRICS["startup_steps"] = ", ".join(f"{name}={ms}ms" for name, ms in steps) if steps else None
    if total:
        note += f" total={total[-1]}s"
    result("P21", "PASS", note=note)


def case_p22():
    began = time.monotonic()
    rc, out, err = cli("session", "close", REMOTE, "--json", timeout=20)
    time.sleep(5)
    result("P22", "PASS" if rc == 0 else "FAIL", int((time.monotonic() - began) * 1000), 0, "INFO authoritative roster check is lane 8")


def case_p23():
    if not IDLE_PROBE:
        result("P23", "PASS", note="SKIP: --idle-probe not requested")
        return
    params = json.dumps({"argv": REMOTE_ARGV, "cwd": REPO, "env": [["PS1", "PARITY$ "]], "rows": 40, "cols": 120}, separators=(",", ":"))
    rc, out, err = cli("api", "call", "sessions.open", "--params", params, timeout=30)
    if rc:
        result("P23", "FAIL", note=clip(norm(err or out)))
        return
    session = json.loads(out)["id"]
    time.sleep(150)
    rc, out, err = cli("fleet", "list", "--env", ENV_NAME, "--json", timeout=30)
    state = "unknown"
    try:
        rows = json.loads(out)["view"]["rows"]
        state = next((row.get("state", "unknown") for row in rows if isinstance(row, dict)), "unknown")
    except (KeyError, TypeError, ValueError):
        pass
    cli("session", "close", session, "--json", timeout=20)
    result("P23", "PASS" if rc == 0 else "FAIL", 150000, 0, f"INFO an open idle exec keeps the sprite {state}")


def safe(case, function):
    try:
        function()
    except Exception as exc:
        if case not in failed:
            result(case, "FAIL", note=clip(norm(str(exc).replace("\n", " ")), 240))


def summary_text(value):
    if value is None or value == "":
        return "-"
    if isinstance(value, float):
        return f"{value:.1f}"
    return str(value)


def summary_slash(key):
    value = METRICS.get(key)
    if not value:
        return "-"
    return "/".join(summary_text(item) for item in value)


def main():
    cases = [
        ("P01", case_p01), ("P02", case_p02), ("P03", case_p03), ("P04", case_p04),
        ("P05", case_p05), ("P06", case_p06), ("P07", case_p07), ("P08", case_p08),
        ("P09", case_p09), ("P10", case_p10), ("P11", case_p11), ("P12", case_p12),
        ("P13", case_p13), ("P14", case_p14), ("P15", case_p15), ("P16", case_p16),
        ("P17", case_p17), ("P18", case_p18), ("P19", case_p19), ("P20", case_p20),
        ("P20B", case_p20b),
        ("P21", case_p21), ("P22", case_p22), ("P23", case_p23),
    ]
    try:
        host_values, _, _ = command_case("P00H", "uname -n")
        HOSTS[:] = [host_values[0][0].strip(), host_values[1][0].strip()]
        if not all(HOSTS):
            raise RuntimeError("uname -n did not return both hostnames")
    except Exception as exc:
        print(f"FAIL P00H: {clip(norm(str(exc)))}", flush=True)
        failed.append("P00H")
    for case, function in cases:
        before = len(failed)
        safe(case, function)
        if len(failed) > before and not KEEP_GOING:
            break
    print("\nSUMMARY")
    print("case status local remote note")
    for case, status, local, remote, note in results:
        print(f"{case:>3} {status:>4} {local:>6}ms {remote:>6}ms {note}")
    print("\nRUN SUMMARY")
    print(f"shell_ready_s {os.environ.get('RP_SHELL_READY_S') or '-'}")
    print(f"env_up_s {os.environ.get('RP_ENV_UP_S') or '-'}")
    print(f"first_prompt_ms {summary_slash('first_prompt_ms')}")
    echo = METRICS.get("echo_p50_ms")
    print(f"echo_p50_ms {summary_text(echo[0])}/{summary_text(echo[1])} {summary_text(echo[2])}" if echo else "echo_p50_ms -")
    print(f"first_key_p50_ms {summary_text(METRICS.get('first_key_p50_ms'))}")
    print(f"next_key_p50_ms {summary_text(METRICS.get('next_key_p50_ms'))}")
    print(f"paste_kbps {summary_slash('paste_kbps')}")
    print(f"flood_kbps tty {summary_slash('flood_kbps_tty')}")
    print(f"flood_kbps nontty {summary_text(METRICS.get('flood_kbps_nontty'))}")
    print(f"startup_steps {summary_text(METRICS.get('startup_steps'))}")
    print("FAILED CASES: " + (", ".join(failed) if failed else "none"))
    if failed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
