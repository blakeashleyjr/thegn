import json

def parse_codex_stream(lines):
    for line in lines:
        if line.strip():
            yield json.loads(line)
