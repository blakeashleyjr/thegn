"""Exercise AgentMemory via the same stdio command declared for Claude Code."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path
from uuid import uuid4


def call(server, env, tool, arguments):
    messages = [
        {"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "thegn-memory-check", "version": "1"},
        }},
        {"method": "notifications/initialized"},
        {"id": 2, "method": "tools/call", "params": {"name": tool, "arguments": arguments}},
    ]
    request = "".join(json.dumps({"jsonrpc": "2.0", **m}) + "\n" for m in messages)
    run = subprocess.run(
        [server["command"], *server["args"]], input=request, text=True,
        capture_output=True, timeout=25, env=env, check=True,
    )
    for line in run.stdout.splitlines():
        response = json.loads(line)
        if response.get("id") == 2:
            result = response.get("result", {})
            if response.get("error") or result.get("isError"):
                raise ValueError(f"{tool} failed")
            return json.loads(result["content"][0]["text"])
    raise ValueError(f"no MCP response to {tool}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="exercise only isolated fallback persistence")
    args = parser.parse_args()
    config = Path(__file__).resolve().parents[1] / ".mcp.json"
    server = json.loads(config.read_text())["mcpServers"]["agentmemory"]
    with tempfile.TemporaryDirectory() as tmp:
        env = {**os.environ, **server.get("env", {}), "STANDALONE_PERSIST_PATH": f"{tmp}/memory.json"}
        # Fetch rejects port 1. The canary never writes to a live service.
        offline = {**env, "AGENTMEMORY_URL": "http://127.0.0.1:1", "AGENTMEMORY_FORCE_PROXY": "0"}
        marker = f"thegn-memory-check-{uuid4().hex}"
        call(server, offline, "memory_save", {"content": marker, "project": "thegn", "type": "workflow"})
        result = call(server, offline, "memory_recall", {"query": marker, "limit": 1})
        if not any(row.get("content") == marker for row in result.get("results", [])):
            raise ValueError("saved canary missing after MCP restart")
        print("Standalone save and recall across processes passed (isolated store).", flush=True)
        if not args.offline:
            # Even forced proxy mode can silently fall back after a failed request.
            # Exercise the same search endpoint as memory_recall directly.
            base = env.get("AGENTMEMORY_URL", "http://localhost:3111").rstrip("/")
            request = urllib.request.Request(
                base + "/agentmemory/search",
                data=json.dumps({"query": "delivery governance", "project": "thegn",
                                 "format": "compact", "limit": 1}).encode(),
                headers={"Content-Type": "application/json"},
            )
            if env.get("AGENTMEMORY_SECRET"):
                request.add_header("Authorization", "Bearer " + env["AGENTMEMORY_SECRET"])
            with urllib.request.urlopen(request, timeout=8) as response:
                payload = json.load(response)
            if not isinstance(payload, dict) or not isinstance(payload.get("results"), list):
                raise ValueError("shared search did not return a results list")
            print("Shared service answered a read-only search.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, IndexError, subprocess.SubprocessError) as error:
        sys.exit(f"AgentMemory check failed: {error}. Check the installed CLI and configured service with agentmemory status.")
