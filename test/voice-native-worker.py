#!/usr/bin/env python3
"""Opt-in Linux native worker fixtures; uses ALSA null, never a real microphone.

Run: python3 test/voice-native-worker.py target/debug/thegn
Build the supplied binary with dev,voice-native. No downloaded model required.
This checks process/protocol/HTTP behavior, not physical routing or permissions.
"""
import base64
import http.server
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import wave


def main():
    if not sys.platform.startswith("linux"):
        raise SystemExit("ALSA null fixtures require Linux")
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="thegn-voice-worker-") as directory:
        alsa = Path(directory) / "alsa.conf"
        alsa.write_text('pcm.!default { type null hint { show on description "Dictation fixture" } }\n')
        env = dict(os.environ, ALSA_CONFIG_PATH=str(alsa))
        requests = []
        active = 0
        peak_active = 0
        lock = threading.Lock()

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                nonlocal active, peak_active
                body = self.rfile.read(int(self.headers["Content-Length"]))
                assert b'filename="dictation.wav"' in body
                assert b"audio/wav" in body
                assert self.headers["Authorization"] == "Bearer fixture-secret"
                with lock:
                    requests.append(body)
                    number = len(requests)
                    active += 1
                    peak_active = max(active, peak_active)
                payload = json.dumps({"text": f"authoritative result {number}"}).encode()
                try:
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                finally:
                    with lock:
                        active -= 1

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        serving = threading.Thread(target=server.serve_forever)
        serving.start()
        endpoint = f"http://127.0.0.1:{server.server_port}/transcribe"

        def run(cfg, control=None, delay=0):
            start = time.monotonic()
            proc = subprocess.Popen(
                [binary, "--voice-worker"], stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, env=env, start_new_session=True,
            )
            captured = [None, None]

            def read_pipe(stream, index):
                captured[index] = stream.read()

            readers = [threading.Thread(target=read_pipe, args=(stream, index))
                       for index, stream in enumerate((proc.stdout, proc.stderr))]
            for reader in readers:
                reader.start()
            try:
                proc.stdin.write(json.dumps({"cfg": cfg, "token": "fixture-secret"}) + "\n")
                proc.stdin.flush()
                if delay:
                    time.sleep(delay)
                if control == "kill":
                    os.killpg(proc.pid, signal.SIGKILL)
                elif control == "eof":
                    proc.stdin.close()
                    proc.stdin = None
                elif control:
                    proc.stdin.write(control + "\n")
                    proc.stdin.flush()
                proc.wait(timeout=8)
                for reader in readers:
                    reader.join(timeout=2)
                    assert not reader.is_alive(), "worker retained an output pipe"
                output, error = captured
                assert "fixture-secret" not in output + error
                events = [json.loads(line) for line in output.splitlines()]
                return events, time.monotonic() - start, proc.returncode
            finally:
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait(timeout=2)
                if proc.stdin is not None:
                    proc.stdin.close()
                for reader in readers:
                    reader.join(timeout=2)
                proc.stdout.close()
                proc.stderr.close()

        cfg = dict(enabled=True, kind="http", endpoint=endpoint, model="fixture",
                   language="en", max_seconds=1, timeout_seconds=2, live_preview=False)
        try:
            events, _, code = run(cfg)
            assert code == 0 and events[-1] == {"Final": "authoritative result 1"}, events
            assert len(requests) == 1 and "Stopped" in events
            assert not any(isinstance(e, dict) and "Partial" in e for e in events)
            print("final-only: exactly one authoritative full-recording HTTP request")

            requests.clear()
            events, _, code = run(dict(cfg, max_seconds=2, live_preview=True))
            assert code == 0 and len(requests) >= 2, events
            assert any(isinstance(e, dict) and "Partial" in e for e in events)
            assert events[-1] == {"Final": f"authoritative result {len(requests)}"}, events
            assert peak_active == 1
            print("live: serialized preview requests and authoritative final replacement")

            requests.clear()
            events, elapsed, code = run(dict(cfg, max_seconds=5), "stop", 0.15)
            assert code == 0 and len(requests) == 1 and elapsed < 2, (events, elapsed)
            assert events[-1] == {"Final": "authoritative result 1"}
            print("stop: capture closes and final completes within its own deadline")

            requests.clear()
            events, elapsed, code = run(dict(cfg, max_seconds=5), "eof", 0.15)
            assert code == 0 and not events and not requests and elapsed < 2
            print("cancel/parent EOF: no final, HTTP request, or surviving worker")

            events, elapsed, code = run(dict(cfg, max_seconds=5), "kill", 0.15)
            assert code == -signal.SIGKILL and not events and not requests and elapsed < 2
            print("worker crash: bounded isolated process teardown")

            events, _, code = run(dict(cfg, kind="command", command=["must-not-execute"] ))
            assert code == 0 and len(events) == 1 and "Captured" in events[0], events
            with wave.open(io.BytesIO(base64.b64decode(events[0]["Captured"]))) as audio:
                assert (audio.getnchannels(), audio.getsampwidth(), audio.getframerate()) == (1, 2, 16000)
                assert 0 < audio.getnframes() <= 16000
            print("command adapter: bounded PCM16/mono/16 kHz WAV returned to parent")

            events, _, code = run(dict(cfg, kind="embedded", model_path=str(Path(directory)/"missing.bin")))
            assert code == 0 and len(events) == 1 and "model missing" in events[0]["Failed"], events
            assert not requests
            events, elapsed, code = run(dict(cfg, enabled=False))
            assert code != 0 and not events and not requests and elapsed < 2
            print("preflight/disabled: missing model fails before capture; disabled refuses work")
        finally:
            server.shutdown()
            server.server_close()
            serving.join(timeout=2)
            assert not serving.is_alive()


if __name__ == "__main__":
    main()
