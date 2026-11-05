#!/usr/bin/env python3
"""Real embedded-host PTY conformance. Exit 77 means unavailable, never passing.

Build thegn-host first, then run:
  python3 test/plugin-apps.py --binary /path/to/thegn
Only synthetic plugin data and a fresh private profile are used. Failed runs
retain their profile, terminal transcript, and machine-readable event log.
"""
import argparse
import errno
import json
import os
from pathlib import Path
import re
import selectors
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
from queue import Empty, Queue


# STEPS_DOC:
# - emacs embedded readiness
# - reserved Ctrl+Q
# - typing/paste/submit
# - help and palette overlays
# - app-switch chords
# - resize
# - invalid frame and crash restart
# - unknown request recovery
def fixture_child(events_path):
    """One externally supervised plugin exercising the actual app wire."""
    events = open(events_path, "a", encoding="utf-8", buffering=1)
    revision = 0
    width, height = 80, 12
    saved = "ready"
    session = ""

    def note(kind, **data):
        events.write(json.dumps({"kind": kind, "pid": os.getpid(),
                                 "session": session, **data}, ensure_ascii=False) + "\n")

    def emit(value):
        print(json.dumps(value, ensure_ascii=False), flush=True)

    def view(invalid=False):
        nonlocal revision
        revision += 1
        root = {
            "kind": "weighted_split", "id": "layout", "vertical": False,
            "children": [
                {"weight": 2, "min_size": 30, "widget": {
                    "kind": "form", "id": "editor", "instance": "synthetic-form",
                    "title": "PTY APP FORM", "submit": "Save", "binding": {},
                    "fields": [{"id": "text", "label": "Text", "value": saved,
                                "multiline": True, "required": True}]}},
                {"weight": 1, "min_size": 15, "widget": {
                    "kind": "grid", "id": "grid", "width": width, "height": height,
                    "cursor": [0, 0], "cells": [{"x": 0, "y": 0,
                    "span": {"text": "G", "role": "Accent"}}]}},
                {"weight": 1, "min_size": 15, "widget": {
                    "kind": "actions", "id": "actions", "actions": [
                        {"id": "crash", "label": "Crash once", "binding": {}},
                        {"id": "invalid", "label": "Emit invalid frame", "binding": {}}]}}
            ]}
        if invalid:
            root["id"] = ""
        emit({"method": "update", "params": {"surface": "pty.workspace", "app": {
            "state_scope": "synthetic", "revision": revision,
            "status": "Synthetic PTY fixture", "root": root}}})
        note("invalid_sent" if invalid else "view", revision=revision)

    note("started")
    for line in sys.stdin:
        request = json.loads(line)
        method = request.get("method")
        if method == "activate":
            session = request.get("params", {}).get("session", "")
            note("activated")
            view()
        elif method == "deactivate":
            note("deactivated")
            break
        elif method == "on_event":
            payload = request.get("params", {}).get("payload", {})
            if payload.get("session") != session:
                note("wrong_session", payload=payload)
                if "id" in request:
                    emit({"id": request["id"], "error": {"code": "invalid", "message": "wrong session"}})
                continue
            event, data = payload.get("event"), payload.get("data", {})
            note("event", event=event, data=data, request_id=request.get("id"))
            if event == "action" and data.get("id") == "crash":
                note("crashing")
                os._exit(7)
            if event == "submit":
                saved = data["values"]["text"]
            if "id" in request:
                emit({"id": request["id"], "result": {}})
            if event == "submit":
                note("saved", value=saved)
                view()
            elif event == "resize":
                width = max(1, min(512, data["width"]))
                height = max(1, min(256, data["height"]))
            elif event == "action" and data.get("id") == "invalid":
                view(invalid=True)
    note("eof")


class Unavailable(RuntimeError):
    def __init__(self, reason, facility="PTY/terminal ownership"):
        super().__init__(reason)
        self.facility = facility


