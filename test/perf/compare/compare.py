#!/usr/bin/env python3
"""tmux / zellij / thegn comparison harness (THE-2661).

Same machine, same PTY wrapper, same pane "shell" for every tool. Measures:
  (a) startup: spawn -> pane marker visible in the client PTY (tmux, zellij);
      thegn uses THEGN_BENCH_FIRST_FRAME_EXIT (spawn -> exit after first frame).
  (b) throughput: `cat` of a 100 MiB file and `yes | head -n 5e7` in one pane,
      wall time from releasing the pane (after READY is seen) to the DONE marker
      as the client sees it.
  (c) idle CPU + RSS of the multiplexer processes (client + server [+ thegn
      daemon]) with 1 and 8 panes after a settle.

A missing tool, a version that differs from pins.json, an unreachable marker or a
wrong pane count is a hard failure: the harness never skips silently.
"""

import argparse
import fcntl
import json
import os
import platform
import re
import select
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import time
import pty

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
PANE_SHELL = os.path.join(HERE, "pane-shell.sh")
ROWS, COLS = 40, 120
LINUX = sys.platform.startswith("linux")
ANSI = re.compile(
    r"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b[\]P_^X].*?(?:\x07|\x1b\\)|\x1b[@-Z\\-_]|[\x00-\x08\x0b-\x1f]",
    re.S,
)


class Fail(Exception):
    pass


def log(msg):
    print(msg, file=sys.stderr, flush=True)


class Pty:
    """A child process on a fresh PTY with a pump that strips escapes."""

    def __init__(self, argv, env, cwd):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

        def ctty():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.proc = subprocess.Popen(
            argv, stdin=slave, stdout=slave, stderr=slave, env=env, cwd=cwd, preexec_fn=ctty
        )
        os.close(slave)
        self.fd = master
        self.raw = b""
        self.t0 = time.monotonic()

    def _read(self, timeout):
        r, _, _ = select.select([self.fd], [], [], timeout)
        if not r:
            return None
        try:
            chunk = os.read(self.fd, 1 << 16)
        except OSError:
            return b""
        return chunk

    def wait_for(self, token, timeout):
        """Return the monotonic time at which `token` first shows in the output."""
        deadline = time.monotonic() + timeout
        tok = token.encode()
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise Fail(f"marker {token!r} not seen within {timeout:.0f}s; tail={ANSI.sub('', self.raw.decode('utf-8', 'replace'))[-200:]!r}")
            chunk = self._read(min(left, 0.5))
            if chunk is None:
                continue
            if chunk == b"":
                if self.proc.poll() is not None:
                    raise Fail(f"client exited ({self.proc.returncode}) before {token!r}")
                continue
            now = time.monotonic()
            self.raw += chunk
            text = ANSI.sub("", self.raw.decode("utf-8", "replace")).encode()
            if tok in text:
                self.raw = b""
                return now
            self.raw = self.raw[-8192:]

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self._read(min(0.25, max(0.0, end - time.monotonic())))

    def send(self, data):
        os.write(self.fd, data)

    def close(self):
        try:
            if self.proc.poll() is None:
                self.proc.terminate()
                try:
                    self.proc.wait(3)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait()
        finally:
            try:
                os.close(self.fd)
            except OSError:
                pass


def ps_rows():
    out = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,args="], capture_output=True, text=True, check=True
    ).stdout
    rows = []
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) >= 2:
            rows.append((int(parts[0]), int(parts[1]), parts[2] if len(parts) > 2 else ""))
    return rows


CLK = os.sysconf("SC_CLK_TCK") if LINUX else 100


def cpu_seconds(pid):
    if LINUX:
        with open(f"/proc/{pid}/stat") as f:
            s = f.read()
        rest = s[s.rindex(")") + 2 :].split()
        return (int(rest[11]) + int(rest[12])) / CLK
    out = subprocess.run(
        ["ps", "-o", "cputime=", "-p", str(pid)], capture_output=True, text=True
    ).stdout.strip()
    if not out:
        raise FileNotFoundError(pid)
    days = 0
    if "-" in out:
        d, out = out.split("-", 1)
        days = int(d)
    secs = 0.0
    for part in out.split(":"):
        secs = secs * 60 + float(part)
    return days * 86400 + secs


def rss_kb(pid):
    out = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True
    ).stdout.strip()
    if not out:
        raise FileNotFoundError(pid)
    return int(out)


