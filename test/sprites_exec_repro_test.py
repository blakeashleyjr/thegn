#!/usr/bin/env python3
"""Loopback tests for test/sprites_exec_repro.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import os
import socket
import subprocess
import sys
import threading
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("sprites_exec_repro.py")


def load_repro():
    spec = importlib.util.spec_from_file_location("sprites_exec_repro", SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load repro script")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def websocket_frame(opcode: int, payload: bytes) -> bytes:
    length = len(payload)
    if length < 126:
        return bytes((0x80 | opcode, length)) + payload
    if length <= 0xFFFF:
        return bytes((0x80 | opcode, 126)) + length.to_bytes(2, "big") + payload
    return bytes((0x80 | opcode, 127)) + length.to_bytes(8, "big") + payload


class LoopbackSprites:
    def __init__(self) -> None:
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(2)
        self.base = f"ws://127.0.0.1:{self.listener.getsockname()[1]}/v1"
        self.headers: list[dict[str, str]] = []
        self.targets: list[str] = []
        self.error: BaseException | None = None
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self) -> None:
        try:
            for _ in range(2):
                connection, _ = self.listener.accept()
                with connection:
                    request = bytearray()
                    while b"\r\n\r\n" not in request:
                        chunk = connection.recv(4096)
                        if not chunk:
                            raise AssertionError("client closed before handshake")
                        request.extend(chunk)
                    head = bytes(request).split(b"\r\n\r\n", 1)[0].decode("iso-8859-1")
                    lines = head.split("\r\n")
                    self.targets.append(lines[0].split()[1])
                    headers = {}
                    for line in lines[1:]:
                        name, value = line.split(":", 1)
                        headers[name.lower()] = value.strip()
                    self.headers.append(headers)
                    key = headers["sec-websocket-key"]
                    import hashlib
                    import base64

                    accept = base64.b64encode(
                        hashlib.sha1(
                            key.encode("ascii")
                            + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
                        ).digest()
                    ).decode("ascii")
                    response = (
                        "HTTP/1.1 101 Switching Protocols\r\n"
                        "Upgrade: websocket\r\n"
                        "Connection: Upgrade\r\n"
                        f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
                    )
                    connection.sendall(response.encode("ascii"))
                    connection.sendall(websocket_frame(2, b"/dev/pts/0"))
                    connection.sendall(websocket_frame(2, b"ok\r\n"))
                    exit_message = b'{"type":"exit","exit_code":0}'
                    connection.sendall(websocket_frame(1, exit_message))
                    connection.sendall(websocket_frame(8, b"\x03\xe8"))
        except BaseException as error:  # surfaced by wait() in the test
            self.error = error
        finally:
            self.listener.close()

    def wait(self) -> None:
        self.thread.join(5)
        if self.thread.is_alive():
            raise AssertionError("loopback server did not finish")
        if self.error is not None:
            raise self.error


class SpritesExecReproTest(unittest.TestCase):
    def test_loopback_frames_and_secret_handling(self) -> None:
        server = LoopbackSprites()
        environment = os.environ.copy()
        environment["SPRITES_TOKEN"] = "SYNTHETIC"
        completed = subprocess.run(
            [sys.executable, "-B", str(SCRIPT), "demo-sprite", "--run", "--api-base", server.base],
            capture_output=True,
            text=True,
            env=environment,
            check=False,
        )
        server.wait()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertNotIn("SYNTHETIC", completed.stdout)
        self.assertNotIn("SYNTHETIC", completed.stderr)
        expected = [
            "#1 binary len=10 bytes=b'/dev/pts/0'",
            "#2 binary len=4 bytes=b'ok\\r\\n'",
            '#3 text len=29 bytes=b\'{"type":"exit","exit_code":0}\'',
            "exit=0",
        ]
        self.assertEqual(completed.stdout.splitlines(), expected + expected)
        self.assertEqual(len(server.headers), 2)
        self.assertTrue(all(header["authorization"] == "Bearer SYNTHETIC" for header in server.headers))
        self.assertEqual(["true", "false"], [
            dict(item.split("=", 1) for item in target.split("?", 1)[1].split("&"))["tty"]
            for target in server.targets
        ])

    def test_rejects_remote_plaintext_websocket(self) -> None:
        environment = os.environ.copy()
        environment["SPRITES_TOKEN"] = "SYNTHETIC"
        completed = subprocess.run(
            [sys.executable, "-B", str(SCRIPT), "demo-sprite", "--run", "--api-base", "ws://example.com"],
            capture_output=True,
            text=True,
            env=environment,
            check=False,
        )
        self.assertEqual(completed.returncode, 2)
        self.assertNotIn("SYNTHETIC", completed.stdout + completed.stderr)

    def test_plan_opens_no_socket(self) -> None:
        repro = load_repro()
        with mock.patch.object(repro.socket, "create_connection", side_effect=AssertionError("network")):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                result = repro.main(["demo-sprite"])
        self.assertEqual(result, 0)
        self.assertIn("fixed command: sh -c 'tty && echo ok'", output.getvalue())
        self.assertIn("may wake the sprite and bill briefly", output.getvalue())

    def test_bad_sprite_name_exits_two(self) -> None:
        completed = subprocess.run(
            [sys.executable, "-B", str(SCRIPT), "Bad_Name"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 2)


if __name__ == "__main__":
    unittest.main()