class Host:
    def __init__(self, binary, root, emacs=False):
        import fcntl
        import pty
        import termios
        self.fcntl, self.termios = fcntl, termios
        self.root = root
        self.host_log = (root / "thegn/profiles/emacs/state/thegn/logs/thegn.log" if emacs
                         else root / "state/thegn/logs/thegn.log")
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
        for name in ("config", "state", "cache", "runtime", "data", "projects", "worktrees", "thegn"):
            (root / name).mkdir(mode=0o700, parents=True, exist_ok=True)
        self.events_path = root / "events.jsonl"
        config = root / "config/config.toml"
        command = [sys.executable, "-u", str(Path(__file__).resolve()), "--fixture-child", str(self.events_path)]
        config.write_text("\n".join([
            'projects_dir = ' + json.dumps(str(root / "projects")),
            'worktrees_dir = ' + json.dumps(str(root / "worktrees")),
            'repo_roots = []', 'keymap_preset = "default"',
            '[daemon]', 'enabled = false', '[sandbox]', 'backend = "host"',
            '[apps]', 'default_tab = "plugin:pty:workspace"',
            'tab_order = ["work", "plugin:pty:workspace"]',
            '[profiles.emacs]', 'default_mode = "emacs"',
            '[[plugins]]', 'id = "pty"', 'name = "PTY conformance"',
            'version = "0.1.0"', 'api = "0.4.0"', 'mode = "resident"',
            'timeout_secs = 2',
            'trusted_native = true', 'background = false',
            'cwd = ' + json.dumps(str(root / "projects")),
            'command = ' + json.dumps(command), 'capabilities = ["surface:app"]',
            '[[plugins.contributions]]', 'id = "workspace"',
            'extension_point = "AppTab"', 'label = "PTY APP"',
            'surface = "pty.workspace"', '']), encoding="utf-8")
        config.chmod(0o600)
        try:
            self.master, self.slave = pty.openpty()
        except OSError as error:
            raise Unavailable(f"pty.openpty: {error}") from error
        self.before = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
        env = dict(os.environ)
        env.pop("THEGN_PROFILE", None)
        for key in list(env):
            if key.startswith("GIT_") or key == "THEGN_BENCH_FIRST_FRAME_EXIT":
                env.pop(key, None)
        env.update({"XDG_CONFIG_HOME": str(root / "config"), "XDG_STATE_HOME": str(root / "state"),
                    "XDG_CACHE_HOME": str(root / "cache"), "XDG_RUNTIME_DIR": str(root / "runtime"),
                    "XDG_DATA_HOME": str(root / "data"), "THEGN_DIR": str(root / "thegn"),
                    "THEGN_SKIP_ONBOARDING": "1", "TERM": "xterm-256color", "LANG": "C.UTF-8",
                    "LC_ALL": "C.UTF-8", "SHELL": "/bin/sh"})
        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)
        argv = [str(binary), "--config", str(config)]
        if emacs:
            argv += ["--profile", "emacs"]
        self.child = subprocess.Popen(argv, stdin=self.slave, stdout=self.slave, stderr=self.slave,
                                      cwd=root / "projects", env=env, preexec_fn=controlling_terminal,
                                      close_fds=True)
        os.set_blocking(self.master, False)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.master, selectors.EVENT_READ)
        self.output = bytearray()
        self.transcript = open(root / "terminal.bin", "wb", buffering=0)
        self.finished = False

    def drain(self, timeout=0.1):
        for _key, _mask in self.selector.select(timeout):
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno in (errno.EAGAIN, errno.EIO):
                    continue
                raise
            if not data:
                continue
            self.transcript.write(data)
            self.output.extend(data)
            if len(self.output) > 4 * 1024 * 1024:
                raise AssertionError("terminal output exceeded the synthetic-test budget")
            # Answer terminal capability queries through the same PTY as a real
            # terminal. These are protocol replies, not driven user actions.
            for query, reply in ((b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?1;2c"),
                                 (b"\x1b[>c", b"\x1b[>0;136;0c")):
                if query in data:
                    os.write(self.master, reply)

    def records(self):
        if not self.events_path.exists():
            return []
        raw = self.events_path.read_bytes()
        if len(raw) > 8 * 1024 * 1024:
            raise AssertionError("fixture log exceeded its budget")
        # A producer can be midway through a write while the host is polled.
        complete = raw[:raw.rfind(b"\n") + 1]
        return [json.loads(line) for line in complete.splitlines() if line.strip()]

    def wait(self, predicate, description, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain(min(0.1, max(0, deadline - time.monotonic())))
            records = self.records()
            if predicate(records):
                return records
            if self.child.poll() is not None:
                text = bytes(self.output).decode("utf-8", "replace")
                restricted = ("Operation not permitted" in text or "Permission denied" in text)
                if restricted and any(word in text.lower() for word in ("terminal", "pty", "waker", "signal", "socket")):
                    raise Unavailable(f"host terminal facility refused: {text[-2000:]}")
                raise AssertionError(f"host exited {self.child.returncode} waiting for {description}: {text[-2000:]}")
        log = self.host_log
        if log.exists():
            for line in reversed(log.read_text(encoding="utf-8", errors="replace").splitlines()):
                if "wake" in line.lower() and "failed" in line.lower() and any(
                        marker in line for marker in ("Operation not permitted", "Permission denied")):
                    raise Unavailable(
                        f"TerminalWaker.wake() is denied while waiting for {description}; {line}",
                        facility="TerminalWaker.wake()")
        raise AssertionError(f"timed out waiting for {description}; transcript: {self.root / 'terminal.bin'}")

    def send(self, data):
        os.write(self.master, data if isinstance(data, bytes) else data.encode("utf-8"))

    def screen(self, token, after=0):
        # The renderer batches a label at one style; strip terminal positioning
        # and styles when looking for newly painted readiness/overlay labels.
        value = bytes(self.output[after:])
        value = re.sub(rb"\x1b\][^\x07]*(?:\x07|\x1b\\)", b"", value)
        value = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", value)
        return token.encode() in value

    def resize(self, cols, rows):
        self.fcntl.ioctl(self.slave, self.termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.killpg(self.child.pid, signal.SIGWINCH)

    def finish(self, quit_keys=b"\x11"):
        self.send(quit_keys)
        self.wait(lambda _: self.child.poll() is not None, "host shutdown", timeout=12)
        if self.child.returncode != 0:
            raise AssertionError(f"host exited {self.child.returncode}")
        after = self.termios.tcgetattr(self.slave)
        if self.before != after:
            raise AssertionError("host failed to restore terminal attributes")
        pids = {record["pid"] for record in self.records() if record.get("kind") == "started"}
        for pid in pids:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                continue
            raise AssertionError(f"owned fixture child {pid} was not reaped")
        self.finished = True

    def close(self):
        if self.child.poll() is None:
            os.killpg(self.child.pid, signal.SIGKILL)
            self.child.wait(timeout=5)
        self.selector.close()
        self.transcript.close()
        os.close(self.master)
        os.close(self.slave)


def run_case(binary, root, emacs, steps, quit_only=False):
    host = Host(binary, root, emacs)
    def passed(name):
        steps.append({"name": name, "status": "passed"})
    def changed(records, value):
        return any(r.get("event") == "change" and r.get("data", {}).get("values", {}).get("text") == value for r in records)
    try:
        host.wait(lambda r: any(v.get("event") == "visibility" and v.get("data", {}).get("visible") for v in r)
                  and any(v.get("event") == "resize" and v.get("data", {}).get("width", 0) > 0 for v in r)
                  and host.screen("PTY APP FORM"), "visible embedded app and initial viewport")
        passed("emacs embedded readiness" if emacs else "embedded readiness")
        if emacs:
            host.finish(b"\x18\x03")
            passed("Emacs Ctrl+x Ctrl+c quits; terminal restored; owned children reaped")
            return
        if quit_only:
            host.finish()
            passed("reserved Ctrl+Q quits; terminal restored; owned children reaped")
            return
        host.send(b"\x01alpha")
        host.wait(lambda r: changed(r, "alpha"), "host-rendered editor typing")
        pasted = " βeta\nline 👩🏽‍💻"
        host.send(b"\x1b[200~" + pasted.encode("utf-8") + b"\x1b[201~")
        expected = "alpha" + pasted
        host.wait(lambda r: changed(r, expected), "Unicode multiline bracketed paste")
        host.send(b"\x1b[13;5u")
        host.wait(lambda r: any(v.get("kind") == "saved" and v.get("value") == expected for v in r), "Ctrl+Enter local acknowledgment")
        passed("typing, Unicode multiline paste, and acknowledged submit")
        for key, label, suffix in ((b"\x1bOP", "help", "x"), (b"\x00", "jump", "y")):
            mark = len(host.output)
            host.send(key)
            host.wait(lambda _: host.screen(label, mark), f"reserved {label} overlay")
            host.send(b"\x1b[200~DO_NOT_LEAK\x1b[201~")
            mark = len(host.output)
            host.send(b"\x1b")
            host.wait(lambda _: len(host.output) > mark, f"dismiss {label} overlay")
            host.send(suffix)
            expected += suffix
            host.wait(lambda r: changed(r, expected), f"{label} owns paste, editor resumes")
        passed("help and palette remain reserved; modal paste never reaches plugin")
        mark = len(host.records())
        host.send(b"\x1b1")
        host.wait(lambda r: any(v.get("event") == "visibility" and v.get("data", {}).get("visible") is False for v in r[mark:]), "Alt+1 hides plugin")
        mark = len(host.records())
        host.send(b"\x1b2")
        host.wait(lambda r: any(v.get("event") == "visibility" and v.get("data", {}).get("visible") is True for v in r[mark:]), "Alt+2 restores plugin")
        passed("exact app-switch chords and visibility lifecycle")
        mark = len(host.records())
        host.resize(100, 30)
        host.wait(lambda r: any(v.get("event") == "resize" and 0 < v.get("data", {}).get("width", 0) < 80 for v in r[mark:]), "actual grid viewport after terminal resize")
        passed("actual nested grid viewport after resize")
        host.send(b"\t")  # editor -> grid
        host.send("g")
        host.wait(lambda r: any(v.get("event") == "key" and v.get("data", {}).get("widget") == "grid" for v in r), "targeted grid key")
        host.send(b"\t\x1b[B\r")  # grid -> actions, second action = invalid view
        host.wait(lambda r: any(v.get("kind") == "invalid_sent" for v in r), "invalid view rejection")
        host.send(b"\x1b[A\r")  # first action = crash
        host.wait(lambda r: len([v for v in r if v.get("kind") == "activated"]) >= 2, "supervised restart", timeout=12)
        sessions = {r["session"] for r in host.records() if r.get("kind") == "activated"}
        if len(sessions) < 2:
            raise AssertionError("restart reused a durable session token")
        passed("invalid frame preserves app; crash restarts with fresh session")
        mark = len(host.output)
        host.send(b"\x1bOR")  # F3: inspect the unacknowledged crash action.
        host.wait(lambda _: host.screen("Request recovery", mark)
                  and host.screen("Record 1/1", mark), "captured unknown request recovery")
        mark = len(host.output)
        host.send(b"\x1b[3;5~")  # Ctrl+Delete explicitly discards local recovery.
        host.wait(lambda _: host.screen("No requests have an unknown outcome.", mark),
                  "explicit unknown request discard")
        if sum(row.get("kind") == "crashing" for row in host.records()) != 1:
            raise AssertionError("unknown crash action was automatically replayed")
        host.send(b"\x1b")
        passed("crash action retained without replay; F3 inspection and explicit discard")
        host.finish()
        passed("reserved Ctrl+Q quits; terminal restored; owned children reaped")
    finally:
        host.close()


def selftest():
    """Exercise the fixture protocol without the host binary or a PTY."""
    with tempfile.TemporaryDirectory(prefix="tg-plugin-selftest-") as directory:
        root = Path(directory)
        events_path = root / "events.jsonl"
        proc = subprocess.Popen(
            [sys.executable, "-u", __file__, "--fixture-child", str(events_path)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
        )
        replies = Queue()

        def read_stdout():
            for line in proc.stdout:
                replies.put(line)
            replies.put(None)

        reader = threading.Thread(target=read_stdout, daemon=True)
        reader.start()

        def send(request):
            proc.stdin.write(json.dumps(request) + "\n")
            proc.stdin.flush()

        def receive():
            try:
                line = replies.get(timeout=10)
            except Empty as error:
                raise AssertionError("fixture child produced no protocol reply within 10 seconds") from error
            if line is None:
                raise AssertionError("fixture child closed stdout before the expected protocol reply")
            value = json.loads(line)
            if not isinstance(value, dict):
                raise AssertionError("fixture child protocol reply was not a JSON object")
            return value

        try:
            send({"method": "activate", "params": {"session": "s1"}})
            update = receive()
            app = update.get("params", {}).get("app", {})
            tree = app.get("root", {})
            if (update.get("method") != "update"
                    or update.get("params", {}).get("surface") != "pty.workspace"
                    or app.get("revision") != 1
                    or tree.get("kind") != "weighted_split"
                    or len(tree.get("children", [])) != 3):
                raise AssertionError("activate did not produce revision 1 weighted-split app update")

            send({"id": 1, "method": "on_event", "params": {"payload": {
                "session": "s1", "event": "submit", "data": {"values": {"text": "hello"}},
            }}})
            if receive() != {"id": 1, "result": {}}:
                raise AssertionError("submit did not receive its empty success reply")
            update = receive()
            app = update.get("params", {}).get("app", {})
            fields = app.get("root", {}).get("children", [{}])[0].get("widget", {}).get("fields", [])
            if (update.get("method") != "update" or app.get("revision") != 2
                    or not any(field.get("id") == "text" and field.get("value") == "hello"
                               for field in fields)):
                raise AssertionError("submit did not produce revision 2 with the saved form value")

            send({"id": 2, "method": "on_event", "params": {"payload": {
                "session": "s1", "event": "action", "data": {"id": "invalid"},
            }}})
            if receive() != {"id": 2, "result": {}}:
                raise AssertionError("invalid-frame action did not receive its empty success reply")
            update = receive()
            if (update.get("method") != "update"
                    or update.get("params", {}).get("app", {}).get("root", {}).get("id") != ""):
                raise AssertionError("invalid-frame action did not emit the empty root id")

            send({"id": 3, "method": "on_event", "params": {"payload": {
                "session": "other", "event": "submit", "data": {"values": {"text": "wrong"}},
            }}})
            error = receive()
            if error.get("id") != 3 or error.get("error", {}).get("code") != "invalid":
                raise AssertionError("wrong-session event did not receive an invalid error reply")

            send({"method": "deactivate"})
            proc.stdin.close()
            if proc.wait(timeout=10) != 0:
                raise AssertionError(f"fixture child exited with status {proc.returncode}")
            reader.join(timeout=10)

            records = [json.loads(line) for line in events_path.read_text(encoding="utf-8").splitlines()]
            kinds = []
            for record in records:
                kind = record.get("kind")
                if kind not in kinds:
                    kinds.append(kind)
            expected = ["started", "activated", "view", "saved", "invalid_sent",
                        "wrong_session", "deactivated", "eof"]
            observed = [kind for kind in kinds if kind in expected]
            if observed != expected:
                raise AssertionError(f"fixture event order was {observed!r}, expected {expected!r}")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/thegn"))
    parser.add_argument("--output", type=Path)
    parser.add_argument("--test-dir", type=Path)
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args()
    if args.selftest:
        try:
            selftest()
        except (AssertionError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
            print(json.dumps({"status": "failed", "reason": str(error)}))
            return 1
        print(json.dumps({"status": "selftest-passed"}))
        return 0
    binary = args.binary.resolve()
    result = {"status": "failed", "steps": []}
    code = 1
    if not binary.is_file():
        result.update(status="blocked", reason=f"host binary missing: {binary}")
        code = 2
    elif os.name != "posix":
        result.update(status="unavailable", facility="POSIX controlling PTY", reason="run on a Unix desktop or CI PTY worker")
        code = 77
    else:
        root = args.test_dir.resolve() if args.test_dir else Path(tempfile.mkdtemp(prefix="tg-app-pty-"))
        root.mkdir(parents=True, mode=0o700, exist_ok=True)
        root.chmod(0o700)
        result["artifacts"] = str(root)
        try:
            run_case(binary, root / "emacs", True, result["steps"])
            run_case(binary, root / "quit", False, result["steps"], quit_only=True)
            run_case(binary, root / "normal", False, result["steps"])
            result["status"], code = "passed", 0
        except Unavailable as error:
            result.update(status="unavailable", facility=error.facility, reason=str(error))
            code = 77
        except (AssertionError, OSError, subprocess.SubprocessError) as error:
            result["reason"] = str(error)
    encoded = json.dumps(result, indent=2, ensure_ascii=False)
    print(encoded)
    if args.output:
        args.output.write_text(encoded + "\n", encoding="utf-8")
    return code


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture-child":
        fixture_child(sys.argv[2])
    else:
        raise SystemExit(main())