class Session:
    """One running instance of a tool in an isolated environment."""

    def __init__(self, tool, args, extra_env=None, cmd=""):
        self.tool = tool
        self.args = args
        self.root = tempfile.mkdtemp(prefix="tg-cmp.")
        self.uniq = f"{os.getpid() % 100000}{int(time.time()) % 100000}"
        self.sleep = f"86400.{self.uniq}"
        self.sock = f"cmp{self.uniq}"
        r = self.root
        env = {k: v for k, v in os.environ.items() if not k.startswith(("THEGN_", "TMUX", "ZELLIJ"))}
        for d in ("home", "config", "data", "state", "cache", "run", "tmux", "zellij"):
            os.makedirs(os.path.join(r, d), exist_ok=True)
        os.chmod(os.path.join(r, "run"), 0o700)
        env.update(
            HOME=os.path.join(r, "home"),
            XDG_CONFIG_HOME=os.path.join(r, "config"),
            XDG_DATA_HOME=os.path.join(r, "data"),
            XDG_STATE_HOME=os.path.join(r, "state"),
            XDG_CACHE_HOME=os.path.join(r, "cache"),
            XDG_RUNTIME_DIR=os.path.join(r, "run"),
            TMUX_TMPDIR=os.path.join(r, "tmux"),
            ZELLIJ_SOCKET_DIR=os.path.join(r, "zellij"),
            GIT_CONFIG_GLOBAL=os.path.join(r, "gitconfig"),
            GIT_CONFIG_SYSTEM="/dev/null",
            TERM="xterm-256color",
            LANG="C.UTF-8",
            SHELL=PANE_SHELL,
            TG_CMP_READY=f"TGREADY{self.uniq}",
            TG_CMP_DONE=f"TGDONE{self.uniq}",
            TG_CMP_SLEEP=self.sleep,
            TG_CMP_CMD=cmd,
            TG_CMP_GO=os.path.join(r, "go"),
        )
        env.update(extra_env or {})
        self.env = env
        self.cwd = os.path.join(r, "repo")
        os.makedirs(self.cwd)
        g = ["git", "-c", "commit.gpgsign=false", "-c", "user.name=c", "-c", "user.email=c@c"]
        subprocess.run(["git", "init", "-q", "-b", "main", self.cwd], env=env, check=True)
        open(os.path.join(self.cwd, "f.txt"), "w").write("x\n")
        subprocess.run(g + ["-C", self.cwd, "add", "-A"], env=env, check=True)
        subprocess.run(g + ["-C", self.cwd, "commit", "-q", "-m", "seed"], env=env, check=True)
        self.client = None

    def start(self):
        self.client = Pty(self.args, self.env, self.cwd)
        return self.client

    @property
    def ready(self):
        return self.env["TG_CMP_READY"]

    @property
    def done(self):
        return self.env["TG_CMP_DONE"]

    def pane_count(self):
        return sum(1 for _, _, a in ps_rows() if a.strip() == f"sleep {self.sleep}")

    def open_panes(self, n):
        c = self.client
        want = n
        for _ in range(n - 1):
            if self.tool == "tmux":
                self.run(["tmux", "-L", self.sock, "split-window", "-d"])
                self.run(["tmux", "-L", self.sock, "select-layout", "tiled"])
            elif self.tool == "zellij":
                self.run(["zellij", "--session", self.sock, "action", "new-pane"])
            else:
                c.send(b"\x1bp")
            c.pump(0.4)
        deadline = time.monotonic() + 20
        while self.pane_count() != want:
            if time.monotonic() > deadline:
                raise Fail(f"{self.tool}: expected {want} panes, found {self.pane_count()}")
            c.pump(0.3)

    def run(self, argv):
        r = subprocess.run(argv, env=self.env, cwd=self.cwd, capture_output=True, text=True)
        if r.returncode:
            raise Fail(f"{argv[:4]} failed: {r.stderr.strip()}")

    def mux_pids(self):
        """Client + server (+ thegn daemon) pids; never the pane workloads."""
        pids = {self.client.proc.pid}
        if self.tool == "tmux":
            out = subprocess.run(
                ["tmux", "-L", self.sock, "display-message", "-p", "#{pid}"],
                env=self.env, capture_output=True, text=True,
            ).stdout.strip()
            if not out.isdigit():
                raise Fail("cannot resolve tmux server pid")
            pids.add(int(out))
        elif self.tool == "zellij":
            found = [p for p, _, a in ps_rows() if "--server" in a and self.root in a]
            if not found:
                raise Fail("cannot resolve zellij server pid")
            pids.update(found)
        else:
            pids.update(
                p for p, _, a in ps_rows()
                if self.root in a and re.search(r"\bdaemon\b", a) and p != os.getpid()
            )
        return sorted(pids)

    def teardown(self):
        try:
            if self.tool == "tmux":
                subprocess.run(["tmux", "-L", self.sock, "kill-server"], env=self.env,
                               capture_output=True, timeout=10)
            elif self.tool == "zellij":
                subprocess.run(["zellij", "kill-all-sessions", "-y"], env=self.env,
                               capture_output=True, timeout=10)
        except (subprocess.TimeoutExpired, OSError):
            pass
        if self.client:
            self.client.close()
        for p, _, a in ps_rows():
            if self.root in a and p != os.getpid():
                try:
                    os.kill(p, signal.SIGTERM)
                except OSError:
                    pass
        shutil.rmtree(self.root, ignore_errors=True)


