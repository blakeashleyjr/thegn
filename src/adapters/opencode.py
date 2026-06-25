import json

def parse_opencode_sessions(json_str: str):
    return json.loads(json_str)
