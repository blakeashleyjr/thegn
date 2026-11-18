#!/usr/bin/env python3
"""WS10-P2-04 throwaway Python echo child (stdlib only).

Reads NDJSON lines on stdin, decodes the base64 `data` field and writes the
line back with `data` re-encoded. The shape matches a `stream.data` frame:
{"stream":3,"seq":41,"data":"<base64>","ch":"out"}
"""
import base64
import json
import sys


def main() -> None:
    out = sys.stdout.buffer
    for line in sys.stdin.buffer:
        msg = json.loads(line)
        raw = base64.b64decode(msg["data"])
        msg["data"] = base64.b64encode(raw).decode("ascii")
        out.write(json.dumps(msg, separators=(",", ":")).encode("ascii") + b"\n")
        out.flush()


if __name__ == "__main__":
    main()