def tool_argv(tool, cfg):
    if tool == "tmux":
        return lambda s: ["tmux", "-L", s.sock, "-f", "/dev/null", "new-session", "-s", "cmp"]
    if tool == "zellij":
        def argv(s):
            conf = os.path.join(s.root, "zellij.kdl")
            with open(conf, "w") as f:
                f.write("show_startup_tips false\nshow_release_notes false\n")
            return ["zellij", "--config", conf, "--session", s.sock]
        return argv
    return lambda s: [cfg.bin]


def make_session(tool, cfg, cmd="", first_frame_exit=False):
    extra = {}
    if tool == "thegn":
        extra["THEGN_NO_MIGRATE"] = "1"
        if first_frame_exit:
            extra["THEGN_BENCH_FIRST_FRAME_EXIT"] = "1"
    s = Session(tool, [], extra, cmd)
    s.args = tool_argv(tool, cfg)(s)
    return s


def bench_startup(tool, cfg):
    times = []
    for i in range(cfg.warmup + cfg.runs):
        s = make_session(tool, cfg, first_frame_exit=(tool == "thegn"))
        try:
            c = s.start()
            if tool == "thegn":
                deadline = time.monotonic() + 60
                while c.proc.poll() is None:
                    c.pump(0.05)
                    if time.monotonic() > deadline:
                        raise Fail("thegn did not exit after first frame within 60s")
                if c.proc.returncode != 0:
                    raise Fail(f"thegn first-frame exit status {c.proc.returncode}")
                t = time.monotonic() - c.t0
            else:
                t = c.wait_for(s.ready, 30) - c.t0
        finally:
            s.teardown()
        if i >= cfg.warmup:
            times.append(t * 1000)
    return statistics.median(times), min(times)


def bench_throughput(tool, cfg, cmd):
    s = make_session(tool, cfg, cmd=cmd)
    try:
        c = s.start()
        c.wait_for(s.ready, 60)
        t_go = time.monotonic()
        open(s.env["TG_CMP_GO"], "w").close()
        t_done = c.wait_for(s.done, cfg.workload_timeout)
        return t_done - t_go
    finally:
        s.teardown()


def bench_idle(tool, cfg, panes):
    s = make_session(tool, cfg)
    try:
        c = s.start()
        c.wait_for(s.ready, 60)
        s.open_panes(panes)
        c.pump(cfg.settle)
        pids = s.mux_pids()
        c0 = {p: cpu_seconds(p) for p in pids}
        w0 = time.monotonic()
        c.pump(cfg.window)
        elapsed = time.monotonic() - w0
        cpu = sum(cpu_seconds(p) - c0[p] for p in pids)
        rss = sum(rss_kb(p) for p in pids) / 1024.0
        if s.pane_count() != panes:
            raise Fail(f"{tool}: pane count drifted to {s.pane_count()} (want {panes})")
        return 100.0 * cpu / elapsed, rss, pids
    finally:
        s.teardown()


