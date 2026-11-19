#!/usr/bin/env python3
"""Reproduce the Sprites exec PTY line-ending behavior without dependencies."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import secrets
import socket
import ssl
import struct
import sys
import time
from urllib.parse import quote, urlencode, urlsplit


DEFAULT_API_BASE = "wss://api.sprites.dev/v1"
COMMAND = "tty && echo ok"
FRAME_LIMIT = 1 << 20
UNKNOWN_EXIT = 254
RUN_TIMEOUT = 30.0
WEBSOCKET_MAGIC = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class ProtocolError(Exception):
    """The peer did not speak the small WebSocket subset this tool needs."""


def valid_sprite_name(name: str) -> bool:
    return bool(name) and len(name) <= 63 and all(
        "a" <= char <= "z" or "0" <= char <= "9" or char == "-" for char in name
    )


def api_parts(api_base: str) -> tuple[str, str, int, str, str]:
    parsed = urlsplit(api_base)
    if parsed.scheme not in ("ws", "wss"):
        raise ValueError("--api-base must use ws:// or wss://")
    if parsed.username is not None or parsed.password is not None:
        raise ValueError("--api-base must not contain user information")
    if parsed.query or parsed.fragment:
        raise ValueError("--api-base must not contain a query or fragment")
    host = parsed.hostname
    if not host:
        raise ValueError("--api-base must contain a host")
    if parsed.scheme == "ws" and host not in ("127.0.0.1", "::1"):
        raise ValueError("ws:// is allowed only for 127.0.0.1 or ::1")
    try:
        port = parsed.port or (443 if parsed.scheme == "wss" else 80)
    except ValueError as error:
        raise ValueError("--api-base has an invalid port") from error
    host_header = parsed.netloc
    path = parsed.path.rstrip("/")
    return parsed.scheme, host, port, host_header, path


def exec_target(api_base: str, sprite: str, tty: bool) -> tuple[str, str, int, str, str]:
    scheme, host, port, host_header, base_path = api_parts(api_base)
    path = f"{base_path}/sprites/{quote(sprite, safe='-')}/exec"
    query = urlencode(
        [
            ("cmd", "sh"),
            ("cmd", "-c"),
            ("cmd", COMMAND),
            ("tty", "true" if tty else "false"),
            ("stdin", "true"),
            ("cols", "80"),
            ("rows", "24"),
        ]
    )
    return scheme, host, port, host_header, f"{path}?{query}"


def set_read_timeout(connection: socket.socket, deadline: float) -> None:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("WebSocket read timed out after 30 seconds")
    connection.settimeout(min(remaining, 5.0))


def receive_exact(connection: socket.socket, size: int, deadline: float) -> bytes:
    result = bytearray()
    while len(result) < size:
        set_read_timeout(connection, deadline)
        try:
            chunk = connection.recv(size - len(result))
        except socket.timeout:
            continue
        if not chunk:
            raise EOFError("WebSocket peer closed the connection")
        result.extend(chunk)
    return bytes(result)


def send_frame(connection: socket.socket, opcode: int, payload: bytes) -> None:
    if len(payload) > FRAME_LIMIT:
        raise ProtocolError("WebSocket payload exceeds 1 MiB")
    if opcode & 0x8 and len(payload) > 125:
        raise ProtocolError("WebSocket control payload is too large")
    length = len(payload)
    if length < 126:
        header = bytes((0x80 | opcode, 0x80 | length))
    elif length <= 0xFFFF:
        header = bytes((0x80 | opcode, 0x80 | 126)) + struct.pack(">H", length)
    else:
        header = bytes((0x80 | opcode, 0x80 | 127)) + struct.pack(">Q", length)
    mask = secrets.token_bytes(4)
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    connection.sendall(header + mask + masked)


def receive_frame(
    connection: socket.socket, deadline: float
) -> tuple[bool, int, bytes]:
    first, second = receive_exact(connection, 2, deadline)
    fin = bool(first & 0x80)
    rsv = first & 0x70
    if rsv:
        raise ProtocolError("WebSocket extensions are not supported")
    opcode = first & 0x0F
    masked = bool(second & 0x80)
    length = second & 0x7F
    if length == 126:
        length = struct.unpack(">H", receive_exact(connection, 2, deadline))[0]
    elif length == 127:
        length = struct.unpack(">Q", receive_exact(connection, 8, deadline))[0]
    if length > FRAME_LIMIT:
        raise ProtocolError("WebSocket frame exceeds 1 MiB")
    mask = receive_exact(connection, 4, deadline) if masked else None
    payload = receive_exact(connection, length, deadline)
    if mask is not None:
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return fin, opcode, payload


def handshake(
    connection: socket.socket,
    host_header: str,
    target: str,
    token: str,
    deadline: float,
) -> None:
    key = base64.b64encode(secrets.token_bytes(16)).decode("ascii")
    request = (
        f"GET {target} HTTP/1.1\r\n"
        f"Host: {host_header}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        f"Authorization: Bearer {token}\r\n"
        "\r\n"
    ).encode("ascii")
    connection.sendall(request)

    response = bytearray()
    while b"\r\n\r\n" not in response:
        response.extend(receive_exact(connection, 1, deadline))
        if len(response) > 64 * 1024:
            raise ProtocolError("WebSocket handshake response is too large")
    header_text = bytes(response).split(b"\r\n\r\n", 1)[0].decode("iso-8859-1")
    lines = header_text.split("\r\n")
    if not lines or len(lines[0].split()) < 2 or lines[0].split()[1] != "101":
        raise ProtocolError("WebSocket handshake was not accepted")
    headers: dict[str, str] = {}
    for line in lines[1:]:
        if ":" in line:
            name, value = line.split(":", 1)
            headers[name.strip().lower()] = value.strip()
    expected = base64.b64encode(hashlib.sha1(key.encode("ascii") + WEBSOCKET_MAGIC).digest())
    if headers.get("sec-websocket-accept") != expected.decode("ascii"):
        raise ProtocolError("WebSocket handshake has an invalid accept key")


def frame_line(number: int, kind: str, payload: bytes) -> None:
    print(f"#{number} {kind} len={len(payload)} bytes={payload[:200]!r}")


def binary_exit_code(payload: bytes) -> int:
    rest = payload[1:]
    if len(rest) == 1:
        return rest[0]
    try:
        return int(rest.decode("ascii").strip())
    except (UnicodeDecodeError, ValueError):
        return UNKNOWN_EXIT


def read_output(connection: socket.socket, tty: bool, deadline: float) -> int:
    number = 0
    message_opcode: int | None = None
    message_payload = bytearray()
    while True:
        try:
            fin, opcode, payload = receive_frame(connection, deadline)
        except (EOFError, TimeoutError):
            return UNKNOWN_EXIT
        if opcode == 0x8:
            return UNKNOWN_EXIT
        if opcode == 0x9:
            send_frame(connection, 0xA, payload)
            continue
        if opcode == 0xA:
            continue
        if opcode in (0x1, 0x2):
            if message_opcode is not None:
                raise ProtocolError("new WebSocket data frame interrupted a fragment")
            message_opcode = opcode
        elif opcode == 0x0:
            if message_opcode is None:
                raise ProtocolError("WebSocket continuation has no initial frame")
        else:
            raise ProtocolError("unsupported WebSocket opcode")

        number += 1
        kind = "text" if message_opcode == 0x1 else "binary"
        frame_line(number, kind, payload)
        message_payload.extend(payload)
        if not fin:
            continue

        complete = bytes(message_payload)
        if message_opcode == 0x1:
            try:
                value = json.loads(complete.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                value = None
            if isinstance(value, dict) and value.get("type") == "exit":
                code = value.get("exit_code", UNKNOWN_EXIT)
                return code if isinstance(code, int) and not isinstance(code, bool) else UNKNOWN_EXIT
        elif not tty and complete.startswith(b"\x03"):
            return binary_exit_code(complete)
        message_opcode = None
        message_payload.clear()


def run_once(api_base: str, sprite: str, token: str, tty: bool) -> None:
    scheme, host, port, host_header, target = exec_target(api_base, sprite, tty)
    deadline = time.monotonic() + RUN_TIMEOUT
    connection = socket.create_connection((host, port), timeout=min(10.0, RUN_TIMEOUT))
    try:
        if scheme == "wss":
            context = ssl.create_default_context()
            connection = context.wrap_socket(connection, server_hostname=host)
        handshake(connection, host_header, target, token, deadline)
        code = read_output(connection, tty, deadline)
        print(f"exit={code}")
    finally:
        try:
            connection.close()
        except OSError:
            pass


def plan(api_base: str, sprite: str) -> None:
    print("sprites-exec-repro plan (nothing runs without --run)")
    print(f"connects to: {api_base.rstrip('/')}/sprites/{sprite}/exec")
    print(f"fixed command: sh -c {COMMAND!r}")
    print("runs twice: tty=true, then tty=false")
    print("may wake the sprite and bill briefly")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sprite")
    parser.add_argument("--run", action="store_true", help="connect and run the repro")
    parser.add_argument("--token-env", default="SPRITES_TOKEN", metavar="NAME")
    parser.add_argument("--api-base", default=DEFAULT_API_BASE, metavar="URL")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if not valid_sprite_name(args.sprite):
        print("error: sprite must match ^[a-z0-9-]{1,63}$", file=sys.stderr)
        return 2
    try:
        api_parts(args.api_base)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    if not args.run:
        plan(args.api_base, args.sprite)
        return 0

    token = os.environ.get(args.token_env, "")
    if not token:
        print(f"error: environment variable {args.token_env} is empty", file=sys.stderr)
        return 2
    try:
        run_once(args.api_base, args.sprite, token, True)
        run_once(args.api_base, args.sprite, token, False)
    except (ConnectionError, OSError, ProtocolError, TimeoutError, EOFError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
