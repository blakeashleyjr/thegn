import json

def parse_pi_jsonl(lines):
    session_id = None
    usage = {"input": 0, "output": 0}
    for line in lines:
        if not line.strip(): continue
        data = json.loads(line)
        if data.get("type") == "session":
            session_id = data.get("id")
        elif data.get("type") == "turn_end":
            turn_usage = data.get("usage", {})
            usage["input"] += turn_usage.get("input", 0)
            usage["output"] += turn_usage.get("output", 0)
    return session_id, usage
