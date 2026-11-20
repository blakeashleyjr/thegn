#!/usr/bin/env python3
"""Driver for the two-session remote terminal parity run."""
import base64
import atexit
import json
import os
import re
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


def command_case(case, command, timeout=30):
    streams = [Attach(LOCAL), Attach(REMOTE)]
    for stream in streams:
        stream.start()
    for stream in streams:
        if not stream.wait_initial():
            raise RuntimeError(f"attach did not receive initial repaint for {case}")
    started = time.monotonic()
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
            result("P01", "FAIL", note=f"prompt timeout: {err or out}")
            return
        elapsed.append(opened + int((time.monotonic() - began) * 1000))
    result("P01", "PASS", elapsed[0], elapsed[1], "INFO prompt times")


def case_p02():
    values, local_ms, remote_ms = command_case("P02", r"""printf '%s|%s|%s|%s\n' "$TERM" "$(printenv COLORTERM)" "$(printenv LANG)" "$(printenv LC_ALL)"; infocmp "$TERM" >/dev/null 2>&1 && echo ti-ok; locale 2>&1 | grep -c 'Cannot set'""")
    status, note = compare(values[0][0], values[1][0])
    identity = []
    for index, value in enumerate(values):
        first = next((line for line in norm(value[0], index == 1).splitlines() if "|" in line), "")
        identity.append(first.split("|", 3))
    if len(identity[0]) >= 2 and len(identity[1]) >= 2 and identity[0][:2] != identity[1][:2]:
        status, note = "FAIL", "TERM or COLORTERM differs"
    elif len(identity[0]) >= 3 and len(identity[1]) >= 3 and identity[0][2] != identity[1][2] and status == "PASS":
        status, note = "WARN", "LANG differs"
    if any("ti-ok" not in norm(v[0], index == 1) or re.search(r"(?m)^[1-9]\d*$", norm(v[0], index == 1)) for index, v in enumerate(values)):
        status, note = "FAIL", "terminfo or locale warning"
    result("P02", status, local_ms, remote_ms, note)


def case_p03():
    first, local_a, remote_a = command_case("P03A", "stty size")
    resize(LOCAL, 30, 100)
    resize(REMOTE, 30, 100)
    second, local_b, remote_b = command_case("P03B", "stty size")
    left = first[0][0] + second[0][0]
    right = first[1][0] + second[1][0]
    status, note = compare(left, right)
    if "40 120" not in norm(left) or "30 100" not in norm(left):
        status, note = "FAIL", "expected 40 120 then 30 100"
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


def case_p10():
    for session in (LOCAL, REMOTE):
        send(session, "echo one\necho two", enter=False, paste=True)
    time.sleep(0.5)
    held = [snap(s).get("text", "") for s in (LOCAL, REMOTE)]
    for session in (LOCAL, REMOTE):
        key(session, "enter")
    time.sleep(0.8)
    after = [snap(s).get("text", "") for s in (LOCAL, REMOTE)]
    status = "PASS" if norm(held[0]) == norm(held[1], True) else "FAIL"
    note_sides(norm(held[0]) + "\n--after--\n" + norm(after[0]), norm(held[1], True) + "\n--after--\n" + norm(after[1], True))
    if any(token not in norm(value) for value in held for token in ("echo one", "echo two")):
        status = "FAIL"
    if "one" not in norm(after[0]) or "two" not in norm(after[1], True):
        status = "FAIL"
    result("P10", status, note="" if status == "PASS" else "bracketed paste differs")


def common_program():
    values, _, _ = command_case("P11C", "command -v less || command -v vi || command -v top")
    found = []
    for index, value in enumerate(values):
        found.append(next((p for p in ("less", "vi", "top") if re.search(rf"(?m)^{p}$", norm(value[0], index == 1))), None))
    return found[0] if found[0] and found[0] == found[1] else None


def full_command(program):
    return {"less": "seq 1 500 > /tmp/rp.$$; LESS= LESSOPEN= LESSCLOSE= less /tmp/rp.$$", "vi": "vi -u NONE", "top": "top"}[program]


def leave_fullscreen(program):
    for session in (LOCAL, REMOTE):
        key(session, "q" if program != "vi" else "esc")
    if program == "vi":
        for session in (LOCAL, REMOTE):
            send(session, ":qa!")


def case_p11():
    program = common_program()
    if not program:
        result("P11", "PASS", note="SKIP: no common less, vi or top")
        return
    for session in (LOCAL, REMOTE):
        send(session, full_command(program))
    time.sleep(1)
    screens = [snap(s) for s in (LOCAL, REMOTE)]
    leave_fullscreen(program)
    status = "PASS" if all(screen.get("alt_screen") for screen in screens) else "FAIL"
    result("P11", status, note="" if status == "PASS" else "alt_screen false")