def version_of(tool, cfg):
    if tool == "thegn":
        argv = [cfg.bin, "--version"]
    else:
        argv = [tool, "-V" if tool == "tmux" else "--version"]
    return subprocess.run(argv, capture_output=True, text=True, check=True).stdout.strip()


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--bin", default=os.path.join(REPO, "target/release/thegn"))
    ap.add_argument("--tools", default="thegn,tmux,zellij")
    ap.add_argument("--runs", type=int, default=10)
    ap.add_argument("--warmup", type=int, default=2)
    ap.add_argument("--settle", type=float, default=60.0, help="idle settle seconds")
    ap.add_argument("--window", type=float, default=30.0, help="idle sample window seconds")
    ap.add_argument("--workload-timeout", type=float, default=600.0)
    ap.add_argument("--file-mib", type=int, default=100)
    ap.add_argument("--allow-version-drift", action="store_true")
    ap.add_argument("--out", default=os.path.join(REPO, "target/perf-compare"))
    cfg = ap.parse_args()

    tools = cfg.tools.split(",")
    pins = json.load(open(os.path.join(HERE, "pins.json")))
    versions = {}
    for t in tools:
        exe = cfg.bin if t == "thegn" else shutil.which(t)
        if not exe or not os.access(exe, os.X_OK):
            raise Fail(f"{t} not found ({exe or 'not on PATH'}); the harness does not skip tools")
        v = version_of(t, cfg)
        versions[t] = v
        if t in pins and pins[t] not in v and not cfg.allow_version_drift:
            raise Fail(f"{t} version {v!r} != pinned {pins[t]!r} (pins.json; --allow-version-drift)")

    tmp = tempfile.mkdtemp(prefix="tg-cmp-data.")
    big = os.path.join(tmp, "big.txt")
    line = ("lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor "
            "incididunt\n").encode()
    with open(big, "wb") as f:
        total = cfg.file_mib * 1024 * 1024
        chunk = line * 4096
        while total > 0:
            f.write(chunk[:total])
            total -= len(chunk)
    workloads = {
        "cat": f"cat {big}",
        "yes": "yes | head -n 50000000",
    }

    results = {}
    try:
        for t in tools:
            log(f"== {t} startup")
            med, best = bench_startup(t, cfg)
            r = {"startup_ms_median": med, "startup_ms_min": best}
            for name, cmd in workloads.items():
                log(f"== {t} {name}")
                r[f"{name}_s"] = bench_throughput(t, cfg, cmd)
            for panes in (1, 8):
                log(f"== {t} idle {panes} panes")
                cpu, rss, pids = bench_idle(t, cfg, panes)
                r[f"idle_cpu_pct_{panes}p"] = cpu
                r[f"rss_mb_{panes}p"] = rss
                r[f"pids_{panes}p"] = pids
            results[t] = r
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    sha = subprocess.run(["git", "-C", REPO, "rev-parse", "--short", "HEAD"],
                         capture_output=True, text=True).stdout.strip()
    manifest = {
        "date": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "git_sha": sha,
        "host": {"uname": " ".join(platform.uname()), "python": platform.python_version(),
                 "term": "xterm-256color", "size": f"{COLS}x{ROWS}"},
        "versions": versions,
        "pins": pins,
        "params": {k: getattr(cfg, k) for k in
                   ("runs", "warmup", "settle", "window", "file_mib", "workload_timeout")},
        "results": results,
    }
    os.makedirs(cfg.out, exist_ok=True)
    path = os.path.join(cfg.out, time.strftime("manifest-%Y%m%d-%H%M%S.json"))
    json.dump(manifest, open(path, "w"), indent=2)

    hdr = ("| tool | startup ms | cat-%dMB s | yes 5e7 s | idle %%CPU 1p | RSS MB 1p "
           "| idle %%CPU 8p | RSS MB 8p |" % cfg.file_mib)
    print(hdr)
    print("|" + "---|" * 8)
    for t, r in results.items():
        print(f"| {t} | {r['startup_ms_median']:.0f} | {r['cat_s']:.2f} | {r['yes_s']:.2f} "
              f"| {r['idle_cpu_pct_1p']:.2f} | {r['rss_mb_1p']:.1f} "
              f"| {r['idle_cpu_pct_8p']:.2f} | {r['rss_mb_8p']:.1f} |")
    print()
    for t, v in versions.items():
        print(f"- {t}: {v}")
    print(f"- thegn startup is spawn → first frame (THEGN_BENCH_FIRST_FRAME_EXIT); "
          f"tmux/zellij are spawn → pane marker visible")
    print(f"- manifest: {path}")


if __name__ == "__main__":
    try:
        main()
    except Fail as e:
        print(f"perf-compare: FAIL: {e}", file=sys.stderr)
        sys.exit(1)