def case_p12():
    program = common_program()
    if not program:
        result("P12", "PASS", note="SKIP: no common full-screen program")
        return
    for session in (LOCAL, REMOTE):
        send(session, full_command(program))
    time.sleep(1)
    for session in (LOCAL, REMOTE):
        for value in ("down", "down", "down", "pgdn"):
            key(session, value)
    screens = [snap(s) for s in (LOCAL, REMOTE)]
    leave_fullscreen(program)
    status = "PASS" if screens[0].get("app_cursor") == screens[1].get("app_cursor") and screens[0].get("text", "").splitlines()[:1] == screens[1].get("text", "").splitlines()[:1] else "FAIL"
    result("P12", status, note="" if status == "PASS" else "arrow/DECCKM differs")


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
    status = "PASS" if all(value[1] is not None for value in first + second) else "FAIL"
    result("P15", status, local_ms + local_ms2, remote_ms + remote_ms2, "INFO flood prompt times" if status == "PASS" else "flood prompt did not return")


def case_p16():
    payload = ("x" * 63 + "\n") * 16384
    chunks = [payload[i:i + 96 * 1024] for i in range(0, len(payload), 96 * 1024)]
    if len(chunks) != 11:
        raise RuntimeError("1 MiB payload did not split into 11 chunks")
    for session in (LOCAL, REMOTE):
        send(session, "stty -echo; cat > /tmp/rp.paste; stty echo")
    for chunk in chunks:
        for session in (LOCAL, REMOTE):
            send(session, chunk, enter=False, paste=True)
    for session in (LOCAL, REMOTE):
        key(session, "ctrl+d")
    values, local_ms, remote_ms = command_case("P16", "wc -c < /tmp/rp.paste; cksum < /tmp/rp.paste", timeout=60)
    status, note = compare(values[0][0], values[1][0])
    sides = (norm(values[0][0]), norm(values[1][0], True))
    if any("1048576" not in norm(value[0], i == 1) for i, value in enumerate(values)):
        status, note = "FAIL", "paste byte count is not 1048576"
    note_sides(*sides)
    result("P16", status, local_ms, remote_ms, note or "INFO paste times")


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
    status = "WARN" if p95[1] > 1000 else "PASS"
    note = f"INFO p50={p50[0]:.1f}/{p50[1]:.1f}ms p95={p95[0]:.1f}/{p95[1]:.1f}ms delta={p50[1]-p50[0]:.1f}ms"
    result("P20", status, int(p50[0]), int(p50[1]), note)


def case_p21():
    text = ""
    for name in ("up.log", "up.out"):
        path = TMP / name
        if path.exists():
            text += path.read_text(errors="replace")
    steps = re.findall(r"provision step done.*?(?:step[ =:]([A-Za-z0-9_.-]+)).*?(?:ms[ =:]([0-9.]+))", text, re.I)
    total = re.findall(r"\(([0-9.]+) s\)", text)
    note = "INFO startup steps " + (", ".join(f"{name}={ms}ms" for name, ms in steps) if steps else "not found")
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
        result("P23", "FAIL", note=err or out)
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
            result(case, "FAIL", note=str(exc).replace("\n", " ")[:240])


def main():
    cases = [
        ("P01", case_p01), ("P02", case_p02), ("P03", case_p03), ("P04", case_p04),
        ("P05", case_p05), ("P06", case_p06), ("P07", case_p07), ("P08", case_p08),
        ("P09", case_p09), ("P10", case_p10), ("P11", case_p11), ("P12", case_p12),
        ("P13", case_p13), ("P14", case_p14), ("P15", case_p15), ("P16", case_p16),
        ("P17", case_p17), ("P18", case_p18), ("P19", case_p19), ("P20", case_p20),
        ("P21", case_p21), ("P22", case_p22), ("P23", case_p23),
    ]
    try:
        host_values, _, _ = command_case("P00H", "uname -n")
        HOSTS[:] = [host_values[0][0].strip(), host_values[1][0].strip()]
        if not all(HOSTS):
            raise RuntimeError("uname -n did not return both hostnames")
    except Exception as exc:
        print(f"FAIL P00H: {str(exc).replace(chr(10), ' ')[:240]}", flush=True)
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
    print("FAILED CASES: " + (", ".join(failed) if failed else "none"))
    if failed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
